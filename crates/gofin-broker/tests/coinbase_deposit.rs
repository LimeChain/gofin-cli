use async_trait::async_trait;
use gofin_broker::coinbase_deposit::CoinbaseBaseDepositAdapter;
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use gofin_domain::*;
use gofin_evm::{EnvelopeSigner, LocalEnvelopeSigner, alloy::primitives::Bytes};
use gofin_exchange::coinbase::*;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const SOURCE: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const WALLET: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const DESTINATION: &str = "0x1111111111111111111111111111111111111111";

struct Coinbase {
    credited: AtomicBool,
    wrong_address: AtomicBool,
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
                json!({"data":[{"id":"usdc-account","currency":{"code":"USDC"}}],"pagination":{}})
            }
            CoinbaseRoute::Account { account_id } => {
                assert_eq!(account_id, "usdc-account");
                {
                    let mut value = json!({"account":{"uuid":"usdc-account","currency":"USDC","active":true,"ready":true,"available_balance":{"value":if self.credited.load(Ordering::SeqCst){"5.000000"}else{"0.000000"},"currency":"USDC"}}});
                    if let Some((field, invalid)) =
                        self.invalid_account_flag.lock().unwrap().as_ref()
                    {
                        value["account"][field] = invalid.clone();
                    }
                    value
                }
            }
            CoinbaseRoute::DepositAddresses { account_id } => {
                assert_eq!(account_id, "usdc-account");
                json!({"data":[{"id":"base-address","address":DESTINATION,"network":"base"}],"pagination":{}})
            }
            CoinbaseRoute::Transactions { account_id, .. } => {
                assert_eq!(account_id, "usdc-account");
                let rows = if self.credited.load(Ordering::SeqCst) {
                    vec![
                        json!({"id":"receive-1","type":"receive","status":"completed","amount":{"amount":"5.000000","currency":"USDC"},"network":{"name":"base","hash":self.hash.lock().unwrap().clone().unwrap_or_default()},"to":{"address":if self.wrong_address.load(Ordering::SeqCst){"0x2222222222222222222222222222222222222222"}else{DESTINATION}}}),
                    ]
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
    transaction: Mutex<Option<gofin_evm::FrozenTransaction>>,
    hash: Mutex<Option<String>>,
    submitted: AtomicBool,
    submissions: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl gofin_evm::EvmRpc for Rpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let result = match method {
            "eth_chainId" => json!("0x2105"),
            "eth_getCode" => json!("0x6000"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_getTransactionCount" => json!("0x0"),
            "eth_estimateGas" => json!("0x10000"),
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_blockNumber" => json!("0x65"),
            "eth_getBlockByNumber" => {
                json!({"number":if params[0]=="latest" {"0x66"} else {params[0].as_str().unwrap_or("0x64")},"hash":format!("0x{}","cd".repeat(32)),"baseFeePerGas":"0x3b9aca00","extraData":format!("0x00{}","0".repeat(16))})
            }
            "eth_call" => {
                let data = params[0]["data"]
                    .as_str()
                    .or_else(|| params[0]["input"].as_str())
                    .unwrap_or("");
                if data.starts_with("0x313ce567") {
                    json!(format!("0x{:064x}", 6))
                } else if data.starts_with("0x70a08231") {
                    json!(format!("0x{:064x}", 50_000_000_u64))
                } else if data.starts_with("0x95d89b41") || data.starts_with("0x06fdde03") {
                    json!(format!(
                        "0x{}20{}04{}{}",
                        "0".repeat(62),
                        "0".repeat(62),
                        "55534443",
                        "0".repeat(56)
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
                json!({"hash":self.hash.lock().unwrap().clone().unwrap_or_default(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","from":tx.sender,"to":tx.to,"chainId":"0x2105","type":"0x2","nonce":"0x0","gas":format!("{:#x}",tx.gas),"value":format!("{:#x}",gofin_evm::u256(&tx.value)?),"input":tx.calldata,"maxFeePerGas":format!("{:#x}",gofin_evm::u256(&tx.max_fee_per_gas)?),"maxPriorityFeePerGas":format!("{:#x}",gofin_evm::u256(&tx.max_priority_fee_per_gas)?),"accessList":[]})
            }
            "eth_getTransactionReceipt" => {
                if self.submitted.load(Ordering::SeqCst) {
                    json!({"transactionHash":self.hash.lock().unwrap().clone().unwrap_or_default(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x10000","effectiveGasPrice":"0x3b9aca00","l1Fee":"0x0","logs":[{"address":SOURCE,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",format!("0x{}{}","0".repeat(24),WALLET.trim_start_matches("0x")),format!("0x{}{}","0".repeat(24),DESTINATION.trim_start_matches("0x"))],"data":format!("0x{:064x}",5_000_000_u64),"transactionHash":self.hash.lock().unwrap().clone().unwrap_or_default(),"blockHash":format!("0x{}","cd".repeat(32)),"blockNumber":"0x64","logIndex":"0x0","removed":false}]})
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
    async fn quote_usd(&self, _: &AssetId, units: Quantity) -> Result<(Quantity, String)> {
        Ok((units, "synthetic Base deposit fixture".into()))
    }
}
fn fixture() -> (CoinbaseBaseDepositAdapter, Arc<Coinbase>, Arc<Rpc>) {
    let rpc = Arc::new(Rpc {
        transaction: Mutex::new(None),
        hash: Mutex::new(None),
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
        invalid_account_flag: Mutex::new(None),
        hash: Mutex::new(None),
    });
    let profile = Arc::new(
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
            std::collections::BTreeMap::from([(
                "synthetic".into(),
                api.clone() as Arc<dyn CoinbaseApi>,
            )]),
        )
        .unwrap(),
    );
    let eth = gofin_ethereum::EthereumAdapter::new(
        gofin_ethereum::EthereumConfig::wallet_only(8453, 2, vec![]),
        vec![gofin_ethereum::WalletConnection {
            name: "wallet".into(),
            address: WALLET.into(),
            signing_enabled: true,
        }],
        Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 8453, Some(signer))),
    )
    .unwrap()
    .with_wallet_valuation(Arc::new(Price));
    (
        CoinbaseBaseDepositAdapter::new(profile, Arc::new(eth)).unwrap(),
        api,
        rpc,
    )
}

#[tokio::test]
async fn base_deposit_waits_for_correlated_usable_credit_and_never_assumes_source_is_credit() {
    let (adapter, api, rpc) = fixture();
    assert!(!adapter.authorization_identities().is_empty());
    assert!(
        adapter.snapshot_identities().is_empty(),
        "the composition must not duplicate custody observers"
    );
    let input = json!({"account":"coinbase","wallet":"wallet","accountId":"usdc-account","addressId":"base-address","amount":"5","requestId":"synthetic-deposit","maxFeeEth":"0.001"});
    let prepared = adapter.prepare("coinbase.deposit", &input).await.unwrap();
    assert_eq!(prepared.binding["address"], DESTINATION);
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
