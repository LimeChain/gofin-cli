use async_trait::async_trait;
use gofin_broker::coinbase_deposit::CoinbaseBaseDepositAdapter;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use gofin_domain::*;
use gofin_evm::{EnvelopeSigner, LocalEnvelopeSigner, alloy::primitives::Bytes};
use gofin_exchange::coinbase::*;
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[allow(dead_code)]
#[path = "support/coinbase_withdrawal_client.rs"]
mod coinbase_withdrawal_client;
use coinbase_withdrawal_client::Client;
#[path = "support/coinbase_http_fixture.rs"]
mod coinbase_http_fixture;

const SOURCE: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const WALLET: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const DESTINATION: &str = "0x1111111111111111111111111111111111111111";
const ARC_USDC_ALIAS: &str = "0x3600000000000000000000000000000000000000";

struct Coinbase {
    credited: AtomicBool,
    wrong_address: AtomicBool,
    include_to: AtomicBool,
    incoming_send: AtomicBool,
    wrong_receive_hash: AtomicBool,
    receive_amount: Mutex<String>,
    receive_status: Mutex<String>,
    credit_balance: Mutex<String>,
    address_network: Mutex<Option<String>>,
    address_tag: Mutex<Option<String>>,
    address: Mutex<String>,
    receive_network: Mutex<String>,
    receive_network_name_field: AtomicBool,
    account_currency: Mutex<String>,
    receive_currency: Mutex<String>,
    invalid_account_flag: Mutex<Option<(String, Value)>>,
    hash: Mutex<Option<String>>,
}
#[async_trait]
impl CoinbaseApi for Coinbase {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        _: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        assert!(guard.is_none());
        let payload = match endpoint {
            CoinbaseRoute::KeyPermissions => {
                json!({"portfolio_uuid":"portfolio-uuid","can_view":true})
            }
            CoinbaseRoute::LegacyAccounts { .. } => {
                json!({"data":[{"id":"usdc-account","currency":{"code":self.account_currency.lock().unwrap().clone()}}],"pagination":{}})
            }
            CoinbaseRoute::Account { account_id } => {
                let _ = account_id;
                {
                    let currency = self.account_currency.lock().unwrap().clone();
                    let balance = if self.credited.load(Ordering::SeqCst) {
                        self.credit_balance.lock().unwrap().clone()
                    } else {
                        "0".into()
                    };
                    let mut value = json!({"account":{"uuid":"usdc-account","currency":currency,"active":true,"ready":true,"available_balance":{"value":balance,"currency":currency},"hold":{"value":"0","currency":currency}}});
                    if let Some((field, invalid)) =
                        self.invalid_account_flag.lock().unwrap().as_ref()
                    {
                        value["account"][field] = invalid.clone();
                    }
                    value
                }
            }
            CoinbaseRoute::DepositAddresses { account_id: _ } => {
                let mut address =
                    json!({"id":"base-address","address":self.address.lock().unwrap().clone()});
                if let Some(network) = self.address_network.lock().unwrap().as_ref() {
                    address["network"] = json!(network);
                }
                if let Some(tag) = self.address_tag.lock().unwrap().as_ref() {
                    address["destination_tag"] = json!(tag);
                }
                json!({"data":[address],"pagination":{}})
            }
            CoinbaseRoute::Transactions { account_id, .. } => {
                assert_eq!(account_id, "usdc-account");
                let rows = if self.credited.load(Ordering::SeqCst) {
                    let source_hash = self.hash.lock().unwrap().clone().unwrap_or_default();
                    let transaction_hash = if self.wrong_receive_hash.load(Ordering::SeqCst) {
                        "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
                    } else {
                        source_hash
                    };
                    let kind = if self.incoming_send.load(Ordering::SeqCst) {
                        "send"
                    } else {
                        "receive"
                    };
                    let mut row = json!({"id":"receive-1","type":kind,"status":self.receive_status.lock().unwrap().clone(),"amount":{"amount":self.receive_amount.lock().unwrap().clone(),"currency":self.receive_currency.lock().unwrap().clone()},"network":{"hash":transaction_hash}});
                    let label_field = if self.receive_network_name_field.load(Ordering::SeqCst) {
                        "network_name"
                    } else {
                        "name"
                    };
                    row["network"][label_field] =
                        json!(self.receive_network.lock().unwrap().clone());
                    if self.include_to.load(Ordering::SeqCst) {
                        row["to"] = json!({"address":if self.wrong_address.load(Ordering::SeqCst){"0x2222222222222222222222222222222222222222"}else{DESTINATION}});
                    }
                    vec![row]
                } else {
                    vec![]
                };
                json!({"data":rows,"pagination":{}})
            }
            other => panic!("unexpected Coinbase fixture endpoint {other:?}"),
        };
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&payload).unwrap(),

            pagination: Default::default(),
        })
    }
}

