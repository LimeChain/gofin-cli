//! Manual, isolated deployed-contract verification. This is never live-write,
//! credential-isolation, or biometric evidence.
//!
//! Run through `scripts/verify-rust-fork.sh`, which starts and owns a fresh Anvil
//! on an allocated loopback endpoint and passes its URL and instance ID. This
//! test starts no process, loads no files except its temporary SQLite journal,
//! and has no remote fallback.
use async_trait::async_trait;
use fin_broker::{
    AssetValuator, BrokerConfigV2, BrokerPolicy, BrokerRuntime, ConnectionConfig, DeploymentConfig,
    NetworkConfig, RuntimeDependencies, compose_runtime,
};
use fin_domain::{
    AccountingLocation, Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer,
    CostTreatment, FinError, FinancialEffect, OperationRecord, OperationRequest, Quantity,
    ResourceId, Result, StateSnapshot, Status, now,
};
use fin_ethereum::{
    EthereumAdapter, EthereumConfig, OracleObservationClock, USDC, WETH, WalletConnection,
    route_gas_ceiling,
};
use fin_evm::{
    EnvelopeSigner, EvmRpc, EvmRuntime, FrozenTransaction, LocalEnvelopeSigner, address,
    alloy::primitives::{Bytes, U256},
    hex_quantity, lower_address, quantity, rpc_u64, rpc_u256, u256,
};
use fin_protocol::{BrokerRequest, UnwindStep};
use fin_store::Store;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const FORK_BLOCK: u64 = 26_099_326;
const FORK_HASH: &str = "0xe24db6a0bd48918550f6a7c35c0e129172816d1f6641ac16a37459627551f7bf";
// Public Anvil default account 0. Never valid as a production-key fixture.
const SYNTHETIC_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
// Public deterministic fixture: SHA256 of
// "Fin isolated mainnet-fork synthetic return wallet fixture v1 - PUBLIC DO NOT FUND".
// Never use or fund this key outside the guarded local Anvil test.
const SYNTHETIC_RETURN_KEY: &str =
    "b013ed10eaec648ee473566eb1fc11f1bfa1495c21d9336047058e263cf02e8e";
const WALLET: &str = "isolated-anvil-wallet";
const RETURN_WALLET: &str = "isolated-anvil-return-wallet";
const MAX_WRITES: u64 = 24;
fin_evm::alloy::sol! {
    interface PlanningPool {
        function getReserveNormalizedIncome(address asset) external view returns (uint256 value);
    }
}
const ORDERED_OPERATIONS: [&str; 22] = [
    "wallet.transfer",
    "wallet.transfer",
    "wallet.wrap",
    "wallet.approve",
    "uniswap.swap",
    "wallet.transfer",
    "wallet.transfer",
    "wallet.approve",
    "aave.supply",
    "aave.collateral",
    "aave.collateral",
    "aave.borrow",
    "wallet.approve",
    "aave.repay",
    "aave.withdraw",
    "wallet.approve",
    "wallet.approve",
    "wallet.approve",
    "uniswap.swap",
    "wallet.approve",
    "wallet.approve",
    "wallet.unwrap",
];

fn blocked(message: &str) -> FinError {
    FinError::new(
        "ISOLATED_FORK_REQUIRED",
        "fork",
        message,
        "Use a fresh loopback Anvil at the exact documented block and chain. No remote fallback is permitted.",
    )
}

fn validate_endpoint(endpoint: &str, instance_id: &str) -> Result<()> {
    let port = endpoint
        .strip_prefix("http://127.0.0.1:")
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            blocked("The fork endpoint must be an exact IPv4 loopback URL with a selected port.")
        })?;
    if endpoint != format!("http://127.0.0.1:{port}")
        || !instance_id.starts_with("0x")
        || instance_id.len() != 66
        || !instance_id[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(blocked(
            "The fork endpoint or instance identity is malformed.",
        ));
    }
    Ok(())
}

