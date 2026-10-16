//! Owned multi-network Aave V3 fork; no remote signing or production credentials.
use async_trait::async_trait;
use gofin_domain::{
    Adapter, ApprovalAudit, AuthorizationChallenge, Authorizer, GoFinError, OperationRequest,
    Quantity, Result, Status, now,
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
fn chain_id() -> u64 {
    std::env::var("GOFIN_AAVE_FORK_CHAIN_ID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
fn weth() -> &'static str {
    match chain_id() {
        1 => "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
        8453 => "0x4200000000000000000000000000000000000006",
        42161 => "0x82af49447d8a07e3bd95bd0d56f35241523fbab1",
        _ => "unsupported",
    }
}
fn fork_config() -> Result<EthereumConfig> {
    let mut config = EthereumConfig::wallet_only(
        chain_id(),
        if chain_id() == 1 { 2 } else { 1 },
        vec![weth().into()],
    );
    config.require_finalized = false;
    config.weth = Some(weth().into());
    let profiles = gofin_ethereum::aave::reviewed_markets(chain_id());
    let requested = std::env::var("GOFIN_AAVE_FORK_POOL").ok();
    let profile = profiles
        .iter()
        .find(|p| requested.as_ref().is_none_or(|pool| pool == &p.pool))
        .ok_or_else(|| blocked("Reviewed market is absent."))?;
    if chain_id() == 1 && profile.pool == "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2" {
        config
            .snapshot_tokens
            .push("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into());
    }
    config.aave = Some(gofin_ethereum::AaveConfig {
        pool: profile.pool.clone(),
        oracle: profile.oracle.clone(),
    });
    Ok(config)
}
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
        let endpoint = get("GOFIN_AAVE_FORK_RPC_URL")?;
        let port = endpoint
            .strip_prefix("http://127.0.0.1:")
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| blocked("Only an exact loopback peer is accepted."))?;
        if endpoint != format!("http://127.0.0.1:{port}") {
            return Err(blocked("Loopback endpoint is malformed."));
        }
        let instance = get("GOFIN_AAVE_FORK_INSTANCE_ID")?;
        gofin_evm::hash(&instance)?;
        let block = get("GOFIN_AAVE_FORK_BLOCK")?
            .parse()
            .map_err(|_| blocked("Fork block is invalid."))?;
        let hash = get("GOFIN_AAVE_FORK_HASH")?;
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
            || metadata["chainId"] != chain_id()
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
        if rpc_u64(&self.raw("eth_chainId", json!([])).await?)? != chain_id() {
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
            "anvil_setBalance"
                | "anvil_setStorageAt"
                | "anvil_setNextBlockBaseFeePerGas"
                | "evm_mine"
                | "evm_setNextBlockTimestamp"
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
        if chain_id() == 8453
            && method == "eth_call"
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
        if matches!(chain_id(), 8453 | 42161) && method == "eth_maxPriorityFeePerGas" {
            // Anvil's percentile recommender feeds prior padded fixture tips
            // back into itself. Fix the modeled tip, not a production fee.
            return Ok(json!("0x3b9aca00"));
        }
        if method == "eth_sendRawTransaction" {
            if chain_id() == 42161 {
                self.mutate("anvil_setNextBlockBaseFeePerGas", json!(["0x3b9aca00"]))
                    .await?;
            }
            if self.sent.fetch_add(1, Ordering::SeqCst) >= 20 {
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
        if chain_id() == 8453 && method == "eth_getBlockByNumber" && result.is_object() {
            result["extraData"] = json!(format!("0x00{}", "00".repeat(8)));
        }
        if chain_id() == 8453 && method == "eth_getTransactionReceipt" && result.is_object() {
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
                if log["address"] == weth() && log["topics"][0] == withdrawal_topic {
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
        if chain_id() == 42161 && method == "eth_getTransactionReceipt" && result.is_object() {
            // Owned Anvil is a local EVM, not Nitro. Model only this absent
            // receipt component; execution cost remains actual Anvil cash.
            result["gasUsedForL1"] = json!("0x0");
        }
        Ok(result)
    }
}
struct ForkValuation(Arc<ForkRpc>);
#[async_trait]
impl gofin_ethereum::WalletValuation for ForkValuation {
    async fn quote_usd(
        &self,
        asset: &gofin_domain::AssetId,
        units: gofin_domain::Quantity,
    ) -> Result<(gofin_domain::Quantity, String)> {
        use gofin_evm::alloy::primitives::U256;
        self.0.guard().await?;
        if asset.network != "ethereum:1" {
            return Err(blocked("Fork valuation chain mismatch."));
        }
        let reference = if asset.reference == "native" {
            weth()
        } else {
            &asset.reference
        };
        let selector =
            hex::encode(&gofin_evm::alloy::primitives::keccak256("getAssetPrice(address)")[..4]);
        let calldata = format!("0x{selector}{:0>64}", &reference[2..]);
        let price=gofin_evm::rpc_u256(&self.0.raw("eth_call",json!([{"to":"0x54586be62e3c3580375ae3723c145253060ca0c2","data":calldata},"latest"])).await?)?;
        if price.is_zero() {
            return Err(blocked(
                "Exact asset has no public Core oracle fork valuation.",
            ));
        }
        let denominator = U256::from(10).pow(U256::from(asset.decimals)) * U256::from(100);
        let usd = (gofin_evm::u256(&units)? * price + denominator - U256::from(1)) / denominator;
        Ok((
            gofin_evm::quantity(usd),
            format!("owned-fork-core-oracle:{}", reference),
        ))
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
        if transaction.chain_id != chain_id()
            || transaction.sender != lower_address(self.key.address())
        {
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
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-owned-base-fork-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
fn input(id: &str, extra: Value) -> Value {
    let mut result = json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":chain_id(),"request_id":id,"max_fee_eth":"0.01"});
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
        if chain_id() == 42161 {
            rpc.mutate("anvil_setNextBlockBaseFeePerGas", json!(["0x3b9aca00"]))
                .await?;
        }
        rpc.mutate("evm_mine", json!([])).await?;
        record = engine.status(&record.id, true).await?;
    }
    if record.status != Status::Completed {
        let diagnostic = EthereumAdapter::new(
            fork_config()?,
            vec![WalletConnection {
                name: "synthetic-base-aave".into(),
                address: owner.clone(),
                signing_enabled: true,
            }],
            Arc::new(EvmRuntime::new(
                Arc::new(ForkRpc::from_runner()?),
                chain_id(),
                None,
            )),
        )?;
        if let Ok(proof) = diagnostic.reconcile(&record).await {
            println!(
                "SYNTHETIC_FORK_PROOF={}",
                json!({"intent":record.preview.as_ref().map(|p|&p.intent),"effects":proof.verified.as_ref().map(|v|&v.effects)})
            );
        }
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
#[ignore = "Requires owned exact-network Anvil fork; no live fallback"]
async fn deployed_aave_v3_lifecycle_restores_positions_and_allowance() -> Result<()> {
    let rpc = Arc::new(ForkRpc::from_runner()?);
    rpc.guard().await?;
    if chain_id() == 42161 {
        rpc.mutate("anvil_setNextBlockBaseFeePerGas", json!(["0x3b9aca00"]))
            .await?;
        rpc.mutate("evm_mine", json!([])).await?;
    }
    println!(
        "Owned chain {} fixture; automatic isolated approval and synthetic funds. Rollup fees/liveness are modeled only when testing a rollup.",
        chain_id()
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
    let mut adapter = EthereumAdapter::new(
        fork_config()?,
        vec![WalletConnection {
            name: "synthetic-base-aave".into(),
            address: owner.clone(),
            signing_enabled: true,
        }],
        Arc::new(EvmRuntime::new(rpc.clone(), chain_id(), Some(signer))),
    )?
    .with_isolated_oracle_clock(Arc::new(ForkClock(rpc.clone())));
    if adapter.config.aave.as_ref().unwrap().pool == "0xae05cd22df81871bc7cc2a04becfb516bfe332c8" {
        adapter = adapter.with_wallet_valuation(Arc::new(ForkValuation(rpc.clone())));
    }
    let adapter = Arc::new(adapter);
    let read = json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":chain_id(),"tokens":[weth()]});
    let horizon =
        adapter.config.aave.as_ref().unwrap().pool == "0xae05cd22df81871bc7cc2a04becfb516bfe332c8";
    let etherfi =
        adapter.config.aave.as_ref().unwrap().pool == "0x0aa97c284e98396202b6a04024f5e2c65026f3c0";
    let core_risk = chain_id() == 1
        && adapter.config.aave.as_ref().unwrap().pool
            == "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2";
    let usdc = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    let asset = if horizon || etherfi {
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
    } else {
        weth()
    };
    let mut read = read;
    read["tokens"] = json!([asset]);
    if horizon || etherfi || core_risk {
        // Synthetic token funding on the guarded owned fork only. Locate the
        // balance slot by an exact balanceOf read, restoring every wrong slot.
        // GoFin has no fork-funding API; these are fixture mutations, never live.
        let mut funded = false;
        for slot in 0u64..128 {
            let encoded = format!("{:0>64}{slot:064x}", &owner[2..]);
            let hash = format!(
                "{:#x}",
                gofin_evm::alloy::primitives::keccak256(hex::decode(encoded).unwrap())
            );
            let previous = rpc
                .raw("eth_getStorageAt", json!([usdc, hash, "latest"]))
                .await?;
            rpc.mutate(
                "anvil_setStorageAt",
                json!([usdc, hash, format!("0x{:064x}", 10_000_000u64)]),
            )
            .await?;
            let balance = adapter.read("wallet.balances", &read).await?;
            // Wallet reads may have a different envelope; direct balanceOf is
            // exact fixture validation, never an extra financial operation.
            let calldata = format!("0x70a08231{:0>64}", &owner[2..]);
            let units = gofin_evm::rpc_u256(
                &rpc.raw("eth_call", json!([{"to":usdc,"data":calldata},"latest"]))
                    .await?,
            )?;
            let _ = balance;
            if units == gofin_evm::alloy::primitives::U256::from(10_000_000) {
                funded = true;
                break;
            }
            rpc.mutate("anvil_setStorageAt", json!([usdc, hash, previous]))
                .await?;
        }
        if !funded {
            return Err(blocked(
                "Cannot fund owned Horizon fixture without changing issuer permissions.",
            ));
        }
    }
    let reserves = adapter.read("aave.reserves", &read).await?;
    let reserve = &reserves["data"][0];
    assert_eq!(reserve["a_token_revision"], if horizon { "3" } else { "5" });
    assert_eq!(
        reserve["variable_debt_revision"],
        if horizon { "1" } else { "5" }
    );
    let pool = adapter.config.aave.as_ref().unwrap().pool.clone();
    let baseline = adapter.read("aave.positions", &read).await?;
    assert_eq!(baseline["data"]["positions"][0]["supplied"], "0");
    assert_eq!(baseline["data"]["positions"][0]["variable_debt"], "0");
    let before_allowance=adapter.read("wallet.allowance",&json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":chain_id(),"token":asset,"spender":pool})).await?;
    if etherfi {
        let prepared = adapter
            .prepare(
                "aave.supply",
                &input("etherfi-cap", json!({"token":asset,"amount":"0.1"})),
            )
            .await;
        let rejection = prepared.expect_err("retiring market must reject new supply");
        assert!(
            matches!(rejection.code.as_str(), "SUPPLY_CAP" | "SUPPLY_UNAVAILABLE"),
            "{rejection:?}"
        );
        assert_eq!(rpc.sent.load(Ordering::SeqCst), 0);
        println!(
            "EtherFi retiring-market supply restriction checked against deployed contracts; no submission. Position exits remain separately unverified."
        );
        return Ok(());
    }
    let engine = Engine::new(
        Store::in_memory().map_err(|_| blocked("Temporary isolated journal is unavailable."))?,
        vec![adapter.clone()],
        Arc::new(ForkApproval(rpc.clone())),
        EngineOptions {
            max_run_allocation_usd_micros: "500000000".into(),
            ..Default::default()
        },
    )?;
    if horizon {
        // Keep the funding journal intact; a separate observation journal
        // models a first GoFin operation over an externally created position.
        let observed_engine = Engine::new(
            Store::in_memory()
                .map_err(|_| blocked("Temporary observation journal unavailable."))?,
            vec![adapter.clone()],
            Arc::new(ForkApproval(rpc.clone())),
            EngineOptions {
                max_run_allocation_usd_micros: "500000000".into(),
                ..Default::default()
            },
        )?;
        for (index, (operation, extra)) in [
            (
                "wallet.approve",
                json!({"token":asset,"spender":pool,"amount":"2"}),
            ),
            ("aave.supply", json!({"token":asset,"amount":"2"})),
            (
                "aave.withdraw",
                json!({"token":asset,"amount":"0.5","recipient":owner}),
            ),
            (
                "aave.withdraw",
                json!({"token":asset,"amount":"all","max_amount":"2.01","recipient":owner}),
            ),
            (
                "wallet.approve",
                json!({"token":asset,"spender":pool,"amount":"0"}),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            execute(
                if index < 2 { &engine } else { &observed_engine },
                &rpc,
                operation,
                input(&format!("horizon-fork-{index}"), extra),
            )
            .await?;
        }
        let after = adapter.read("aave.positions", &read).await?;
        assert_eq!(after["data"]["positions"][0]["supplied"], "0");
        assert_eq!(after["data"]["positions"][0]["variable_debt"], "0");
        let allowance=adapter.read("wallet.allowance",&json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":chain_id(),"token":asset,"spender":pool})).await?;
        assert_eq!(
            before_allowance["data"]["allowance"],
            allowance["data"]["allowance"]
        );
        println!(
            "Horizon deployed revision-3 supply, partial/full withdrawal and allowance cleanup settled. RWA collateral/debt lifecycle requires an issuer-eligible fixture and is separately unverified."
        );
        return Ok(());
    }
    if core_risk {
        execute(
            &engine,
            &rpc,
            "wallet.approve",
            input(
                "other-collateral-approval",
                json!({"token":usdc,"spender":pool,"amount":"4"}),
            ),
        )
        .await?;
        execute(
            &engine,
            &rpc,
            "aave.supply",
            input(
                "other-collateral-supply",
                json!({"token":usdc,"amount":"4"}),
            ),
        )
        .await?;
    }
    let mut observed_engine: Option<Engine> = None;
    for (index, (operation, extra)) in [
        ("wallet.wrap", json!({"amount":"0.004"})),
        (
            "wallet.approve",
            json!({"token":weth(),"spender":pool,"amount":"0.003"}),
        ),
        ("aave.supply", json!({"token":weth(),"amount":"0.003"})),
        ("aave.collateral", json!({"token":weth(),"enabled":false})),
        ("aave.collateral", json!({"token":weth(),"enabled":true})),
        (
            "aave.borrow",
            json!({"token":weth(),"amount":"0.0003","min_health_factor":"3","max_debt_usd":"20","max_debt_to_collateral_bps":2000}),
        ),
        (
            "wallet.approve",
            json!({"token":weth(),"spender":pool,"amount":"0.0004"}),
        ),
        ("aave.repay", json!({"token":weth(),"amount":"0.0001"})),
        (
            "aave.repay",
            json!({"token":weth(),"amount":"all","max_amount":"0.0004"}),
        ),
        ("aave.withdraw", json!({"token":weth(),"amount":"0.001","recipient":owner})),
        (
            "aave.withdraw",
            json!({"token":weth(),"amount":"all","max_amount":"0.0031","recipient":owner}),
        ),
        (
            "wallet.approve",
            json!({"token":weth(),"spender":pool,"amount":"0"}),
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
            observed_engine.as_ref().unwrap_or(&engine),
            &rpc,
            operation,
            input(&format!("aave-v3-fork-{index}"), extra),
        )
        .await?;
        if operation=="aave.borrow" && chain_id()==1 {
            // Preserve the original journal. A separately owned observation
            // journal starts with existing, independently verified obligations;
            // no historical principal or gross borrowing permission is invented.
            observed_engine=Some(Engine::new(Store::in_memory().map_err(|_|blocked("Observation journal unavailable."))?, vec![adapter.clone()], Arc::new(ForkApproval(rpc.clone())),EngineOptions {max_run_allocation_usd_micros:"500000000".into(),..Default::default()})?);
            println!("Switching to an independent journal over existing supply and variable debt; original funding/borrowing journal retained.");
            if core_risk {
                let current=observed_engine.as_ref().unwrap();
                execute(current,&rpc,"aave.collateral",input("disable-with-debt-other-collateral",json!({"token":weth(),"enabled":false,"min_health_factor":"3"}))).await?;
                let state=adapter.read("aave.positions",&read).await?;
                assert_eq!(state["data"]["positions"][0]["collateral_enabled"],false);
                let floor=Quantity::from_decimal("3",18)?;
                assert!(floor<=Quantity::new(state["data"]["account"]["health_factor"].as_str().unwrap())?);
                assert_ne!(state["data"]["account"]["debt_base"],"0");
                execute(current,&rpc,"aave.collateral",input("reenable-with-debt",json!({"token":weth(),"enabled":true}))).await?;
                println!("Legitimate collateral disable with remaining debt settled; independently read other-collateral health factor remains at least 3.");
            }

        }
    }
    if core_risk {
        let current = observed_engine.as_ref().unwrap_or(&engine);
        execute(
            current,
            &rpc,
            "aave.withdraw",
            input(
                "close-other-collateral",
                json!({"token":usdc,"amount":"all","max_amount":"4.01","recipient":owner}),
            ),
        )
        .await?;
        execute(
            current,
            &rpc,
            "wallet.approve",
            input(
                "restore-other-collateral-allowance",
                json!({"token":usdc,"spender":pool,"amount":"0"}),
            ),
        )
        .await?;
        let state=adapter.read("aave.positions",&json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":chain_id(),"tokens":[usdc]})).await?;
        assert_eq!(state["data"]["positions"][0]["scaled_supply"], "0");
        assert_eq!(state["data"]["positions"][0]["pool_allowance"], "0");
    }
    let after = adapter.read("aave.positions", &read).await?;
    assert_eq!(after["data"]["positions"][0]["supplied"], "0");
    assert_eq!(after["data"]["positions"][0]["variable_debt"], "0");
    let after_allowance=adapter.read("wallet.allowance",&json!({"wallet":"synthetic-base-aave","network":"evm","chain_id":chain_id(),"token":weth(),"spender":pool})).await?;
    assert_eq!(before_allowance["data"], after_allowance["data"]);
    assert!(adapter.config.uniswap.is_none());
    println!(
        "Aave V3 fork: complete lifecycle settled, duplicates prevented, supply/debt/allowance restored. Automatic isolated approval and modeled rollup fees; no biometric, authentic rollup fee or live-write evidence."
    );
    Ok(())
}