struct Rpc {
    chain_id: u64,
    source: Mutex<String>,
    symbol: Mutex<String>,
    token_decimals: std::sync::atomic::AtomicU8,
    transaction: Mutex<Option<gofin_evm::FrozenTransaction>>,
    hash: Mutex<Option<String>>,
    receipt_recipient: Mutex<String>,
    submitted: AtomicBool,
    submissions: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl gofin_evm::EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let result = match method {
            "eth_chainId" => json!(format!("0x{:x}", self.chain_id)),
            "eth_getCode" => {
                if params[0]
                    .as_str()
                    .is_some_and(|address| address.eq_ignore_ascii_case(DESTINATION))
                {
                    json!("0x")
                } else {
                    json!("0x6000")
                }
            }
            "eth_getBalance" => json!("0x3635c9adc5dea00000"),
            "eth_getTransactionCount" => json!("0x0"),
            "eth_estimateGas" => json!("0x10000"),
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_blockNumber" => json!("0x100"),
            "eth_getBlockByNumber" => {
                json!({"number":if params[0]=="latest" || params[0]=="finalized" {"0x100"} else {params[0].as_str().unwrap_or("0x64")},"hash":format!("0x{}","cd".repeat(32)),"baseFeePerGas":"0x3b9aca00","extraData":format!("0x00{}","0".repeat(16))})
            }
            "eth_call" => {
                let data = params[0]["data"]
                    .as_str()
                    .or_else(|| params[0]["input"].as_str())
                    .unwrap_or("");
                if data.starts_with("0x313ce567") {
                    json!(format!(
                        "0x{:064x}",
                        self.token_decimals.load(Ordering::SeqCst)
                    ))
                } else if data.starts_with("0x70a08231") {
                    let decimals = self.token_decimals.load(Ordering::SeqCst) as u32;
                    json!(format!("0x{:064x}", 50_u128 * 10_u128.pow(decimals)))
                } else if data.starts_with("0x95d89b41") || data.starts_with("0x06fdde03") {
                    let symbol_name = self.symbol.lock().unwrap().clone();
                    let symbol = hex::encode(symbol_name.as_bytes());
                    json!(format!(
                        "0x{}20{}{:02x}{}{}",
                        "0".repeat(62),
                        "0".repeat(62),
                        symbol_name.len(),
                        symbol,
                        "0".repeat(64 - symbol_name.len() * 2)
                    ))
                } else if data.starts_with("0xa9059cbb") || data.starts_with("0x960e3a23") {
                    json!(format!("0x{:064x}", 1))
                } else {
                    json!(format!("0x{:064x}", 0))
                }
            }
            "eth_sendRawTransaction" => {
                let raw =
                    hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let hash = format!("{:#x}", gofin_evm::alloy::primitives::keccak256(raw));
                *self.hash.lock().unwrap() = Some(hash.clone());
                self.submitted.store(true, Ordering::SeqCst);
                self.submissions.fetch_add(1, Ordering::SeqCst);
                json!(hash)
            }
            "eth_getTransactionByHash" => {
                let tx = self.transaction.lock().unwrap();
                let tx = tx.as_ref().unwrap();
                json!({"hash":self.hash.lock().unwrap().clone().unwrap_or_default(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","from":tx.sender,"to":tx.to,"chainId":format!("0x{:x}", self.chain_id),"type":"0x2","nonce":"0x0","gas":format!("{:#x}",tx.gas),"value":format!("{:#x}",gofin_evm::u256(&tx.value)?),"input":tx.calldata,"maxFeePerGas":format!("{:#x}",gofin_evm::u256(&tx.max_fee_per_gas)?),"maxPriorityFeePerGas":format!("{:#x}",gofin_evm::u256(&tx.max_priority_fee_per_gas)?),"accessList":[]})
            }
            "eth_getTransactionReceipt" => {
                if self.submitted.load(Ordering::SeqCst) {
                    let source = self.source.lock().unwrap().clone();
                    let tx = self.transaction.lock().unwrap();
                    let calldata = tx
                        .as_ref()
                        .map(|tx| tx.calldata.trim_start_matches("0x").to_owned())
                        .unwrap_or_default();
                    let amount = calldata
                        .get(calldata.len().saturating_sub(64)..)
                        .and_then(|value| u128::from_str_radix(value, 16).ok())
                        .unwrap_or(
                            5 * 10_u128.pow(self.token_decimals.load(Ordering::SeqCst) as u32),
                        );
                    let recipient = self.receipt_recipient.lock().unwrap().clone();
                    let logs = if source == "native" {
                        json!([{"address":ARC_USDC_ALIAS,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",format!("0x{}{}","0".repeat(24),WALLET.trim_start_matches("0x")),format!("0x{}{}","0".repeat(24),recipient.trim_start_matches("0x"))],"data":format!("0x{:064x}",5_000_000_u64),"transactionHash":self.hash.lock().unwrap().clone().unwrap_or_default(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","logIndex":"0x0","removed":false}])
                    } else {
                        json!([{"address":source,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",format!("0x{}{}","0".repeat(24),WALLET.trim_start_matches("0x")),format!("0x{}{}","0".repeat(24),recipient.trim_start_matches("0x"))],"data":format!("0x{:064x}",amount),"transactionHash":self.hash.lock().unwrap().clone().unwrap_or_default(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","logIndex":"0x0","removed":false}])
                    };
                    json!({"transactionHash":self.hash.lock().unwrap().clone().unwrap_or_default(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x10000","effectiveGasPrice":"0x3b9aca00","l1Fee":"0x0","logs":logs})
                } else {
                    Value::Null
                }
            }
            _ => panic!("unexpected synthetic Base RPC method {method}"),
        };
        Ok(result)
    }
}
struct RecordingSigner {
    inner: LocalEnvelopeSigner,
    rpc: Arc<Rpc>,
}
#[async_trait]
impl EnvelopeSigner for RecordingSigner {
    async fn sign_prepared(&self, tx: &gofin_evm::FrozenTransaction) -> Result<Bytes> {
        *self.rpc.transaction.lock().unwrap() = Some(tx.clone());
        self.inner.sign_prepared(tx).await
    }
}
struct Journal {
    candidate: PreparedOperation,
    saved: Mutex<Option<Submission>>,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn assert_candidate(&self, candidate: &PreparedOperation) -> Result<()> {
        assert_eq!(candidate, &self.candidate);
        Ok(())
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        *self.saved.lock().unwrap() = Some(submission.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, _: &Submission, _: &Value) -> Result<()> {
        Ok(())
    }
}
struct Price;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Price {
    async fn quote(&self, _: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic Base deposit fixture".into(),
        })
    }
}
#[async_trait]
impl gofin_ethereum::WalletValuation for Price {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        let atomic_per_unit = Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))?;
        let usd_micros_per_unit = Quantity::from_u64(1_000_000);
        Ok((
            units.mul_div_ceil(&usd_micros_per_unit, &atomic_per_unit)?,
            "synthetic USD 1.00 per asset unit".into(),
        ))
    }
}

struct DepositApproval(std::sync::atomic::AtomicUsize);
#[async_trait]
impl Authorizer for DepositApproval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert_eq!(challenge.purpose, "financial_write");
        let binding = challenge.binding.to_string();
        assert!(
            binding.contains(SOURCE),
            "approval must bind exact Base USDC contract"
        );
        assert!(
            binding.contains(DESTINATION),
            "approval must bind Coinbase's receive address"
        );
        assert!(challenge.summary.contains("USDC"));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic exact-scope Coinbase deposit approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct TestClock;
impl gofin_domain::clock::Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}
fn deposit_profile(api: Arc<dyn CoinbaseApi>) -> Arc<CoinbaseProfile> {
    Arc::new(
        CoinbaseProfile::new(
            vec![CoinbaseAccount {
                name: "coinbase".into(),
                resource: ResourceId("coinbase:portfolio:portfolio-uuid".into()),
                credential_ref: "synthetic".into(),
                physical_identity: "portfolio-uuid".into(),
                identity_verified: true,
                portfolio_id: Some("portfolio-uuid".into()),
                product: CoinbaseProduct::Advanced,
            }],
            std::collections::BTreeMap::from([("synthetic".into(), api)]),
        )
        .unwrap(),
    )
}

fn fixture_parts(
    address_network: Option<&str>,
    address_tag: Option<&str>,
    address: &str,
    receive_network: &str,
    chain_id: u64,
) -> (
    CoinbaseBaseDepositAdapter,
    Arc<Coinbase>,
    Arc<Rpc>,
    Arc<gofin_ethereum::EthereumAdapter>,
) {
    let source = match chain_id {
        1 => "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        8453 => SOURCE,
        42161 => "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
        10 => "0x0b2c639c533813f4aa9d7837caf62653d097ff85",
        137 => "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359",
        43114 => "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e",
        5042 => "native",
        999 => "0xb88339cb7199b77e23db6e890353e22632ba630f",
        _ => panic!("unsupported fixture chain {chain_id}"),
    };
    let rpc = Arc::new(Rpc {
        chain_id,
        source: Mutex::new(source.into()),
        symbol: Mutex::new("USDC".into()),
        token_decimals: std::sync::atomic::AtomicU8::new(if source == "native" { 18 } else { 6 }),
        transaction: Mutex::new(None),
        hash: Mutex::new(None),
        receipt_recipient: Mutex::new(DESTINATION.into()),
        submitted: AtomicBool::new(false),
        submissions: std::sync::atomic::AtomicUsize::new(0),
    });
    let signer = Arc::new(RecordingSigner {
        inner: LocalEnvelopeSigner::from_private_key(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap(),
        rpc: rpc.clone(),
    });
    let api = Arc::new(Coinbase {
        credited: AtomicBool::new(false),
        wrong_address: AtomicBool::new(false),
        include_to: AtomicBool::new(true),
        incoming_send: AtomicBool::new(false),
        wrong_receive_hash: AtomicBool::new(false),
        receive_amount: Mutex::new("5.000000000000000000".into()),
        receive_status: Mutex::new("completed".into()),
        credit_balance: Mutex::new("5.000000000000000000".into()),
        address_network: Mutex::new(address_network.map(str::to_owned)),
        address_tag: Mutex::new(address_tag.map(str::to_owned)),
        address: Mutex::new(address.to_owned()),
        receive_network: Mutex::new(receive_network.to_owned()),
        receive_network_name_field: AtomicBool::new(false),
        account_currency: Mutex::new("USDC".into()),
        receive_currency: Mutex::new("USDC".into()),
        invalid_account_flag: Mutex::new(None),
        hash: Mutex::new(None),
    });
    let profile = deposit_profile(api.clone());
    let eth = gofin_ethereum::EthereumAdapter::new(
        gofin_ethereum::EthereumConfig::wallet_only(chain_id, 2, vec![]),
        vec![gofin_ethereum::WalletConnection {
            name: "wallet".into(),
            address: WALLET.into(),
            signing_enabled: true,
        }],
        Arc::new(gofin_evm::EvmRuntime::new(
            rpc.clone(),
            chain_id,
            Some(signer),
        )),
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(Price));
    let eth = Arc::new(eth);
    (
        CoinbaseBaseDepositAdapter::with_networks(profile, vec![eth.clone()]).unwrap(),
        api,
        rpc,
        eth,
    )
}

fn fixture_with_chain(
    address_network: Option<&str>,
    address_tag: Option<&str>,
    address: &str,
    receive_network: &str,
    chain_id: u64,
) -> (CoinbaseBaseDepositAdapter, Arc<Coinbase>, Arc<Rpc>) {
    let (adapter, api, rpc, _) = fixture_parts(
        address_network,
        address_tag,
        address,
        receive_network,
        chain_id,
    );
    (adapter, api, rpc)
}

fn fixture_with(
    address_network: Option<&str>,
    address_tag: Option<&str>,
    address: &str,
    receive_network: &str,
) -> (CoinbaseBaseDepositAdapter, Arc<Coinbase>, Arc<Rpc>) {
    fixture_with_chain(address_network, address_tag, address, receive_network, 8453)
}

fn fixture() -> (CoinbaseBaseDepositAdapter, Arc<Coinbase>, Arc<Rpc>) {
    fixture_with(Some("base"), None, DESTINATION, "base")
}

fn deposit_runtime(
    path: &std::path::Path,
    adapter: Arc<CoinbaseBaseDepositAdapter>,
    approval: Arc<DepositApproval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![adapter],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Price))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap()
}