fn validate_metadata(metadata: &Value, expected_instance_id: &str) -> Result<()> {
    if metadata["instanceId"].as_str() != Some(expected_instance_id) {
        return Err(blocked(
            "The fork endpoint is no longer the owned Anvil instance.",
        ));
    }
    if !metadata["clientVersion"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .starts_with("anvil/")
    {
        return Err(blocked("The fixed loopback peer is not Anvil."));
    }
    if metadata["chainId"].as_u64() != Some(1) {
        return Err(blocked("The isolated fork must explicitly use chain ID 1."));
    }
    let fork = &metadata["forkedNetwork"];
    if fork["forkBlockNumber"].as_u64() != Some(FORK_BLOCK)
        || fork["forkBlockHash"].as_str() != Some(FORK_HASH)
    {
        return Err(blocked(
            "The immutable fork anchor differs from the reviewed mainnet block.",
        ));
    }
    Ok(())
}

struct ForkRpc {
    endpoint: String,
    expected_instance_id: String,
    client: reqwest::Client,
    ids: AtomicU64,
    sent: AtomicU64,
}
impl ForkRpc {
    fn new() -> Result<Self> {
        let endpoint = std::env::var("FIN_FORK_RPC_URL")
            .map_err(|_| blocked("The owned fork runner did not provide its loopback endpoint."))?;
        let expected_instance_id = std::env::var("FIN_FORK_INSTANCE_ID").map_err(|_| {
            blocked("The owned fork runner did not provide the Anvil instance identity.")
        })?;
        validate_endpoint(&endpoint, &expected_instance_id)?;
        Ok(Self {
            endpoint,
            expected_instance_id,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|_| blocked("Could not construct loopback-only transport."))?,
            ids: AtomicU64::new(1),
            sent: AtomicU64::new(0),
        })
    }
    async fn raw(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let response = self
            .client
            .post(&self.endpoint)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| blocked("Loopback Anvil is unavailable."))?;
        if !response.status().is_success() {
            return Err(blocked("Loopback transport rejected the request."));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| blocked("Loopback RPC response is malformed."))?;
        if body["jsonrpc"] != json!("2.0") || body["id"] != json!(id) || body.get("error").is_some()
        {
            return Err(blocked(
                "Loopback RPC rejected a required method or returned a different identity.",
            ));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| blocked("Loopback RPC omitted the required result."))
    }
    async fn guard(&self) -> Result<()> {
        let metadata = self.raw("anvil_metadata", json!([])).await?;
        validate_metadata(&metadata, &self.expected_instance_id)?;
        if rpc_u64(&self.raw("eth_chainId", json!([])).await?)? != 1 {
            return Err(blocked("The isolated fork must explicitly use chain ID 1."));
        }
        let anchor = self
            .raw(
                "eth_getBlockByNumber",
                json!([format!("{FORK_BLOCK:#x}"), false]),
            )
            .await?;
        if anchor["hash"].as_str() != Some(FORK_HASH) || rpc_u64(&anchor["number"])? != FORK_BLOCK {
            return Err(blocked(
                "The immutable fork anchor differs from the reviewed mainnet block.",
            ));
        }
        Ok(())
    }
    async fn mutate_local(&self, method: &str, params: Value) -> Result<Value> {
        if !matches!(
            method,
            "anvil_setBalance" | "evm_mine" | "evm_setNextBlockTimestamp"
        ) {
            return Err(blocked("Unreviewed local test mutation was rejected."));
        }
        self.guard().await?;
        self.raw(method, params).await
    }
    async fn latest_timestamp(&self) -> Result<u64> {
        self.guard().await?;
        let block = self
            .raw("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        rpc_u64(&block["timestamp"])
    }
    async fn next_timestamp(&self, seconds: u64) -> Result<()> {
        if seconds == 0 || seconds > 3600 {
            return Err(blocked(
                "Unreviewed next-block timestamp advance was rejected.",
            ));
        }
        let timestamp = self
            .latest_timestamp()
            .await?
            .checked_add(seconds)
            .ok_or_else(|| blocked("Isolated fork timestamp overflowed."))?;
        self.mutate_local("evm_setNextBlockTimestamp", json!([timestamp]))
            .await?;
        Ok(())
    }
    async fn advance(&self, seconds: u64) -> Result<()> {
        if seconds == 0 || seconds > 3600 {
            return Err(blocked("Unreviewed fork clock advance was rejected."));
        }
        self.next_timestamp(seconds).await?;
        self.mutate_local("evm_mine", json!([])).await?;
        Ok(())
    }
    async fn mine(&self) -> Result<()> {
        self.advance(1).await
    }
}

#[test]
fn fork_endpoint_and_numeric_metadata_are_strict() {
    let instance = format!("0x{}", "a".repeat(64));
    assert!(validate_endpoint("http://127.0.0.1:8546", &instance).is_ok());
    for endpoint in [
        "http://localhost:8546",
        "http://127.0.0.1:0",
        "https://127.0.0.1:8546",
        "http://127.0.0.1:8546/path",
        "http://127.0.0.1:8546?redirect=remote",
    ] {
        assert!(
            validate_endpoint(endpoint, &instance).is_err(),
            "{endpoint}"
        );
    }
    let metadata = json!({
        "instanceId":instance,
        "clientVersion":"anvil/v1.6.0",
        "chainId":1,
        "forkedNetwork":{"forkBlockNumber":FORK_BLOCK,"forkBlockHash":FORK_HASH}
    });
    assert!(validate_metadata(&metadata, &instance).is_ok());
    assert!(validate_metadata(&metadata, &format!("0x{}", "b".repeat(64))).is_err());
    let mut wrong_chain = metadata.clone();
    wrong_chain["chainId"] = json!(5);
    assert!(validate_metadata(&wrong_chain, &instance).is_err());
    let mut wrong_block = metadata;
    wrong_block["forkedNetwork"]["forkBlockNumber"] = json!(FORK_BLOCK - 1);
    assert!(validate_metadata(&wrong_block, &instance).is_err());
}
struct ForkOracleClock(Arc<ForkRpc>);
#[async_trait]
impl OracleObservationClock for ForkOracleClock {
    async fn unix_seconds(&self) -> Result<u64> {
        self.0.latest_timestamp().await
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
            return Err(blocked("Unreviewed EVM transport method was rejected."));
        }
        if method == "eth_sendRawTransaction" {
            self.guard().await?;
            self.next_timestamp(1).await?;
            let previous = self.sent.fetch_add(1, Ordering::SeqCst);
            if previous >= MAX_WRITES {
                return Err(blocked(
                    "The finite isolated-test submission ceiling was exhausted.",
                ));
            }
        }
        // The runner-selected IPv4 loopback URL is validated above; no redirects,
        // ambient proxy, remote fallback, or credentials are possible.
        self.raw(method, params).await
    }
}
struct ForkSigner {
    rpc: Arc<ForkRpc>,
    keys: Vec<LocalEnvelopeSigner>,
    signatures: AtomicU64,
}
#[async_trait]
impl EnvelopeSigner for ForkSigner {
    async fn sign_prepared(&self, tx: &FrozenTransaction) -> Result<Bytes> {
        self.rpc.guard().await?; // Mandatory immediately before EVERY signature.
        if tx.chain_id != 1 {
            return Err(blocked("Frozen signer chain differs."));
        }
        let key = self
            .keys
            .iter()
            .find(|key| lower_address(key.address()) == tx.sender)
            .ok_or_else(|| {
                blocked("Frozen sender differs from the two isolated fixture identities.")
            })?;
        let previous = self.signatures.fetch_add(1, Ordering::SeqCst);
        if previous >= MAX_WRITES {
            return Err(blocked(
                "The finite isolated-test signature ceiling was exhausted.",
            ));
        }
        // A failed attempt conservatively consumes its slot; it is never
        // refunded into another possible financial dispatch after uncertainty.
        key.sign_prepared(tx).await
    }
}
struct IsolatedApproval {
    rpc: Arc<ForkRpc>,
    count: AtomicU64,
}
#[async_trait]
impl Authorizer for IsolatedApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.rpc.guard().await?;
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic-isolated-fork-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct OracleValuation(Arc<EthereumAdapter>);
#[async_trait]
impl AssetValuator for OracleValuation {
    async fn usd_micros(&self, a: &Amount) -> Result<(String, String, String)> {
        let (usd, source) = self
            .0
            .quote_usd(&a.asset.reference, a.atoms.clone())
            .await?;
        Ok((usd.to_string(), now(), source))
    }
}

