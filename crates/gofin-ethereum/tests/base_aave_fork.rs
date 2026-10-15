//! Explicit owned Base/OP Anvil fork only; no remote signing or production credentials.
use async_trait::async_trait;
use gofin_domain::{
    Adapter, ApprovalAudit, AuthorizationChallenge, Authorizer, GoFinError, OperationRequest,
    Result, Status, now,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_ethereum::{EthereumAdapter, EthereumConfig, OracleObservationClock, WalletConnection};
use gofin_evm::{
    EnvelopeSigner, EvmRpc, EvmRuntime, FrozenTransaction, LocalEnvelopeSigner,
    alloy::primitives::Bytes, lower_address, rpc_u64,
};
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
const WETH: &str = "0x4200000000000000000000000000000000000006";
// Deterministic PUBLIC SHA256 fixture of
// "GoFin isolated Base Aave fork fixture v1 - PUBLIC DO NOT FUND".
// Never use or fund this key outside the guarded loopback fork.
const KEY: &str = "e7dc452ecea25fa05a22e0551126bee1822a2774df1c922515a11babc4f42fdd";
fn blocked(message: &str) -> GoFinError {
    GoFinError::new(
        "ISOLATED_BASE_FORK_REQUIRED",
        "fork",
        message,
        "Run scripts/verify-base-aave-fork.py; no remote fallback is allowed.",
    )
}

struct ForkRpc {
    endpoint: String,
    instance: String,
    block: u64,
    hash: String,
    client: reqwest::Client,
    sent: AtomicU64,
    gas_bound: AtomicU64,
}
impl ForkRpc {
    fn from_runner() -> Result<Self> {
        let get = |key| {
            std::env::var(key).map_err(|_| blocked("Owned Base fork runner context is absent."))
        };
        let endpoint = get("GOFIN_BASE_AAVE_FORK_RPC_URL")?;
        let port = endpoint
            .strip_prefix("http://127.0.0.1:")
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| blocked("Only an exact loopback peer is accepted."))?;
        if endpoint != format!("http://127.0.0.1:{port}") {
            return Err(blocked("Loopback endpoint is malformed."));
        }
        let instance = get("GOFIN_BASE_AAVE_FORK_INSTANCE_ID")?;
        gofin_evm::hash(&instance)?;
        let block = get("GOFIN_BASE_AAVE_FORK_BLOCK")?
            .parse()
            .map_err(|_| blocked("Fork block is invalid."))?;
        let hash = get("GOFIN_BASE_AAVE_FORK_HASH")?;
        gofin_evm::hash(&hash)?;
        Ok(Self {
            endpoint,
            instance,
            block,
            hash,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|_| blocked("Loopback-only client unavailable."))?,
            sent: AtomicU64::new(0),
            gas_bound: AtomicU64::new(21_000),
        })
    }
    async fn raw(&self, method: &str, params: Value) -> Result<Value> {
        let response = self
            .client
            .post(&self.endpoint)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| blocked("Owned Anvil unavailable."))?;
        if !response.status().is_success() {
            return Err(blocked("Owned Anvil rejected request."));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| blocked("Malformed Anvil response."))?;
        if body["jsonrpc"] != "2.0" || body["id"] != 1 || body.get("error").is_some() {
            let code = body["error"]["code"].as_i64().unwrap_or_default();
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("Malformed response");
            // This peer is the owned synthetic Anvil, never a live venue. Keep
            // diagnosis bounded and omit calldata/transaction/error-data bodies.
            return Err(blocked(&format!(
                "Required loopback {method} failed ({code}): {}",
                message.chars().take(160).collect::<String>()
            )));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| blocked("Missing Anvil result."))
    }

    async fn guard(&self) -> Result<()> {
        let metadata = self.raw("anvil_metadata", json!([])).await?;
        if metadata["instanceId"] != self.instance
            || metadata["chainId"] != 8453
            || !metadata["clientVersion"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .starts_with("anvil/")
            || metadata["forkedNetwork"]["forkBlockNumber"] != self.block
            || metadata["forkedNetwork"]["forkBlockHash"] != self.hash
        {
            return Err(blocked("Exact owned Base fork identity changed."));
        }
        if rpc_u64(&self.raw("eth_chainId", json!([])).await?)? != 8453 {
            return Err(blocked("Wrong fork chain."));
        }
        let anchor = self
            .raw(
                "eth_getBlockByNumber",
                json!([format!("{:#x}", self.block), false]),
            )
            .await?;
        if anchor["hash"] != self.hash {
            return Err(blocked("Pinned Base fork block changed."));
        }
        Ok(())
    }
    async fn mutate(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "anvil_setBalance" | "evm_mine" | "evm_setNextBlockTimestamp"
        ) {
            return Err(blocked("Unreviewed local mutation."));
        }
        self.guard().await?;
        self.raw(method, params).await
    }
}
#[async_trait]
impl EvmRpc for ForkRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "eth_chainId"
                | "eth_blockNumber"
                | "eth_getCode"
                | "eth_getBalance"
                | "eth_getTransactionCount"
                | "eth_call"
                | "eth_estimateGas"
                | "eth_maxPriorityFeePerGas"
                | "eth_getBlockByNumber"
                | "eth_getBlockByHash"
                | "eth_getTransactionByHash"
                | "eth_getTransactionReceipt"
                | "eth_sendRawTransaction"
        ) {
            return Err(blocked("Unreviewed transport method."));
        }
        self.guard().await?;
        // Anvil charges OP extras but omits the receipt component fields.
        // This fork-only model aggregates those extras as one synthetic L1
        // observation. It verifies total local cash, never real component math.
        if method == "eth_call"
            && params[0]["to"].as_str().is_some_and(|to| {
                to.eq_ignore_ascii_case("0x420000000000000000000000000000000000000f")
                    || to.eq_ignore_ascii_case("0x4200000000000000000000000000000000000015")
            })
        {
            let data = params[0]["data"].as_str().unwrap_or_default();
            let selector = |sig: &str| {
                format!(
                    "0x{}",
                    hex::encode(&gofin_evm::alloy::primitives::keccak256(sig)[..4])
                )
            };
            let value = if data.starts_with(&selector("isFjord()")) {
                gofin_evm::alloy::primitives::U256::from(1)
            } else if data.starts_with(&selector("getL1FeeUpperBound(uint256)")) {
                // Preserve current deployed oracle upper bounds for both fee
                // classes; this synthetic aggregate never lowers the reserve.
                let l1 = gofin_evm::rpc_u256(&self.raw("eth_call", params.clone()).await?)?;
                let gas =
                    gofin_evm::alloy::primitives::U256::from(self.gas_bound.load(Ordering::SeqCst));
                let operator_data = format!("{}{:064x}", selector("getOperatorFee(uint256)"), gas);
                let operator=gofin_evm::rpc_u256(&self.raw("eth_call",json!([{"to":"0x420000000000000000000000000000000000000f","data":operator_data},params[1]])).await?)?;
                l1.checked_add(operator)
                    .ok_or_else(|| blocked("Modeled total extra bound overflowed."))?
            } else {
                gofin_evm::alloy::primitives::U256::ZERO
            };
            return Ok(json!(format!("0x{value:064x}")));
        }
        if method == "eth_sendRawTransaction" {
            if self.sent.fetch_add(1, Ordering::SeqCst) >= 16 {
                return Err(blocked("Finite fork submission ceiling exhausted."));
            }
            let latest = self
                .raw("eth_getBlockByNumber", json!(["latest", false]))
                .await?;
            self.mutate(
                "evm_setNextBlockTimestamp",
                json!([rpc_u64(&latest["timestamp"])? + 1]),
            )
            .await?;
        }
        let mut result = self.raw(method, params).await?;
        if method == "eth_estimateGas" {
            let gas = rpc_u64(&result)?;
            self.gas_bound.store(
                gas.checked_mul(120)
                    .and_then(|g| g.checked_add(99))
                    .map(|g| g / 100)
                    .ok_or_else(|| blocked("Fork gas bound overflowed."))?,
                Ordering::SeqCst,
            );
        }
        if method == "eth_getBlockByNumber" && result.is_object() {
            result["extraData"] = json!(format!("0x00{}", "00".repeat(8)));
        }
        if method == "eth_getTransactionReceipt" && result.is_object() {
            let block = rpc_u64(&result["blockNumber"])?;
            let header = self
                .raw("eth_getBlockByNumber", json!([result["blockNumber"], true]))
                .await?;
            let transactions = header["transactions"]
                .as_array()
                .ok_or_else(|| blocked("Fork block transaction list missing."))?;
            if transactions.len() != 1 || transactions[0]["hash"] != result["transactionHash"] {
                return Err(blocked(
                    "Only one exact known fork transaction permits fee aggregation.",
                ));
            }
            let tx = &transactions[0];
            let owner = lower_address(LocalEnvelopeSigner::from_private_key(KEY)?.address());
            if tx["from"] != owner {
                return Err(blocked("Unexpected fork fee payer."));
            }
            let before = gofin_evm::rpc_u256(
                &self
                    .raw(
                        "eth_getBalance",
                        json!([owner, format!("{:#x}", block - 1)]),
                    )
                    .await?,
            )?;
            let after = gofin_evm::rpc_u256(
                &self
                    .raw("eth_getBalance", json!([owner, result["blockNumber"]]))
                    .await?,
            )?;
            let value = gofin_evm::rpc_u256(&tx["value"])?;
            let execution = gofin_evm::rpc_u256(&result["gasUsed"])?
                * gofin_evm::rpc_u256(&result["effectiveGasPrice"])?;
            let withdrawal_topic = format!(
                "{:#x}",
                gofin_evm::alloy::primitives::keccak256("Withdrawal(address,uint256)")
            );
            let mut inflow = gofin_evm::alloy::primitives::U256::ZERO;
            for log in result["logs"]
                .as_array()
                .ok_or_else(|| blocked("Fork receipt logs missing."))?
            {
                if log["address"] == WETH && log["topics"][0] == withdrawal_topic {
                    let owner_word = format!("0x{:0>64}", &owner[2..]);
                    if log["topics"][1] != owner_word {
                        return Err(blocked("Unknown native withdrawal recipient."));
                    }
                    inflow = inflow
                        .checked_add(gofin_evm::rpc_u256(&log["data"])?)
                        .ok_or_else(|| blocked("Native inflow overflowed."))?;
                }
            }
            let total_fee = before
                .checked_add(inflow)
                .and_then(|q| q.checked_sub(after))
                .and_then(|q| q.checked_sub(value))
                .ok_or_else(|| blocked("Fork native conservation could not be established."))?;
            let extras = total_fee
                .checked_sub(execution)
                .ok_or_else(|| blocked("Fork execution fee exceeds observed total debit."))?;
            result["l1Fee"] = json!(format!("{extras:#x}"));
            result["operatorFeeScalar"] = json!("0x0");
            result["operatorFeeConstant"] = json!("0x0");
        }
        Ok(result)
    }
}
struct ForkClock(Arc<ForkRpc>);
#[async_trait]
impl OracleObservationClock for ForkClock {
    async fn unix_seconds(&self) -> Result<u64> {
        self.0.guard().await?;
        rpc_u64(
            &self
                .0
                .raw("eth_getBlockByNumber", json!(["latest", false]))
                .await?["timestamp"],
        )
    }
}
struct ForkSigner {
    rpc: Arc<ForkRpc>,
    key: LocalEnvelopeSigner,
}
#[async_trait]
impl EnvelopeSigner for ForkSigner {
    async fn sign_prepared(&self, transaction: &FrozenTransaction) -> Result<Bytes> {
        self.rpc.guard().await?;
        if transaction.chain_id != 8453 || transaction.sender != lower_address(self.key.address()) {
            return Err(blocked("Synthetic signer scope mismatch."));
        }
        self.key.sign_prepared(transaction).await
    }
}
struct ForkApproval(Arc<ForkRpc>);
#[async_trait]
impl Authorizer for ForkApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.guard().await?;
        Ok(ApprovalAudit {
            mechanism: "synthetic-owned-base-fork-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
fn input(id: &str, extra: Value) -> Value {
    let mut result = json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":8453,"request_id":id,"max_fee_eth":"0.01"});
    result
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    result
}
async fn execute(engine: &Engine, rpc: &ForkRpc, operation: &str, input: Value) -> Result<()> {
    let request = OperationRequest {
        request_id: input["request_id"].as_str().unwrap().into(),
        operation: operation.into(),
        input,
        run_id: None,
    };
    println!("Fork executing {operation}");
    let owner = lower_address(LocalEnvelopeSigner::from_private_key(KEY)?.address());
    let before = gofin_evm::rpc_u256(&rpc.raw("eth_getBalance", json!([owner, "latest"])).await?)?;
    let mut record = engine.execute(request.clone()).await?;
    for _ in 0..3 {
        if record.status.terminal() {
            break;
        }
        rpc.mutate("evm_mine", json!([])).await?;
        record = engine.status(&record.id, true).await?;
    }
    if record.status != Status::Completed {
        return Err(record.error.unwrap_or_else(|| {
            blocked("Original Aave operation lacks settled proof; restoration is unresolved.")
        }));
    }
    // Check actual local cash against the original raw Anvil receipt and typed costs. Neither
    // the modeled fields above nor the ledger can invent an extra fee debit.
    let submission = record
        .submission
        .as_ref()
        .ok_or_else(|| blocked("Missing original fork submission."))?;
    let receipt = rpc
        .raw("eth_getTransactionReceipt", json!([submission.reference]))
        .await?;
    let execution_fee = gofin_evm::rpc_u256(&receipt["gasUsed"])?
        * gofin_evm::rpc_u256(&receipt["effectiveGasPrice"])?;
    let fee = record
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .filter_map(|effect| {
            if let gofin_domain::FinancialEffect::Cost { amount, .. } = effect
                && amount.asset.reference == "native"
            {
                return Some(gofin_evm::u256(&amount.atoms).unwrap());
            }
            None
        })
        .fold(gofin_evm::alloy::primitives::U256::ZERO, |sum, value| {
            sum + value
        });
    assert!(fee >= execution_fee);
    let after = gofin_evm::rpc_u256(&rpc.raw("eth_getBalance", json!([owner, "latest"])).await?)?;
    let binding = &record.preview.as_ref().unwrap().binding;
    let native_debit = if operation == "wallet.wrap" {
        gofin_evm::u256(
            &serde_json::from_value::<gofin_domain::Quantity>(binding["units"].clone())
                .map_err(|_| blocked("Missing wrap units."))?,
        )?
    } else {
        gofin_evm::alloy::primitives::U256::ZERO
    };
    let native_credit = if operation == "wallet.unwrap" {
        gofin_evm::u256(
            &serde_json::from_value::<gofin_domain::Quantity>(binding["units"].clone())
                .map_err(|_| blocked("Missing unwrap units."))?,
        )?
    } else {
        gofin_evm::alloy::primitives::U256::ZERO
    };
    assert_eq!(
        before + native_credit,
        after + native_debit + fee,
        "Actual Anvil balance must equal value flow plus independently observed total fee."
    );
    let sent = rpc.sent.load(Ordering::SeqCst);
    let duplicate = engine.execute(request).await?;
    assert_eq!(duplicate.id, record.id);
    assert_eq!(duplicate.status, Status::Completed);
    assert_eq!(rpc.sent.load(Ordering::SeqCst), sent);
    Ok(())
}

#[tokio::test]
#[ignore = "Requires owned OP Anvil Base fork; no live fallback"]
async fn deployed_base_aave_six_actions_restore_positions_and_allowance() -> Result<()> {
    let rpc = Arc::new(ForkRpc::from_runner()?);
    rpc.guard().await?;
    println!(
        "MODELED FEE CONDITIONS: Anvil total cash charges with synthetic aggregated OP extra-fee observations. This verifies deployed Aave contract behavior, not authentic Base fees or rollup liveness."
    );
    let key = LocalEnvelopeSigner::from_private_key(KEY)?;
    let owner = lower_address(key.address());
    let code = rpc.raw("eth_getCode", json!([owner, "latest"])).await?;
    if code != "0x" {
        return Err(blocked("Synthetic Base fork signer is not a clean EOA."));
    }
    // Funds only the guarded deterministic fork fixture. No imported keys.
    rpc.mutate("anvil_setBalance", json!([owner, "0xde0b6b3a7640000"]))
        .await?;
    let signer = Arc::new(ForkSigner {
        rpc: rpc.clone(),
        key,
    });
    let adapter = Arc::new(
        EthereumAdapter::new(
            EthereumConfig::base_aave(vec![WETH.into()]),
            vec![WalletConnection {
                name: "synthetic-base-aave".into(),
                address: owner.clone(),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(rpc.clone(), 8453, Some(signer))),
        )?
        .with_isolated_oracle_clock(Arc::new(ForkClock(rpc.clone()))),
    );
    let read =
        json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":8453,"tokens":[WETH]});
    let reserves = adapter.read("aave.reserves", &read).await?;
    let reserve = &reserves["data"][0];
    let revision_selector = |sig: &str| {
        format!(
            "0x{}",
            hex::encode(&gofin_evm::alloy::primitives::keccak256(sig)[..4])
        )
    };
    for (token, function) in [
        (reserve["a_token"].as_str().unwrap(), "ATOKEN_REVISION()"),
        (
            reserve["variable_debt_token"].as_str().unwrap(),
            "DEBT_TOKEN_REVISION()",
        ),
    ] {
        let revision = rpc
            .request(
                "eth_call",
                json!([{"to":token,"data":revision_selector(function)},format!("{:#x}",rpc.block)]),
            )
            .await?;
        println!(
            "Observed {function} at pinned Base block {}: {}",
            rpc.block,
            gofin_evm::rpc_u256(&revision)?
        );
        if gofin_evm::rpc_u256(&revision)? != gofin_evm::alloy::primitives::U256::from(5) {
            return Err(GoFinError::new(
                "BASE_AAVE_REVISION_UNSUPPORTED",
                "fork",
                "Current Base receipt/debt token does not use reviewed revision 5 backing math.",
                "Implement revision-specific proof compatibility before enabling these writes.",
            ));
        }
    }
    let pool = adapter.config.aave.as_ref().unwrap().pool.clone();
    let baseline = adapter.read("aave.positions", &read).await?;
    assert_eq!(baseline["data"]["positions"][0]["supplied"], "0");
    assert_eq!(baseline["data"]["positions"][0]["variable_debt"], "0");
    let before_allowance=adapter.read("wallet.allowance",&json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":8453,"token":WETH,"spender":pool})).await?;
    let engine = Engine::new(
        Store::in_memory().map_err(|_| blocked("Temporary isolated journal is unavailable."))?,
        vec![adapter.clone()],
        Arc::new(ForkApproval(rpc.clone())),
        EngineOptions {
            max_run_allocation_usd_micros: "500000000".into(),
            ..Default::default()
        },
    )?;
    for (index, (operation, extra)) in [
        ("wallet.wrap", json!({"amount":"0.004"})),
        (
            "wallet.approve",
            json!({"token":WETH,"spender":pool,"amount":"0.003"}),
        ),
        ("aave.supply", json!({"token":WETH,"amount":"0.003"})),
        ("aave.collateral", json!({"token":WETH,"enabled":false})),
        ("aave.collateral", json!({"token":WETH,"enabled":true})),
        (
            "aave.borrow",
            json!({"token":WETH,"amount":"0.0003","min_health_factor":"1.5"}),
        ),
        (
            "wallet.approve",
            json!({"token":WETH,"spender":pool,"amount":"0.0004"}),
        ),
        (
            "aave.repay",
            json!({"token":WETH,"amount":"all","max_amount":"0.0004"}),
        ),
        (
            "aave.withdraw",
            json!({"token":WETH,"amount":"all","max_amount":"0.0031","recipient":owner}),
        ),
        (
            "wallet.approve",
            json!({"token":WETH,"spender":pool,"amount":"0"}),
        ),
        (
            "wallet.unwrap",
            json!({"amount":"all","max_amount":"0.0041"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        execute(
            &engine,
            &rpc,
            operation,
            input(&format!("base-aave-fork-{index}"), extra),
        )
        .await?;
    }
    let after = adapter.read("aave.positions", &read).await?;
    assert_eq!(after["data"]["positions"][0]["supplied"], "0");
    assert_eq!(after["data"]["positions"][0]["variable_debt"], "0");
    let after_allowance=adapter.read("wallet.allowance",&json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":8453,"token":WETH,"spender":pool})).await?;
    assert_eq!(before_allowance["data"], after_allowance["data"]);
    assert!(adapter.config.uniswap.is_none());
    println!(
        "Base Aave fork: all six actions settled, duplicates prevented, supply/debt/allowance restored. Modeled OP fee observations and automatic isolated approval only; no authentic Base fee, rollup liveness, biometric or live-write evidence."
    );
    Ok(())
}