#[tokio::test]
async fn base_deposit_waits_for_correlated_usable_credit_and_never_assumes_source_is_credit() {
    let (adapter, api, rpc) = fixture_with(Some("ethereum"), None, DESTINATION, "base");
    assert!(!adapter.authorization_identities().is_empty());
    assert!(
        adapter.snapshot_identities().is_empty(),
        "the composition must not duplicate custody observers"
    );
    let input = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-deposit","maxFeeEth":"0.001"});
    let prepared = adapter.prepare("coinbase.deposit", &input).await.unwrap();
    assert_eq!(prepared.binding["address"], DESTINATION);
    assert_eq!(
        prepared.binding["inner"]["binding"]["transaction"]["chain_id"],
        8453
    );
    assert_eq!(
        prepared.binding["inner"]["binding"]["token"]["network"],
        "ethereum:8453"
    );
    assert_eq!(
        prepared.binding["inner"]["binding"]["token"]["reference"],
        SOURCE
    );
    let fresh = adapter.revalidate(&prepared).await.unwrap();
    let validated = RevalidatedOperation::after_revalidation(
        fresh.clone(),
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    let journal = Arc::new(Journal {
        candidate: fresh.clone(),
        saved: Mutex::new(None),
    });
    let submission = adapter
        .submit_revalidated(&validated, journal.clone())
        .await
        .unwrap();
    let hash = submission.reference.clone();
    *api.hash.lock().unwrap() = Some(hash.clone());
    let record = OperationRecord {
        id: "deposit-1".into(),
        request_id: "synthetic-deposit".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: "coinbase.deposit".into(),
        input,
        status: Status::Pending,
        created_at: now(),
        updated_at: now(),
        preview: Some(fresh),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let pending = adapter.reconcile(&record).await.unwrap();
    assert_eq!(pending.status, Status::Pending);
    assert!(pending.verified.is_none());
    api.credited.store(true, Ordering::SeqCst);
    *api.hash.lock().unwrap() =
        Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into());
    assert_eq!(
        adapter.reconcile(&record).await.unwrap().status,
        Status::Pending,
        "unrelated transaction hashes cannot prove credit"
    );
    *api.hash.lock().unwrap() = Some(hash.clone());
    api.wrong_address.store(true, Ordering::SeqCst);
    assert_eq!(
        adapter.reconcile(&record).await.unwrap().status,
        Status::Pending,
        "a different recipient cannot prove credit"
    );
    api.wrong_address.store(false, Ordering::SeqCst);
    *api.receive_network.lock().unwrap() = "ethereum".into();
    assert_eq!(
        adapter.reconcile(&record).await.unwrap().status,
        Status::Pending,
        "an EVM-shared address does not make an Ethereum receive a Base settlement"
    );
    *api.receive_network.lock().unwrap() = "base".into();
    let completed = adapter.reconcile(&record).await.unwrap();
    assert_eq!(
        completed.status,
        Status::Completed,
        "{:?}",
        completed.evidence
    );
    let verified = completed.verified.unwrap();
    assert_eq!(verified.submission.reference, hash);
    assert!(verified.effects.iter().any(|effect|matches!(effect,FinancialEffect::Transfer{to,credit,..} if to==&ResourceId("coinbase:portfolio:portfolio-uuid".into()) && credit.asset.reference=="USDC" && credit.atoms.to_decimal(36)=="5")));
    assert!(rpc.submitted.load(Ordering::SeqCst));
    assert_eq!(
        rpc.submissions.load(Ordering::SeqCst),
        1,
        "reconciliation must never resubmit the source transfer"
    );
}

#[tokio::test]
async fn base_deposit_accepts_shared_evm_address_metadata_and_rejects_unsafe_metadata() {
    for network in [
        "ethereum",
        "base",
        "arbitrum",
        "optimism",
        "polygon",
        "avalanche",
        "bsc",
        "eip155:1",
        "eip155:8453",
        "eip155:18446744073709551615",
    ] {
        let (adapter, _, rpc) = fixture_with(Some(network), None, DESTINATION, "base");
        let input = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-shared-address","maxFeeEth":"0.001"});
        let prepared = adapter.prepare("coinbase.deposit", &input).await.unwrap();
        assert_eq!(
            prepared.binding["inner"]["binding"]["transaction"]["chain_id"], 8453,
            "metadata: {network}"
        );
        assert_eq!(
            prepared.binding["inner"]["binding"]["token"]["network"], "ethereum:8453",
            "metadata: {network}"
        );
        assert_eq!(
            prepared.binding["inner"]["binding"]["token"]["reference"], SOURCE,
            "metadata: {network}"
        );
        assert_eq!(prepared.binding["address"], DESTINATION);
        assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);
    }

    for (network, tag) in [
        (None, None),
        (Some("solana"), None),
        (Some("ethereum-mainnet"), None),
        (Some("eip155:0"), None),
        (Some("eip155:-1"), None),
        (Some("eip155:18446744073709551616"), None),
        (Some("eip155:1:2"), None),
        (Some("base"), Some("memo")),
    ] {
        let (adapter, _, rpc) = fixture_with(network, tag, DESTINATION, "base");
        let input = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-invalid-metadata","maxFeeEth":"0.001"});
        assert!(
            adapter.prepare("coinbase.deposit", &input).await.is_err(),
            "unsafe address metadata should fail: network={network:?}, tag={tag:?}"
        );
        assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);
    }

    for (address, account_id) in [
        ("0x1234", "usdc-account"),
        ("not-an-address", "usdc-account"),
        (DESTINATION, "another-account"),
    ] {
        let (adapter, _, rpc) = fixture_with(Some("base"), None, address, "base");
        let input = json!({"account":"coinbase","wallet":"wallet","accountId":account_id,"addressId":"base-address","amount":"5","requestId":"synthetic-invalid-address","maxFeeEth":"0.001"});
        assert!(adapter.prepare("coinbase.deposit", &input).await.is_err());
        assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);
    }

    let (adapter, _, rpc) = fixture();
    let unsupported = json!({"account":"coinbase","wallet":"wallet","network":"polygon-zkevm","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-unsupported-chain","maxFeeEth":"0.001"});
    assert!(
        adapter
            .prepare("coinbase.deposit", &unsupported)
            .await
            .is_err()
    );
    let both_fee_fields = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-conflicting-fees","maxFeeEth":"0.001","maxFeeNative":"0.001"});
    assert!(
        adapter
            .prepare("coinbase.deposit", &both_fee_fields)
            .await
            .is_err()
    );
    let missing_fee = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-missing-fee"});
    assert!(
        adapter
            .prepare("coinbase.deposit", &missing_fee)
            .await
            .is_err()
    );
    assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shared_coinbase_address_deposits_settle_only_on_the_selected_usdc_chain() {
    for (name, chain_id, token) in [
        (
            "ethereum",
            1_u64,
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        ),
        ("base", 8453, SOURCE),
        (
            "arbitrum",
            42161,
            "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
        ),
        ("optimism", 10, "0x0b2c639c533813f4aa9d7837caf62653d097ff85"),
        ("polygon", 137, "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359"),
        (
            "avalanche",
            43114,
            "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e",
        ),
        ("arc", 5042, "native"),
        (
            "hyperevm",
            999,
            "0xb88339cb7199b77e23db6e890353e22632ba630f",
        ),
    ] {
        let (adapter, api, rpc) =
            fixture_with_chain(Some("ethereum"), None, DESTINATION, name, chain_id);
        let mut input = json!({"account":"coinbase","wallet":"wallet","network":name,"accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":format!("synthetic-{name}-deposit")});
        if matches!(chain_id, 137 | 43114 | 5042 | 999) {
            input["maxFeeNative"] = json!("0.001");
        } else {
            input["maxFeeEth"] = json!("0.001");
        }
        let prepared = adapter.prepare("coinbase.deposit", &input).await.unwrap();
        assert_eq!(prepared.binding["address"], DESTINATION);
        assert_eq!(
            prepared.binding["inner"]["binding"]["transaction"]["chain_id"],
            chain_id
        );
        assert_eq!(
            prepared.binding["inner"]["binding"]["token"]["network"],
            format!("ethereum:{chain_id}")
        );
        assert_eq!(
            prepared.binding["inner"]["binding"]["token"]["reference"],
            token
        );
        assert!(
            prepared.binding["inner"]["binding"]["_gofin_usd_ceiling_v1"]["prices"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |price| price["asset"]["network"] == format!("ethereum:{chain_id}")
                        && price["asset"]["reference"] == "native"
                        && price["asset"]["decimals"] == 18
                ),
            "{name} gas must be valued in the exact 18-decimal native asset"
        );
        if chain_id == 5042 {
            assert_eq!(
                prepared.binding["inner"]["binding"]["transaction"]["value"], "5000000000000000000",
                "Arc's native-USDC principal is a native-value transfer"
            );
            let bounds = &prepared.binding["_gofin_usd_ceiling_v1"];
            assert_eq!(bounds["movement_usd_micros"], "5000000");
            assert!(
                bounds["maximum_fee_usd_micros"]
                    .as_str()
                    .and_then(|value| value.parse::<u128>().ok())
                    .unwrap_or(0)
                    > 0,
                "Arc principal and native gas fee must retain separate bounded amounts"
            );
        }

        let mut switched = prepared.clone();
        switched.input["network"] = json!(if name == "base" { "ethereum" } else { "base" });
        assert!(
            adapter.revalidate(&switched).await.is_err(),
            "approved {name} route must not switch chains"
        );
        assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);

        let fresh = adapter.revalidate(&prepared).await.unwrap();
        let validated = RevalidatedOperation::after_revalidation(
            fresh.clone(),
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap();
        let journal = Arc::new(Journal {
            candidate: fresh.clone(),
            saved: Mutex::new(None),
        });
        let submission = adapter
            .submit_revalidated(&validated, journal)
            .await
            .unwrap();
        *api.hash.lock().unwrap() = Some(submission.reference.clone());
        api.credited.store(true, Ordering::SeqCst);
        let wrong_receive = match name {
            "ethereum" => "base",
            "base" => "ethereum",
            "arbitrum" => "optimism",
            _ => "arbitrum",
        };
        *api.receive_network.lock().unwrap() = wrong_receive.to_owned();
        let record = OperationRecord {
            id: format!("deposit-{name}"),
            request_id: format!("synthetic-{name}-deposit"),
            fingerprint: "synthetic".into(),
            fingerprint_version: 1,
            operation: "coinbase.deposit".into(),
            input,
            status: Status::Pending,
            created_at: now(),
            updated_at: now(),
            preview: Some(fresh),
            approval: None,
            authorization_source: None,
            submission: Some(submission),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        assert_eq!(
            adapter.reconcile(&record).await.unwrap().status,
            Status::Pending,
            "a receive on the wrong chain cannot settle the {name} transfer"
        );
        if name == "avalanche" {
            api.receive_network_name_field.store(true, Ordering::SeqCst);
            *api.receive_network.lock().unwrap() = "avalanche-c-chain".into();
        } else {
            *api.receive_network.lock().unwrap() = name.to_owned();
        }
        let completed = adapter.reconcile(&record).await.unwrap();
        assert_eq!(
            completed.status,
            Status::Completed,
            "network={name}: {:?}",
            completed.evidence
        );
        let verified = completed
            .verified
            .expect("matching receive must produce typed settlement");
        if chain_id == 5042 {
            let transfers = verified
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    FinancialEffect::Transfer { debit, credit, .. } => Some((debit, credit)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                transfers.len(),
                1,
                "Arc's native value transfer and USDC alias log must not double-credit the deposit"
            );
            let (debit, credit) = transfers[0];
            assert_eq!(debit.asset.network, "ethereum:5042");
            assert_eq!(debit.asset.reference, "native");
            assert_eq!(debit.asset.decimals, 18);
            assert_eq!(debit.atoms.to_decimal(18), "5");
            assert_eq!(credit.asset.reference, "USDC");
            assert_eq!(credit.atoms.to_decimal(36), "5");
            assert!(
                verified.effects.iter().any(|effect| matches!(effect,
                FinancialEffect::Cost { amount, .. }
                    if amount.asset.network == "ethereum:5042"
                        && amount.asset.reference == "native"
                        && amount.asset.decimals == 18
                        && !amount.atoms.is_zero())),
                "Arc gas must remain a separate native-asset source cost"
            );
        }
        assert_eq!(
            rpc.submissions.load(Ordering::SeqCst),
            1,
            "reconciliation on {name} must not replay the source transfer"
        );
    }
}

#[tokio::test]
async fn explicit_erc20_contract_requires_matching_currency_and_contract_metadata() {
    const DAI: &str = "0x2222222222222222222222222222222222222222";
    let (adapter, api, rpc) = fixture_with_chain(Some("base"), None, DESTINATION, "base", 8453);
    *api.account_currency.lock().unwrap() = "DAI".into();
    *api.receive_currency.lock().unwrap() = "DAI".into();
    *rpc.source.lock().unwrap() = DAI.into();
    *rpc.symbol.lock().unwrap() = "DAI".into();
    rpc.token_decimals.store(18, Ordering::SeqCst);

    let base = json!({"account":"coinbase","wallet":"wallet","network":"base","currency":"DAI","token":DAI,"accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-dai-deposit","maxFeeNative":"0.001"});
    let prepared = adapter.prepare("coinbase.deposit", &base).await.unwrap();
    assert_eq!(
        prepared.binding["inner"]["binding"]["token"]["reference"],
        DAI
    );
    assert_eq!(
        prepared.binding["inner"]["binding"]["token"]["decimals"],
        18
    );

    let (wrong_network, wrong_network_api, wrong_network_rpc) =
        fixture_with_chain(Some("ethereum"), None, DESTINATION, "base", 8453);
    *wrong_network_api.account_currency.lock().unwrap() = "DAI".into();
    let mut wrong_network_input = base.clone();
    wrong_network_input["requestId"] = json!("synthetic-dai-wrong-address-network");
    assert_eq!(
        wrong_network
            .prepare("coinbase.deposit", &wrong_network_input)
            .await
            .unwrap_err()
            .code,
        "COINBASE_DEPOSIT_NETWORK_UNVERIFIED"
    );
    assert_eq!(
        wrong_network_rpc.submissions.load(Ordering::SeqCst),
        0,
        "a non-USDC contract must not use the shared-network-label exception"
    );

    let mut switched_token = prepared.clone();
    switched_token.input["token"] = json!("0x3333333333333333333333333333333333333333");
    assert!(
        adapter.revalidate(&switched_token).await.is_err(),
        "token contract is part of the approved binding"
    );
    rpc.token_decimals.store(6, Ordering::SeqCst);
    assert!(
        adapter.revalidate(&prepared).await.is_err(),
        "changed ERC-20 decimals invalidate the preview"
    );
    rpc.token_decimals.store(18, Ordering::SeqCst);

    let mut missing_token = base.clone();
    missing_token.as_object_mut().unwrap().remove("token");
    assert!(
        adapter
            .prepare("coinbase.deposit", &missing_token)
            .await
            .is_err()
    );
    let mut wrong_venue_currency = base.clone();
    wrong_venue_currency["currency"] = json!("DAI");
    *api.account_currency.lock().unwrap() = "USDC".into();
    assert!(
        adapter
            .prepare("coinbase.deposit", &wrong_venue_currency)
            .await
            .is_err()
    );
    *api.account_currency.lock().unwrap() = "DAI".into();
    assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);

    let fresh = adapter.revalidate(&prepared).await.unwrap();
    let validated = RevalidatedOperation::after_revalidation(
        fresh.clone(),
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    let journal = Arc::new(Journal {
        candidate: fresh.clone(),
        saved: Mutex::new(None),
    });
    let submission = adapter
        .submit_revalidated(&validated, journal)
        .await
        .unwrap();
    *api.hash.lock().unwrap() = Some(submission.reference.clone());
    api.credited.store(true, Ordering::SeqCst);
    *api.receive_currency.lock().unwrap() = "USDC".into();
    let record = OperationRecord {
        id: "deposit-dai".into(),
        request_id: "synthetic-dai-deposit".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: "coinbase.deposit".into(),
        input: base,
        status: Status::Pending,
        created_at: now(),
        updated_at: now(),
        preview: Some(fresh),
        approval: None,
        authorization_source: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    assert_eq!(
        adapter.reconcile(&record).await.unwrap().status,
        Status::Pending,
        "USDC receive evidence cannot settle a DAI deposit"
    );
    *api.receive_currency.lock().unwrap() = "DAI".into();
    let completed = adapter.reconcile(&record).await.unwrap();
    assert_eq!(
        completed.status,
        Status::Completed,
        "{:?}",
        completed.evidence
    );
    let verified = completed.verified.unwrap();
    assert!(verified.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::Transfer { debit, credit, .. }
            if debit.asset.reference.eq_ignore_ascii_case(DAI)
                && debit.asset.decimals == 18
                && debit.atoms.to_decimal(18) == "5"
                && credit.asset.reference == "DAI"
                && credit.asset.decimals == 36
                && credit.atoms.to_decimal(36) == "5")));
    assert_eq!(
        rpc.submissions.load(Ordering::SeqCst),
        1,
        "DAI reconciliation must never replay the source transfer"
    );
}