async fn call<T: DeserializeOwned>(broker: &BrokerRuntime, request: BrokerRequest) -> Result<T> {
    let result = broker.handle(request).await;
    if !result.ok {
        return Err(result
            .error
            .expect("failed broker response carries sanitized error"));
    }
    serde_json::from_value(result.result.expect("successful response contains result"))
        .map_err(|_| blocked("The typed broker response could not be decoded."))
}
fn base_input(id: &str) -> Value {
    // Financial fee bounds are attached per operation after exact pinned-fee
    // financing is computed. No incomplete input is previewed or executed.
    json!({"wallet":WALLET,"network":"ethereum","chain_id":1,"request_id":id})
}
fn write_input(id: &str, extra: Value) -> Value {
    let mut value = base_input(id);
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    value
}
fn decimal(value: U256, decimals: u8) -> String {
    quantity(value).to_decimal(decimals)
}
fn qty(value: &Value) -> Result<U256> {
    let q: Quantity = serde_json::from_value(value.clone())
        .map_err(|_| blocked("Observed exact quantity is missing."))?;
    u256(&q)
}
async fn position_units(broker: &BrokerRuntime, token: &str, field: &str) -> Result<U256> {
    let v: Value = call(
        broker,
        BrokerRequest::Read {
            operation: "aave.positions".into(),
            input: json!({"wallet":WALLET,"network":"ethereum","chain_id":1,"tokens":[token]}),
        },
    )
    .await?;
    qty(&v["data"]["positions"][0][field])
}
async fn swap_input(
    broker: &BrokerRuntime,
    id: &str,
    input: &str,
    output: &str,
    units: U256,
    decimals: u8,
    target: &str,
) -> Result<Value> {
    let v: Value = call(broker, BrokerRequest::Read { operation:"uniswap.quote".into(),
        input:json!({"wallet":WALLET,"network":"ethereum","chain_id":1,"token_in":input,"token_out":output,"amount":decimal(units, decimals)}) }).await?;
    let quoted = qty(&v["data"]["output"])?;
    let minimum = quoted
        .checked_mul(U256::from(9950))
        .ok_or_else(|| blocked("Bounded quote arithmetic overflowed."))?
        / U256::from(10000);
    let out_decimals = if output == USDC { 6 } else { 18 };
    Ok(write_input(
        id,
        json!({"token_in":input,"token_out":output,"amount":decimal(units,decimals),
        "fee_tier":v["data"]["fee_tier"],"min_output":decimal(minimum,out_decimals),"slippage_bps":50,
        "deadline":chrono::Utc::now().timestamp() as u64 + 120,"recipient":target}),
    ))
}
struct Journey {
    broker: Arc<BrokerRuntime>,
    rpc: Arc<ForkRpc>,
    signer: Arc<ForkSigner>,
    approval: Arc<IsolatedApproval>,
    records: Vec<OperationRecord>,
}
impl Journey {
    fn failure_evidence(&self, operation: &str, error: &FinError) {
        println!(
            "{}",
            json!({
                "verification":"isolated-fork-failed-step",
                "step_index":self.records.len(),
                "operation":operation,
                "error_code":error.code,
                "error_stage":error.stage,
                "settled_writes_before_failure":self.records.len(),
                "authorizations":self.approval.count.load(Ordering::SeqCst),
                "signatures":self.signer.signatures.load(Ordering::SeqCst),
                "dispatches":self.rpc.sent.load(Ordering::SeqCst),
                "cleanup_passed":false,
                "production_writes":0,
            })
        );
    }