#[tokio::test]
async fn eurc_uses_only_canonical_tokens_on_its_four_supported_chains() {
    for (name, chain_id, token) in [
        (
            "ethereum",
            1_u64,
            "0x1abaea1f7c830bd89acc67ec4af516284b1bc33c",
        ),
        ("base", 8453, "0x60a3e35cc302bfa44cb288bc5a4f316fdb1adb42"),
        (
            "avalanche",
            43114,
            "0xc891eb4cbdeff6e073e859e987815ed1505c2acd",
        ),
        ("arc", 5042, "0xbef5f6d51cb62b58e6a8f77868681825c6fe21c1"),
    ] {
        let (adapter, api, rpc) =
            fixture_with_chain(Some("ethereum"), None, DESTINATION, name, chain_id);
        *api.account_currency.lock().unwrap() = "EURC".into();
        *api.receive_currency.lock().unwrap() = "EURC".into();
        *rpc.source.lock().unwrap() = token.into();
        *rpc.symbol.lock().unwrap() = "EURC".into();
        rpc.token_decimals.store(6, Ordering::SeqCst);
        let fee_field = if matches!(chain_id, 1 | 8453) {
            "maxFeeEth"
        } else {
            "maxFeeNative"
        };
        let mut input = json!({"account":"coinbase","wallet":"wallet","network":name,"currency":"EURC","token":token,"accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":format!("synthetic-{name}-eurc")});
        input[fee_field] = json!("0.001");
        let prepared = adapter.prepare("coinbase.deposit", &input).await.unwrap();
        assert_eq!(
            prepared.binding["inner"]["binding"]["transaction"]["chain_id"],
            chain_id
        );
        assert_eq!(
            prepared.binding["inner"]["binding"]["token"]["network"],
            format!("ethereum:{chain_id}")
        );
        assert_eq!(
            prepared.binding["inner"]["binding"]["token"]["reference"],
            token
        );
        assert_eq!(prepared.binding["inner"]["binding"]["token"]["decimals"], 6);
        assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);
    }

    let (adapter, api, rpc) = fixture();
    for input in [
        json!({"account":"coinbase","wallet":"wallet","network":"base","currency":"USDC","token":"0x2222222222222222222222222222222222222222","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-noncanonical-usdc-token","maxFeeEth":"0.001"}),
        json!({"account":"coinbase","wallet":"wallet","network":"base","currency":"EURC","token":"0x1abaea1f7c830bd89acc67ec4af516284b1bc33c","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-wrong-chain-eurc-token","maxFeeEth":"0.001"}),
    ] {
        *api.account_currency.lock().unwrap() = input["currency"].as_str().unwrap().into();
        let error = adapter
            .prepare("coinbase.deposit", &input)
            .await
            .unwrap_err();
        assert!(
            error.message.contains("canonical network token"),
            "{error:?}"
        );
    }
    assert_eq!(
        rpc.submissions.load(Ordering::SeqCst),
        0,
        "USDC and EURC symbols cannot redirect a transfer to an arbitrary ERC-20"
    );
    let (arc, api, rpc) = fixture_with_chain(Some("ethereum"), None, DESTINATION, "arc", 5042);
    *api.account_currency.lock().unwrap() = "EURC".into();
    let input = json!({"account":"coinbase","wallet":"wallet","network":"arc","currency":"EURC","token":ARC_USDC_ALIAS,"accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-eurc-cannot-use-arc-usdc-alias","maxFeeNative":"0.001"});
    let error = arc.prepare("coinbase.deposit", &input).await.unwrap_err();
    assert!(
        error.message.contains("canonical network token"),
        "{error:?}"
    );
    assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);

    let (unsupported, api, rpc) =
        fixture_with_chain(Some("ethereum"), None, DESTINATION, "arbitrum", 42161);
    *api.account_currency.lock().unwrap() = "EURC".into();
    let input = json!({"account":"coinbase","wallet":"wallet","network":"arbitrum","currency":"EURC","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-eurc-unsupported-chain","maxFeeNative":"0.001"});
    let error = unsupported
        .prepare("coinbase.deposit", &input)
        .await
        .unwrap_err();
    assert_eq!(error.code, "COINBASE_DEPOSIT_ASSET_UNAVAILABLE");
    assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_deposit_runs_through_broker_cli_and_recovers_settlement_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("coinbase-deposit.sqlite");
    let (_, api, rpc, ethereum) = fixture_parts(Some("ethereum"), None, DESTINATION, "base", 8453);
    let http = coinbase_http_fixture::Fixture::new(api.clone()).await;
    let adapter = Arc::new(
        CoinbaseBaseDepositAdapter::with_networks(
            deposit_profile(http.api.clone()),
            vec![ethereum],
        )
        .unwrap(),
    );
    let approval = Arc::new(DepositApproval(std::sync::atomic::AtomicUsize::new(0)));
    let client = Client::new(
        deposit_runtime(&store_path, adapter.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let input = json!({"account":"coinbase","wallet":"wallet","network":"base","accountId":"usdc-account","addressId":"base-address","amount":"2","requestId":"synthetic-base-cli-deposit","maxFeeEth":"0.001"});
    let request_id = input["requestId"].as_str().unwrap().to_owned();
    let request = OperationRequest {
        request_id: request_id.clone(),
        operation: "coinbase.deposit".into(),
        input: input.clone(),
        run_id: None,
    };
    let started = client
        .call(
            vec![
                "execute".into(),
                "coinbase.deposit".into(),
                "--input".into(),
                input.to_string(),
                "--id".into(),
                request_id,
                "--json".into(),
            ],
            BrokerRequest::Execute { request },
            true,
        )
        .await;
    assert_eq!(
        started["status"], "pending",
        "deposit must wait for Coinbase credit: {started}"
    );
    let id = started["id"].as_str().unwrap().to_owned();
    assert_eq!(rpc.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(
        approval.0.load(Ordering::SeqCst),
        1,
        "one standalone exact-scope approval must authorize the write"
    );

    api.credited.store(true, Ordering::SeqCst);
    *api.hash.lock().unwrap() = Some(rpc.hash.lock().unwrap().clone().unwrap());
    api.incoming_send.store(true, Ordering::SeqCst);
    api.include_to.store(false, Ordering::SeqCst);
    api.receive_network_name_field.store(true, Ordering::SeqCst);
    *api.receive_amount.lock().unwrap() = "2.000000".into();
    *api.credit_balance.lock().unwrap() = "2.000000".into();
    *api.receive_network.lock().unwrap() = "ethereum".into();
    assert_eq!(
        client.status(&id).await["status"],
        "pending",
        "shared address metadata cannot turn the wrong-chain receive into settlement"
    );
    drop(client);

    let recovered = Client::new(
        deposit_runtime(&store_path, adapter, approval.clone()),
        temp.path(),
    )
    .await;
    assert_eq!(
        recovered.status(&id).await["status"],
        "pending",
        "wrong-chain pending identity must survive reopening the durable store"
    );
    *api.receive_network.lock().unwrap() = "base".into();
    *rpc.receipt_recipient.lock().unwrap() = "0x2222222222222222222222222222222222222222".into();
    assert_eq!(
        recovered.status(&id).await["status"],
        "pending",
        "missing Coinbase recipient requires exact canonical source recipient evidence"
    );
    *rpc.receipt_recipient.lock().unwrap() = DESTINATION.into();
    api.include_to.store(true, Ordering::SeqCst);
    api.wrong_address.store(true, Ordering::SeqCst);
    assert_eq!(
        recovered.status(&id).await["status"],
        "pending",
        "an explicit wrong Coinbase recipient must reject the incoming send"
    );
    api.wrong_address.store(false, Ordering::SeqCst);
    api.include_to.store(false, Ordering::SeqCst);
    api.wrong_receive_hash.store(true, Ordering::SeqCst);
    assert_eq!(recovered.status(&id).await["status"], "pending");
    api.wrong_receive_hash.store(false, Ordering::SeqCst);
    for amount in ["-2.000000", "0", "1.999999"] {
        *api.receive_amount.lock().unwrap() = amount.into();
        assert_eq!(
            recovered.status(&id).await["status"],
            "pending",
            "invalid or incorrect receive amount {amount}"
        );
    }
    *api.receive_amount.lock().unwrap() = "2.000000".into();
    *api.receive_currency.lock().unwrap() = "EURC".into();
    assert_eq!(recovered.status(&id).await["status"], "pending");
    *api.receive_currency.lock().unwrap() = "USDC".into();
    *api.receive_network.lock().unwrap() = "ethereum".into();
    assert_eq!(recovered.status(&id).await["status"], "pending");
    *api.receive_network.lock().unwrap() = "base".into();
    *api.receive_status.lock().unwrap() = "pending".into();
    assert_eq!(recovered.status(&id).await["status"], "pending");
    *api.receive_status.lock().unwrap() = "completed".into();
    let settled = recovered.status(&id).await;
    assert_eq!(settled["status"], "completed", "{settled}");
    let record: OperationRecord = serde_json::from_value(settled).unwrap();
    let effects = record
        .effects
        .expect("completed result must persist typed effects");
    assert!(effects.effects.iter().any(|effect| matches!(effect,
        FinancialEffect::Transfer { to, debit, credit, .. }
            if to == &ResourceId("coinbase:portfolio:portfolio-uuid".into())
                && debit.asset.reference == SOURCE
                && debit.atoms.to_decimal(6) == "2"
                && credit.asset.reference == "USDC"
                && credit.atoms.to_decimal(36) == "2")));
    assert_eq!(
        rpc.submissions.load(Ordering::SeqCst),
        1,
        "durable recovery and settlement must not replay the Base source transfer"
    );
    assert_eq!(
        approval.0.load(Ordering::SeqCst),
        1,
        "recovery must reuse the original exact-scope approval"
    );
    http.assert_dispatched();
    let calls = http.calls.lock().unwrap();
    assert!(calls.iter().any(|call| call.path.contains("/addresses")));
    assert!(calls.iter().any(|call| call.path.contains("/transactions")));
    assert!(calls.iter().all(|call| !call.mutating));
    assert!(calls.iter().all(|call| call.body.is_empty()));
}

#[tokio::test]
async fn base_deposit_requires_explicit_native_account_readiness_before_source_dispatch() {
    for field in ["active", "ready"] {
        for invalid in [Value::Null, json!(false), json!("true"), json!(1)] {
            let (adapter, api, rpc) = fixture();
            *api.invalid_account_flag.lock().unwrap() = Some((field.into(), invalid));
            let input = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-unready-deposit","maxFeeEth":"0.001"});
            assert_eq!(
                adapter
                    .prepare("coinbase.deposit", &input)
                    .await
                    .unwrap_err()
                    .code,
                "COINBASE_DEPOSIT_ACCOUNT_INVALID"
            );
            assert_eq!(rpc.submissions.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn deposit_revalidation_preserves_legacy_fin_binding_and_rejects_duplicate_aliases() {
    let (adapter, _, _) = fixture();
    let input = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-legacy-deposit","maxFeeEth":"0.001"});
    let mut prepared = adapter.prepare("coinbase.deposit", &input).await.unwrap();
    let ceiling = prepared
        .binding
        .as_object_mut()
        .unwrap()
        .remove("_gofin_usd_ceiling_v1")
        .unwrap();
    prepared.binding["_fin_usd_ceiling_v1"] = ceiling.clone();
    let original = prepared.binding.clone();
    let refreshed = adapter.revalidate(&prepared).await.unwrap();
    assert_eq!(
        refreshed.binding, original,
        "approved legacy bytes remain unchanged"
    );
    prepared.binding["_gofin_usd_ceiling_v1"] = ceiling;
    let error = adapter.revalidate(&prepared).await.unwrap_err();
    assert_eq!(error.code, "COINBASE_DEPOSIT_BINDING_INVALID");
}