    async fn execute(&mut self, operation: &str, input: Value) -> Result<OperationRecord> {
        let request = OperationRequest {
            request_id: input["request_id"].as_str().unwrap().into(),
            operation: operation.into(),
            input,
            run_id: None,
        };
        let previous = (
            self.approval.count.load(Ordering::SeqCst),
            self.signer.signatures.load(Ordering::SeqCst),
            self.rpc.sent.load(Ordering::SeqCst),
        );
        let mut record: OperationRecord = call(
            &self.broker,
            BrokerRequest::Execute {
                request: request.clone(),
            },
        )
        .await
        .inspect_err(|error| self.failure_evidence(operation, error))?;
        for _ in 0..4 {
            if record.status.terminal() {
                break;
            }
            self.rpc.mine().await?;
            record = call(
                &self.broker,
                BrokerRequest::Status {
                    id: record.id.clone(),
                },
            )
            .await?;
        }
        if record.status != Status::Completed {
            let error = record.error.clone().unwrap_or_else(|| {
                blocked("Original write did not obtain complete settled proof.")
            });
            self.failure_evidence(operation, &error);
            return Err(error);
        }
        if record.submission.is_none() || record.effects.is_none() {
            return Err(blocked(
                "A completed operation lacks durable identity and verified effects.",
            ));
        }
        assert_eq!(self.approval.count.load(Ordering::SeqCst), previous.0 + 1);
        assert_eq!(
            self.signer.signatures.load(Ordering::SeqCst),
            previous.1 + 1
        );
        assert_eq!(self.rpc.sent.load(Ordering::SeqCst), previous.2 + 1);
        let duplicate: OperationRecord =
            call(&self.broker, BrokerRequest::Execute { request }).await?;
        assert_eq!(duplicate.id, record.id);
        assert_eq!(duplicate.submission, record.submission);
        assert_eq!(self.approval.count.load(Ordering::SeqCst), previous.0 + 1);
        assert_eq!(
            self.signer.signatures.load(Ordering::SeqCst),
            previous.1 + 1
        );
        assert_eq!(self.rpc.sent.load(Ordering::SeqCst), previous.2 + 1);
        self.records.push(record.clone());
        Ok(record)
    }
}
fn reconcile_cash(
    baseline: &StateSnapshot,
    final_state: &StateSnapshot,
    records: &[OperationRecord],
) -> Result<U256> {
    let mut credits = BTreeMap::<(ResourceId, AssetId), U256>::new();
    let mut debits = BTreeMap::<(ResourceId, AssetId), U256>::new();
    let mut gas = U256::ZERO;
    let mut add =
        |resource: &ResourceId, asset: &AssetId, atoms: &Quantity, credit: bool| -> Result<()> {
            let map = if credit { &mut credits } else { &mut debits };
            let old = map.entry((resource.clone(), asset.clone())).or_default();
            *old = old
                .checked_add(u256(atoms)?)
                .ok_or_else(|| blocked("Cash evidence sum overflowed."))?;
            Ok(())
        };
    for record in records {
        for e in &record
            .effects
            .as_ref()
            .ok_or_else(|| blocked("Missing settled proof."))?
            .effects
        {
            match e {
                FinancialEffect::CashDebit { resource, amount } => {
                    add(resource, &amount.asset, &amount.atoms, false)?
                }
                FinancialEffect::UsableCredit { to, amount } => {
                    add(to, &amount.asset, &amount.atoms, true)?
                }
                FinancialEffect::Transfer {
                    from,
                    to,
                    debit,
                    credit,
                } => {
                    add(from, &debit.asset, &debit.atoms, false)?;
                    add(to, &credit.asset, &credit.atoms, true)?;
                }
                FinancialEffect::Conversion {
                    resource,
                    spent,
                    received,
                } => {
                    add(resource, &spent.asset, &spent.atoms, false)?;
                    add(resource, &received.asset, &received.atoms, true)?;
                }
                FinancialEffect::Cost {
                    location: AccountingLocation::Cash { resource },
                    amount,
                    treatment: CostTreatment::AdditionalDebit,
                    ..
                } => {
                    add(resource, &amount.asset, &amount.atoms, false)?;
                    if amount.asset.reference == "native" {
                        gas = gas
                            .checked_add(u256(&amount.atoms)?)
                            .ok_or_else(|| blocked("Verified gas sum overflowed."))?;
                    }
                }
                _ => {}
            }
        }
    }
    for initial in &baseline.state.holdings {
        let key = (initial.resource.clone(), initial.amount.asset.clone());
        let expected = u256(&initial.amount.atoms)?
            .checked_add(*credits.get(&key).unwrap_or(&U256::ZERO))
            .and_then(|n| n.checked_sub(*debits.get(&key).unwrap_or(&U256::ZERO)))
            .ok_or_else(|| {
                blocked("Cash evidence exceeds that original custody source plus verified credits.")
            })?;
        let final_amount = final_state
            .state
            .holdings
            .iter()
            .find(|h| h.resource == initial.resource && h.amount.asset == initial.amount.asset)
            .ok_or_else(|| blocked("Required final exact custody/asset identity is absent."))?;
        assert_eq!(
            u256(&final_amount.amount.atoms)?,
            expected,
            "Independent exact custody proof for {} / {}",
            initial.resource.0,
            initial.amount.asset.reference
        );
    }
    Ok(gas)
}

#[tokio::test]
#[ignore = "Requires a fresh independently started, guarded loopback Anvil fork; no live fallback"]
async fn deployed_protocol_roundtrip_via_broker_restores_exact_state() -> Result<()> {
    let rpc = Arc::new(ForkRpc::new()?);
    rpc.guard().await?;
    let key = LocalEnvelopeSigner::from_private_key(SYNTHETIC_KEY)?;
    let target = lower_address(key.address());
    let return_key = LocalEnvelopeSigner::from_private_key(SYNTHETIC_RETURN_KEY)?;
    let return_target = lower_address(return_key.address());
    for recipient in [&target, &return_target] {
        let code = rpc
            .request("eth_getCode", json!([recipient, "latest"]))
            .await?;
        if !fin_evm::rpc_bytes(&code)?.is_empty() {
            return Err(blocked(
                "Both public synthetic fixtures must be independently observed EOAs before any local funding or Fin write.",
            ));
        }
    }
    let resource = ResourceId(format!("ethereum:1:{target}"));
    let return_resource = ResourceId(format!("ethereum:1:{return_target}"));
    let native = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let weth = AssetId {
        network: "ethereum:1".into(),
        reference: WETH.into(),
        decimals: 18,
    };
    let usdc = AssetId {
        network: "ethereum:1".into(),
        reference: USDC.into(),
        decimals: 6,
    };
    let signer = Arc::new(ForkSigner {
        rpc: rpc.clone(),
        keys: vec![key, return_key],
        signatures: AtomicU64::new(0),
    });
    let config = EthereumConfig::default();
    let adapter = Arc::new(
        EthereumAdapter::new(
            config.clone(),
            vec![
                WalletConnection {
                    name: WALLET.into(),
                    address: target.clone(),
                    signing_enabled: true,
                },
                WalletConnection {
                    name: RETURN_WALLET.into(),
                    address: return_target.clone(),
                    signing_enabled: true,
                },
            ],
            Arc::new(EvmRuntime::new(rpc.clone(), 1, Some(signer.clone()))),
        )?
        .with_isolated_oracle_clock(Arc::new(ForkOracleClock(rpc.clone()))),
    );
    let (eth_unit_price, _) = adapter
        .quote_usd("native", Quantity::from_decimal("1", 18)?)
        .await?;
    let one = U256::from(10).pow(U256::from(18));
    let units_for_usd = |usd: u64| -> Result<U256> {
        U256::from(usd)
            .checked_mul(U256::from(1_000_000))
            .and_then(|n| n.checked_mul(one))
            .map(|n| n / u256(&eth_unit_price).expect("exact bounded oracle quote"))
            .ok_or_else(|| blocked("Initial bounded funding arithmetic overflowed."))
    };
    let (pinned_fee_per_gas, pinned_tip) = adapter.runtime.fees().await?;
    let mut fee_caps = BTreeMap::<String, U256>::new();
    for operation in ORDERED_OPERATIONS {
        if fee_caps.contains_key(operation) {
            continue;
        }
        let gas = U256::from(route_gas_ceiling(operation)?);
        let with_headroom = gas
            .checked_mul(pinned_fee_per_gas)
            .and_then(|n| n.checked_mul(U256::from(3)))
            .and_then(|n| n.checked_add(U256::from(1)))
            .map(|n| n / U256::from(2))
            .ok_or_else(|| blocked("Pinned fee financing overflowed."))?;
        // Ceiling to one gwei of total transaction cost adds at most one gwei.
        let quantum = U256::from(1_000_000_000);
        let cap = with_headroom
            .checked_add(quantum - U256::from(1))
            .map(|n| n / quantum * quantum)
            .ok_or_else(|| blocked("Exact fee ceiling rounding overflowed."))?;
        fee_caps.insert(operation.into(), cap);
    }
    let ray = U256::from(10).pow(U256::from(27));
    let supply_index = adapter
        .runtime
        .call(
            address(&config.aave.as_ref().expect("mainnet Aave").pool)?,
            &PlanningPool::getReserveNormalizedIncomeCall {
                asset: address(WETH)?,
            },
            None,
        )
        .await?;
    if supply_index < ray || supply_index >= (U256::from(1) << 128) {
        return Err(blocked(
            "Pinned supply index is outside reviewed rounding bounds.",
        ));
    }
    let rounding_atoms = (supply_index + ray - U256::from(1)) / ray + U256::from(2);
    let rounding_usd = adapter.quote_usd(WETH, quantity(rounding_atoms)).await?.0;
    let repayment_cost_usd = adapter
        .quote_usd(USDC, Quantity::from_decimal("1.02", 6)?)
        .await?
        .0;
    let mut cost_caps_usd = BTreeMap::<String, Quantity>::new();
    for (operation, atoms) in &fee_caps {
        let mut cost = adapter.quote_usd("native", quantity(*atoms)).await?.0;
        if operation == "aave.repay" {
            cost = cost.checked_add(&repayment_cost_usd)?;
        }
        if matches!(operation.as_str(), "aave.supply" | "aave.withdraw") {
            cost = cost.checked_add(&rounding_usd)?;
        }
        cost_caps_usd.insert(operation.clone(), cost);
    }
    let mut full_fee_atoms = U256::ZERO;
    let mut return_cost_reserve = Quantity::zero();
    for operation in ORDERED_OPERATIONS {
        full_fee_atoms = full_fee_atoms
            .checked_add(fee_caps[operation])
            .ok_or_else(|| blocked("Full gas reserve overflowed."))?;
        return_cost_reserve = return_cost_reserve.checked_add(&cost_caps_usd[operation])?;
    }
    let gas_reserve_usd = adapter
        .quote_usd("native", quantity(full_fee_atoms))
        .await?
        .0;
    let borrowing_reserve = Quantity::from_u64(4_000_000);
    // Fee caps earmark the very same selected native holdings; they do
    // not represent an independent source or a second physical allocation.
    let reserved_usd = borrowing_reserve.checked_add(&gas_reserve_usd)?;
    let pinned_header = rpc
        .request("eth_getBlockByNumber", json!(["latest", false]))
        .await?;
    println!(
        "{}",
        json!({
            "verification":"isolated-fork-pre-funding-financing",
            "fork_block":FORK_BLOCK,"fork_hash":FORK_HASH,
            "observed_block_number":rpc_u64(&pinned_header["number"])?,
            "observed_base_fee_per_gas_atoms":quantity(rpc_u256(&pinned_header["baseFeePerGas"])?),
            "runtime_max_fee_per_gas_atoms":quantity(pinned_fee_per_gas),
            "runtime_max_priority_fee_per_gas_atoms":quantity(pinned_tip),
            "native_unit_price_usd_micros":eth_unit_price,
            "ordered_operation_count":ORDERED_OPERATIONS.len(),
            "sum_reviewed_gas_envelopes":ORDERED_OPERATIONS.iter().try_fold(0u64,|sum,operation|sum.checked_add(route_gas_ceiling(operation)?).ok_or_else(||blocked("Profile gas sum overflowed.")))?,
            "per_operation_fee_caps_atoms":fee_caps.iter().map(|(operation,cap)|(operation.clone(),quantity(*cap))).collect::<BTreeMap<_,_>>(),
            "full_gas_atoms":quantity(full_fee_atoms),"full_gas_usd_micros":gas_reserve_usd,
            "full_step_cost_reserve_usd_micros":return_cost_reserve,
            "borrow_reserve_usd_micros":borrowing_reserve,
            "current_policy_before_principal_usd_micros":reserved_usd,
            "funding_has_not_occurred":true,"fin_submissions":rpc.sent.load(Ordering::SeqCst),
            "accounting":"Physical selected holdings include all gas-financing atoms. Test cap is selected physical holdings plus borrowed-proceeds reserve; finite execution/unwind costReserve is an earmark inside those same tracked holdings, never an additional source.",
            "fee_source":"Unmodified local RPC runtime.fees(): two times observed base fee plus returned priority with existing0.01gwei minimum; each unmodified reviewed gas ceiling times1.5 headroom. No fee/RPC override."
        })
    );
    if reserved_usd >= Quantity::from_u64(100_000_000) {
        return Err(blocked(
            "Pinned complete physical gas funding and borrowing reserve leave no principal within the hundred-dollar allocation.",
        ));
    }
    let remaining_principal = Quantity::from_u64(100_000_000).checked_sub(&reserved_usd)?;
    let wrap_usd = (u256(&remaining_principal)? / U256::from(1_000_000))
        .min(U256::from(30))
        .to::<u64>();
    if wrap_usd < 10 {
        return Err(blocked(
            "Pinned fees leave less than the explicit ten-dollar minimum practical protocol principal within the full hundred-dollar allocation.",
        ));
    }
    let swap_usd = (wrap_usd / 3).min(10);
    let wrap_units = units_for_usd(wrap_usd)?;
    let swap_units = units_for_usd(swap_usd)?;
    let return_funding = fee_caps["wallet.transfer"]
        .checked_mul(U256::from(2))
        .ok_or_else(|| blocked("Destination return funding overflowed."))?;
    let funding = full_fee_atoms
        .checked_sub(return_funding)
        .and_then(|n| n.checked_add(wrap_units))
        .ok_or_else(|| blocked("Selected primary funding overflowed."))?;
    let selected_native_usd = adapter
        .quote_usd(
            "native",
            quantity(
                funding
                    .checked_add(return_funding)
                    .ok_or_else(|| blocked("Selected pool overflowed."))?,
            ),
        )
        .await?
        .0;
    let total_allocation = selected_native_usd.checked_add(&borrowing_reserve)?;
    if return_cost_reserve > selected_native_usd {
        return Err(blocked(
            "The complete finite cost earmark exceeds the original selected physical allocation.",
        ));
    }
    if total_allocation > Quantity::from_u64(100_000_000) {
        return Err(blocked(
            "Complete exact physical fork funding plus borrowed-proceeds reserve exceeds the hundred-dollar allocation.",
        ));
    }
    rpc.mutate_local("anvil_setBalance", json!([target, hex_quantity(funding)]))
        .await?;
    rpc.mutate_local(
        "anvil_setBalance",
        json!([return_target, hex_quantity(return_funding)]),
    )
    .await?;
    let approval = Arc::new(IsolatedApproval {
        rpc: rpc.clone(),
        count: AtomicU64::new(0),
    });
    let temp =
        tempfile::tempdir().map_err(|_| blocked("Temporary isolated journal is unavailable."))?;
    let broker_config = BrokerConfigV2 {
        version: 2,
        networks: vec![NetworkConfig::ethereum_mainnet()],
        deployments: vec![DeploymentConfig::ethereum_mainnet()],
        connections: vec![
            ConnectionConfig::Ethereum {
                name: WALLET.into(),
                resource: resource.clone(),
                credential_ref: Some("11111111-1111-4111-8111-111111111111".into()),
                network: "ethereum:1".into(),
                address: target.clone(),
                signing_enabled: true,
                assets: vec![native.clone(), weth.clone(), usdc.clone()],
            },
            ConnectionConfig::Ethereum {
                name: RETURN_WALLET.into(),
                resource: return_resource.clone(),
                credential_ref: Some("22222222-2222-4222-8222-222222222222".into()),
                network: "ethereum:1".into(),
                address: return_target.clone(),
                signing_enabled: true,
                assets: vec![native.clone(), weth, usdc],
            },
        ],
        policy: BrokerPolicy {
            // The personal gross-turnover budget is independent of this test's
            // separately checked $100 physical funding and restoration plan.
            max_run_allocation_usd_micros: "5000000000".into(),
            ..BrokerPolicy::beta()
        },
        approver_uid: 501,
        extensions: BTreeMap::new(),
    };
    // The mainnet endpoint strings are inert validated network metadata here.
    // Injected ForkRpc alone supplies every read and dispatch; no production
    // credential provider, protected runtime, or public transport is constructed.
    let broker = compose_runtime(
        broker_config,
        RuntimeDependencies {
            store: Store::open(temp.path().join("fork.sqlite"))
                .map_err(|_| blocked("Temporary durable journal could not be opened."))?,
            adapters: vec![adapter.clone()],
            authorizer: approval.clone(),
            valuator: Arc::new(OracleValuation(adapter.clone())),
        },
    )?;
    let baseline = broker.snapshot().await?;
    assert!(baseline.complete && baseline.pending.is_empty());
    assert_eq!(
        baseline
            .state
            .holdings
            .iter()
            .find(|h| h.resource == resource && h.amount.asset == native)
            .unwrap()
            .amount
            .atoms,
        quantity(funding)
    );
    assert_eq!(
        baseline
            .state
            .holdings
            .iter()
            .find(|h| h.resource == return_resource && h.amount.asset == native)
            .unwrap()
            .amount
            .atoms,
        quantity(return_funding)
    );
    assert!(
        baseline
            .state
            .holdings
            .iter()
            .filter(|h| h.amount.asset.reference != "native")
            .all(|h| h.amount.atoms.is_zero())
    );
    assert!(
        baseline
            .state
            .positions
            .iter()
            .all(|p| p.amount.atoms.is_zero())
    );
    assert!(
        baseline
            .state
            .authorities
            .iter()
            .all(|a| a.value == "0" || a.value == "false")
    );
    let supply_units = wrap_units
        .checked_sub(swap_units)
        .ok_or_else(|| blocked("Principal split is invalid."))?;
    let planned_forward =
        swap_input(&broker, "fork-03-swap", WETH, USDC, swap_units, 18, &target).await?;
    let minimum_forward = u256(&Quantity::from_decimal(
        planned_forward["min_output"].as_str().unwrap(),
        6,
    )?)?;
    let minimum_reverse = minimum_forward
        .checked_sub(U256::from(20_000))
        .ok_or_else(|| blocked("Forward proceeds cannot finance bounded repayment interest."))?;
    let mut planned_reverse = swap_input(
        &broker,
        "fork-15-reverse",
        USDC,
        WETH,
        minimum_reverse,
        6,
        &target,
    )
    .await?;
    planned_reverse["amount"] = json!("all");
    planned_reverse["max_amount"] = json!("12");
    planned_reverse["slippage_bps"] = json!(1000);
    let mut rows = vec![
        (
            "wallet.wrap",
            write_input("fork-01-wrap", json!({"amount":decimal(wrap_units,18)})),
            "Introduce only selected native principal as WETH.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-02-router-weth",
                json!({"token":WETH,"spender":config.uniswap.as_ref().expect("mainnet Uniswap").router,"amount":decimal(swap_units,18)}),
            ),
            "Approve finite forward input.",
        ),
        (
            "uniswap.swap",
            planned_forward,
            "Swap bounded WETH to USDC in the verified liquid pool.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-04-pool-weth",
                json!({"token":WETH,"spender":config.aave.as_ref().expect("mainnet Aave").pool,"amount":decimal(supply_units,18)}),
            ),
            "Approve finite collateral supply.",
        ),
        (
            "aave.supply",
            write_input(
                "fork-05-supply",
                json!({"token":WETH,"amount":decimal(supply_units,18)}),
            ),
            "Supply only introduced WETH; preserve exact scaled rounding proof.",
        ),
        (
            "aave.collateral",
            write_input(
                "fork-06-collateral-disable",
                json!({"token":WETH,"enabled":false}),
            ),
            "Verify collateral disable before debt exists.",
        ),
        (
            "aave.collateral",
            write_input(
                "fork-07-collateral-enable",
                json!({"token":WETH,"enabled":true}),
            ),
            "Enable proved supplied collateral.",
        ),
        (
            "aave.borrow",
            write_input(
                "fork-08-borrow",
                json!({"token":USDC,"amount":"1","min_health_factor":"1.5"}),
            ),
            "Borrow one USDC within the four-dollar borrowing reserve.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-09-pool-usdc",
                json!({"token":USDC,"spender":config.aave.as_ref().expect("mainnet Aave").pool,"amount":"1.02"}),
            ),
            "Approve finite full debt/interest repayment cap.",
        ),
        (
            "aave.repay",
            write_input(
                "fork-10-repay",
                json!({"token":USDC,"amount":"all","max_amount":"1.02"}),
            ),
            "Repay complete verified variable debt and proved accrued interest.",
        ),
        (
            "aave.withdraw",
            write_input(
                "fork-11-withdraw",
                json!({"token":WETH,"amount":"all","max_amount":decimal(units_for_usd(22)?,18),"recipient":target}),
            ),
            "Close the complete scaled WETH claim including bounded proved yield.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-12-clear-pool-weth",
                json!({"token":WETH,"spender":config.aave.as_ref().expect("mainnet Aave").pool,"amount":"0"}),
            ),
            "Restore original zero Pool WETH allowance.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-13-clear-pool-usdc",
                json!({"token":USDC,"spender":config.aave.as_ref().expect("mainnet Aave").pool,"amount":"0"}),
            ),
            "Restore original zero Pool USDC allowance.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-14-router-usdc",
                json!({"token":USDC,"spender":config.uniswap.as_ref().expect("mainnet Uniswap").router,"amount":"12"}),
            ),
            "Approve explicit finite conditional cleanup cap, then restore it.",
        ),
        (
            "uniswap.swap",
            planned_reverse,
            "Reverse every exact remaining USDC atom under a finite full-balance cap.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-16-clear-router-usdc",
                json!({"token":USDC,"spender":config.uniswap.as_ref().expect("mainnet Uniswap").router,"amount":"0"}),
            ),
            "Restore original zero reverse router allowance.",
        ),
        (
            "wallet.approve",
            write_input(
                "fork-17-clear-router-weth",
                json!({"token":WETH,"spender":config.uniswap.as_ref().expect("mainnet Uniswap").router,"amount":"0"}),
            ),
            "Restore original zero forward router allowance.",
        ),
        (
            "wallet.unwrap",
            write_input(
                "fork-18-unwrap",
                json!({"amount":"all","max_amount":decimal(units_for_usd(40)?,18)}),
            ),
            "Return all proved final WETH to original native representation under finite cap.",
        ),
    ];
    let native_transfer = decimal(units_for_usd(1)?, 18);
    let native_out = write_input(
        "fork-native-out",
        json!({"token":"native","amount":native_transfer,"recipient":return_target}),
    );
    let mut native_back = write_input(
        "fork-native-back",
        json!({"token":"native","amount":native_transfer,"recipient":target}),
    );
    native_back["wallet"] = json!(RETURN_WALLET);
    rows.insert(0,("wallet.transfer",native_out,"Move exact selected native principal to the separately gas-funded synthetic return wallet."));
    rows.insert(1,("wallet.transfer",native_back,"Restore original native custody allocation; charge return gas only to its verified sender."));
    let erc20_out = write_input(
        "fork-usdc-out",
        json!({"token":USDC,"amount":"0.25","recipient":return_target}),
    );
    let mut erc20_back = write_input(
        "fork-usdc-back",
        json!({"token":USDC,"amount":"0.25","recipient":target}),
    );
    erc20_back["wallet"] = json!(RETURN_WALLET);
    rows.insert(
        5,
        (
            "wallet.transfer",
            erc20_out,
            "Move exact introduced USDC to the same configured return-capable synthetic wallet.",
        ),
    );
    rows.insert(6,("wallet.transfer",erc20_back,"Return every transferred USDC atom to its original custody source before protocol use."));
    assert_eq!(
        rows.iter().map(|row| row.0).collect::<Vec<_>>(),
        ORDERED_OPERATIONS
    );
    let executable_plan = rows
        .into_iter()
        .map(|(operation, mut input, description)| {
            input["max_fee_eth"] = json!(decimal(fee_caps[operation], 18));
            UnwindStep {
                operation: operation.into(),
                input,
                description: description.into(),
                maximum_fee_usd_micros: cost_caps_usd[operation].to_string(),
            }
        })
        .collect::<Vec<_>>();
    let mut j = Journey {
        broker: broker.clone(),
        rpc: rpc.clone(),
        signer: signer.clone(),
        approval: approval.clone(),
        records: vec![],
    };
    let mut advanced_debt = None;
    for step in executable_plan {
        j.execute(&step.operation, step.input).await?;
        if step.operation == "aave.borrow" {
            let before = position_units(&broker, USDC, "variable_debt").await?;
            rpc.advance(3600).await?;
            let after = position_units(&broker, USDC, "variable_debt").await?;
            assert!(
                after > before,
                "The isolated one-hour advance must prove actual positive debt accrual."
            );
            advanced_debt = Some((before, after));
        }
        if step.operation == "aave.repay" {
            assert_eq!(
                position_units(&broker, USDC, "variable_debt").await?,
                U256::ZERO
            );
        }
        if step.operation == "aave.withdraw" {
            assert_eq!(
                position_units(&broker, WETH, "supplied").await?,
                U256::ZERO,
                "Incomplete cleanup cannot pass"
            );
        }
    }
    let final_state = broker.snapshot().await?;
    assert!(final_state.complete && final_state.pending.is_empty());
    assert!(
        final_state
            .state
            .positions
            .iter()
            .all(|p| p.amount.atoms.is_zero())
    );
    assert!(
        final_state
            .state
            .holdings
            .iter()
            .filter(|h| h.amount.asset.reference != "native")
            .all(|h| h.amount.atoms.is_zero())
    );
    assert_eq!(final_state.state.authorities, baseline.state.authorities);
    let gas = reconcile_cash(&baseline, &final_state, &j.records)?;
    let (gas_usd, _) = adapter.quote_usd("native", quantity(gas)).await?;
    assert!(gas_usd <= gas_reserve_usd);
    assert_eq!(
        signer.signatures.load(Ordering::SeqCst),
        j.records.len() as u64
    );
    assert_eq!(
        approval.count.load(Ordering::SeqCst),
        j.records.len() as u64
    );
    assert_eq!(rpc.sent.load(Ordering::SeqCst), j.records.len() as u64);
    assert!(j.records.len() as u64 <= MAX_WRITES);
    // Restoration is asserted by this verification harness, never a runtime
    // execution gate. All writes above execute without a run or classification.
    let (debt_before, debt_after) = advanced_debt
        .ok_or_else(|| blocked("The required isolated interest advance was not exercised."))?;
    println!(
        "{}",
        json!({
            "verification":"isolated-mainnet-fork",
            "fork_block":FORK_BLOCK,"fork_hash":FORK_HASH,
            "accrual_source":"guarded loopback Anvil evm_setNextBlockTimestamp plus evm_mine; actual historical Aave variable-debt index/backing proof at canonical receipts",
            "advanced_seconds":3600,"debt_before_advance_atoms":quantity(debt_before),"debt_after_advance_atoms":quantity(debt_after),
            "final_variable_debt_atoms":"0","final_supplied_atoms":"0",
            "native_gas_atoms":quantity(gas),"native_gas_usd_micros":gas_usd,
        "pinned_max_fee_per_gas_atoms":quantity(pinned_fee_per_gas),"per_operation_fee_caps_atoms":fee_caps.iter().map(|(operation,cap)|(operation.clone(),quantity(*cap))).collect::<BTreeMap<_,_>>(),
        "selected_native_usd_micros":selected_native_usd,"full_cost_reserve_usd_micros":return_cost_reserve,"borrow_reserve_usd_micros":borrowing_reserve,"total_allocation_usd_micros":total_allocation,"wrap_principal_usd":wrap_usd,"swap_principal_usd":swap_usd,
            "settled_writes":j.records.len(),"duplicates_without_new_actions":j.records.len(),
            "restored_resources":baseline.state.holdings.iter().map(|h|h.resource.0.clone()).collect::<std::collections::BTreeSet<_>>(),
            "restoration_checked_by":"test harness cash, positions and allowance assertions",
            "execution_requires_run":false,
            "evidence_classes":{"synthetic_authorization":true,"deployed_protocols_on_fork":true,"live_write":false,"touch_id":false}
        })
    );
    println!(
        "Isolated mainnet-fork verification: {} settled writes; {} duplicates without approval/sign/submission; guarded 1h debt accrual, exact cash evidence and complete cleanup passed. No biometric or live-write claim.",
        j.records.len(),
        j.records.len()
    );
    Ok(())
}
