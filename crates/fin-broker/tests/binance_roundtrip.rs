//! Synthetic end-to-end Binance SPOT transfer run. This fixture never builds a
//! production Binance client and accepts no environment-provided endpoints.
use async_trait::async_trait;
use fin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint,
    BinancePriceProvider, BinanceRequest, BinanceTransport, CredentialHandle, PriceQuote,
    TransportError,
};
use fin_broker::{
    AssetValuator, BrokerConfigV2, BrokerPolicy, ConnectionConfig, RuntimeDependencies,
    compose_runtime,
};
use fin_domain::{
    Adapter, Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, FinError,
    OperationRequest, PlanStep, Quantity, ResourceAmount, ResourceId, Result, now,
};
use fin_evm::EvmRpc;
use fin_protocol::{BrokerRequest, RunBeginRequest};
use fin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

const MAIN: &str = "main";
const CHILD_A: &str = "child-a";
const CHILD_B: &str = "child-b";
const EMAIL_A: &str = "a@example.invalid";
const EMAIL_B: &str = "b@example.invalid";
const MASTER_EMAIL: &str = "synthetic-master@example.invalid";
const ASSET: &str = "ETH";
const STEP_A_IN: &str = "a-to-main";
const STEP_B_IN: &str = "b-to-main";
const STEP_B_OUT: &str = "main-to-b";
const STEP_A_OUT: &str = "main-to-a";

fn resource(uid: u64) -> ResourceId {
    ResourceId(format!("binance:uid:{uid}:SPOT"))
}
fn asset() -> AssetId {
    AssetId {
        network: "binance".into(),
        reference: ASSET.into(),
        decimals: 36,
    }
}
fn transfer(id: &str, from: &str, to: &str, amount: &str) -> Value {
    json!({"requestId":id,"authority":MAIN,"from":from,"to":to,"coin":ASSET,"amount":amount,"fromWallet":"SPOT","toWallet":"SPOT"})
}
fn step(id: &str, from: &str, to: &str, amount: &str) -> PlanStep {
    PlanStep {
        operation: "binance.transfer".into(),
        input: transfer(id, from, to, amount),
        maximum_fee_usd_micros: "0".into(),
        description: format!("Synthetic bounded transfer {id}"),
    }
}

#[derive(Default)]
struct VenueState {
    balances: BTreeMap<String, String>,
    history: Vec<Value>,
    deposit_history: Vec<Value>,
    withdrawal_history: Vec<Value>,
    requests: Vec<(BinanceEndpoint, Vec<(String, String)>)>,
    posts: usize,
    reject_request_id: Option<String>,
    rate_limit_request_id: Option<String>,
    accept_then_timeout: Option<String>,
    restrictions: Option<bool>,
    withdraw_fee: String,
}
struct SyntheticVenue {
    state: Mutex<VenueState>,
    wallet_balance: Mutex<Option<Arc<Mutex<String>>>>,
}
impl SyntheticVenue {
    fn new() -> Self {
        Self {
            state: Mutex::new(VenueState {
                balances: BTreeMap::from([
                    ("main".into(), "0".into()),
                    (EMAIL_A.into(), "0.005".into()),
                    (EMAIL_B.into(), "0.010".into()),
                ]),
                restrictions: Some(true),
                withdraw_fee: "0.001".into(),
                ..VenueState::default()
            }),
            wallet_balance: Mutex::new(None),
        }
    }
    fn free(s: &VenueState, key: &str) -> String {
        s.balances.get(key).cloned().unwrap_or_else(|| "0".into())
    }
    fn alter(s: &mut VenueState, key: &str, delta: i64) {
        // All amounts in this fixture are exact multiples of 0.001 ETH.
        let current = milli(&Self::free(s, key));
        let next = current
            .checked_add(delta)
            .expect("synthetic balance arithmetic");
        assert!(next >= 0);
        s.balances
            .insert(key.into(), format!("{}.{:03}", next / 1000, next % 1000));
    }
    fn record_deposit(&self, tx_hash: &str, amount: &str) {
        let mut state = self.state.lock().unwrap();
        let milli_amount = milli(amount);
        Self::alter(&mut state, "main", milli_amount);
        state.deposit_history.push(json!({
            "id":"synthetic-deposit-row-1",
            "coin":ASSET,
            "amount":amount,
            "transactionFee":"0",
            "network":"ETH",
            "address":"0x1111111111111111111111111111111111111111",
            "addressTag":"",
            "txId":tx_hash,
            "status":1,
            "insertTime":chrono::Utc::now().timestamp_millis() as u64,
            "confirmTimes":"12/12"
        }));
    }
    fn owner(credential: &CredentialHandle) -> &'static str {
        match credential.as_str() {
            "test-only:main" => "main",
            "test-only:a" => EMAIL_A,
            "test-only:b" => EMAIL_B,
            _ => panic!("unexpected synthetic credential"),
        }
    }
    fn params(request: &BinanceRequest) -> BTreeMap<String, String> {
        request
            .params
            .iter()
            .map(|p| (p.key.clone(), p.value.clone()))
            .collect()
    }
}

#[async_trait]
impl BinanceTransport for SyntheticVenue {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        use BinanceEndpoint as E;
        let mut s = self.state.lock().unwrap();
        let params = Self::params(&request);
        s.requests.push((
            request.endpoint,
            params.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ));
        let owner = Self::owner(credential);
        match request.endpoint {
            E::Account => {
                let uid = match owner {
                    "main" => 1001,
                    EMAIL_A => 1002,
                    _ => 1003,
                };
                Ok(
                    json!({"uid":uid,"accountType":"SPOT","balances":[{"asset":ASSET,"free":Self::free(&s, owner),"locked":"0"}]}),
                )
            }
            E::SubAccountSpotAssets => {
                let email = params.get("email").ok_or(TransportError::InvalidResponse)?;
                Ok(json!({"balances":[{"asset":ASSET,"free":Self::free(&s,email),"locked":"0"}]}))
            }
            E::ApiRestrictions => Ok(
                json!({"enableInternalTransfer":s.restrictions,"permitsUniversalTransfer":s.restrictions,"enableWithdrawals":true}),
            ),
            E::SubAccountList => Ok(
                json!({"subAccounts":[{"email":EMAIL_A,"isFreeze":false,"subAccountId":1002},{"email":EMAIL_B,"isFreeze":false,"subAccountId":1003}]}),
            ),
            E::SubAccountTransferHistory => {
                let direction = params.get("type").ok_or(TransportError::InvalidResponse)?;
                let start = params
                    .get("startTime")
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or(TransportError::InvalidResponse)?;
                let end = params
                    .get("endTime")
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or(TransportError::InvalidResponse)?;
                let limit = params
                    .get("limit")
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or(TransportError::InvalidResponse)?;
                let rows = s.history.iter().filter_map(|row| {
                    let child_sends = row["fromEmail"] == owner && row["toEmail"] == MASTER_EMAIL;
                    let child_receives = row["toEmail"] == owner && row["fromEmail"] == MASTER_EMAIL;
                    let kind = if child_sends { 2 } else if child_receives { 1 } else { return None; };
                    let time = row["createTimeStamp"].as_u64()?;
                    if kind.to_string() != *direction || time < start || time > end {
                        return None;
                    }
                    Some(json!({
                        "counterParty":"master", "email":MASTER_EMAIL, "type":kind,
                        "asset":row["asset"], "qty":row["amount"],
                        "fromAccountType":row["fromAccountType"], "toAccountType":row["toAccountType"],
                        "status":row["status"], "tranId":row["tranId"], "time":time
                    }))
                }).take(limit).collect::<Vec<_>>();
                Ok(json!(rows))
            }
            E::CapitalConfig => Ok(
                json!([{"coin":ASSET,"depositAllEnable":true,"withdrawAllEnable":true,"networkList":[{"network":"ETH","coin":ASSET,"withdrawIntegerMultiple":"0.001","withdrawFee":s.withdraw_fee,"withdrawMin":"0.001","withdrawMax":"100","depositDust":"0.001","withdrawEnable":true,"depositEnable":true,"minConfirm":12,"unLockConfirm":64}]}]),
            ),
            E::DepositAddress => Ok(
                json!({"coin":ASSET,"network":"ETH","address":"0x1111111111111111111111111111111111111111","tag":""}),
            ),
            E::WithdrawAddressList => Ok(json!([])),
            E::DepositHistory => {
                let records = s
                    .deposit_history
                    .iter()
                    .filter(|row| {
                        params.get("txId").is_none_or(|tx| {
                            row["txId"]
                                .as_str()
                                .is_some_and(|value| value.eq_ignore_ascii_case(tx))
                        })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(json!(records))
            }
            E::WithdrawalHistory => {
                let filtered = s
                    .withdrawal_history
                    .iter()
                    .filter(|row| {
                        params
                            .get("withdrawOrderId")
                            .is_none_or(|id| row["withdrawOrderId"] == *id)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                Ok(json!(filtered))
            }
            E::TravelRuleRequirements => Ok(json!({"questionnaireCountryCode":"NIL"})),
            E::StandardWithdrawal => {
                if owner != "main" || s.restrictions != Some(true) {
                    return Err(TransportError::PermissionDenied);
                }
                s.posts += 1;
                let order_id = params
                    .get("withdrawOrderId")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                let amount = params
                    .get("amount")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                let fee = s.withdraw_fee.clone();
                let gross = milli(&amount) + milli(&fee);
                if milli(&Self::free(&s, "main")) < gross {
                    return Err(TransportError::Rejected);
                }
                Self::alter(&mut s, "main", -gross);
                if let Some(wallet_balance) = self.wallet_balance.lock().unwrap().as_ref() {
                    adjust_native_balance(wallet_balance, &amount, true);
                }
                let hash = format!("0x{}", "ee".repeat(32));
                let address = params
                    .get("address")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                s.withdrawal_history.push(json!({"id":"synthetic-withdrawal","withdrawOrderId":order_id,"coin":"ETH","network":"ETH","address":address,"amount":amount,"transactionFee":fee,"status":6,"txId":hash,"transferType":0,"walletType":0,"applyTime":chrono::Utc::now().to_rfc3339()}));
                Ok(json!({"id":"synthetic-withdrawal","withdrawOrderId":order_id}))
            }
            E::UniversalTransferHistory => {
                let filtered: Vec<_> = s
                    .history
                    .iter()
                    .filter(|h| {
                        if let Some(id) = params.get("clientTranId") {
                            h["clientTranId"] == *id
                        } else if let Some(email) = params.get("fromEmail") {
                            h["fromEmail"] == *email
                        } else {
                            h["fromEmail"].is_null()
                        }
                    })
                    .cloned()
                    .collect();
                Ok(json!({"totalCount":filtered.len(),"result":filtered}))
            }
            E::UniversalTransfer => {
                if owner != "main" {
                    return Err(TransportError::PermissionDenied);
                }
                s.posts += 1;
                let id = params
                    .get("clientTranId")
                    .cloned()
                    .ok_or(TransportError::InvalidResponse)?;
                if s.rate_limit_request_id.as_deref() == Some(id.as_str()) {
                    return Err(TransportError::RateLimited);
                }
                let from_email = params.get("fromEmail").cloned();
                let to_email = params.get("toEmail").cloned();
                if s.reject_request_id.as_deref() == Some(id.as_str()) {
                    let tran = s.posts.to_string();
                    s.history.push(json!({"tranId":tran,"clientTranId":id,"asset":ASSET,"amount":params.get("amount").ok_or(TransportError::InvalidResponse)?,"fromAccountType":"SPOT","toAccountType":"SPOT","fromEmail":from_email.as_deref().unwrap_or(MASTER_EMAIL),"toEmail":to_email.as_deref().unwrap_or(MASTER_EMAIL),"status":"FAILED","createTimeStamp":chrono::Utc::now().timestamp_millis() as u64}));
                    return Err(TransportError::Rejected);
                }
                let from = from_email.as_deref().unwrap_or("main");
                let to = to_email.as_deref().unwrap_or("main");
                let amount = milli(
                    params
                        .get("amount")
                        .ok_or(TransportError::InvalidResponse)?,
                );
                if milli(&Self::free(&s, from)) < amount {
                    return Err(TransportError::Rejected);
                }
                let millis = amount;
                Self::alter(&mut s, from, -millis);
                Self::alter(&mut s, to, millis);
                let tran = s.posts.to_string();
                let row = json!({"tranId":tran,"clientTranId":id,"asset":ASSET,"amount":params["amount"],"fromAccountType":"SPOT","toAccountType":"SPOT","fromEmail":from_email.as_deref().unwrap_or(MASTER_EMAIL),"toEmail":to_email.as_deref().unwrap_or(MASTER_EMAIL),"status":"SUCCESS","createTimeStamp":chrono::Utc::now().timestamp_millis() as u64});
                s.history.push(row);
                if s.accept_then_timeout.as_deref() == Some(id.as_str()) {
                    s.accept_then_timeout = None;
                    return Err(TransportError::Timeout);
                }
                Ok(json!({"tranId":tran,"clientTranId":id}))
            }
        }
    }
}
fn milli(value: &str) -> i64 {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    assert!(fraction.len() <= 3 && fraction.bytes().all(|b| b.is_ascii_digit()));
    whole.parse::<i64>().unwrap() * 1000
        + fraction.parse::<i64>().unwrap_or(0) * 10_i64.pow((3 - fraction.len()) as u32)
}

fn adjust_native_balance(balance: &Arc<Mutex<String>>, amount: &str, increase: bool) {
    let current = Quantity::from_decimal(&balance.lock().unwrap(), 18).unwrap();
    let delta = Quantity::from_decimal(amount, 18).unwrap();
    let next = if increase {
        current.checked_add(&delta).unwrap()
    } else {
        current.checked_sub(&delta).unwrap()
    };
    *balance.lock().unwrap() = next.to_decimal(18);
}

struct SyntheticApproval(AtomicUsize);
#[async_trait]
impl Authorizer for SyntheticApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic-fixture-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct SyntheticBinancePrice;
#[async_trait]
impl BinancePriceProvider for SyntheticBinancePrice {
    async fn quote_usd(&self, coin: &str) -> std::result::Result<PriceQuote, FinError> {
        if coin != ASSET {
            return Err(FinError::new(
                "PRICE_UNAVAILABLE",
                "test",
                "Unknown synthetic asset.",
                "Use the fixed ETH fixture.",
            ));
        }
        Ok(PriceQuote {
            usd_per_unit: "2000".into(),
            observed_at: now(),
            source: "synthetic-fixture:ETH-USD-2000".into(),
        })
    }
}
struct StaleBinancePrice;
#[async_trait]
impl BinancePriceProvider for StaleBinancePrice {
    async fn quote_usd(&self, coin: &str) -> std::result::Result<PriceQuote, FinError> {
        if coin != ASSET {
            return Err(FinError::validation("Unknown synthetic asset."));
        }
        Ok(PriceQuote {
            usd_per_unit: "2000".into(),
            observed_at: "2020-01-01T00:00:00Z".into(),
            source: "synthetic-stale-fixture".into(),
        })
    }
}
struct SyntheticValuator;
#[async_trait]
impl AssetValuator for SyntheticValuator {
    async fn usd_micros(&self, amount: &Amount) -> Result<(String, String, String)> {
        let atoms = amount.atoms.integer();
        let unit = num_bigint::BigUint::from(10u8).pow(amount.asset.decimals as u32);
        let usd = atoms * num_bigint::BigUint::from(2_000_000_000u64) / unit;
        Ok((
            usd.to_string(),
            now(),
            "synthetic-fixture:ETH-USD-2000".into(),
        ))
    }
}

fn runtime_config() -> BrokerConfigV2 {
    let mut config = BrokerConfigV2 {
        version: 2,
        networks: vec![fin_broker::NetworkConfig::ethereum_mainnet()],
        deployments: vec![fin_broker::DeploymentConfig::ethereum_mainnet()],
        connections: vec![],
        policy: BrokerPolicy::beta(),
        approver_uid: 501,
        extensions: BTreeMap::new(),
    };
    for (name, uid, role, parent, email) in [
        (MAIN, 1001, "main", None, None),
        (CHILD_A, 1002, "sub", Some(MAIN), Some(EMAIL_A)),
        (CHILD_B, 1003, "sub", Some(MAIN), Some(EMAIL_B)),
    ] {
        config.connections.push(ConnectionConfig::Binance {
            name: name.into(),
            resource: resource(uid),
            credential_ref: format!("00000000-0000-4000-8000-{uid:012}"),
            role: role.into(),
            parent: parent.map(str::to_owned),
            email: email.map(str::to_owned),
            wallet_type: "SPOT".into(),
            physical_identity: uid.to_string(),
            physical_identity_verified: true,
        });
    }
    config.connections.push(ConnectionConfig::Ethereum {
        name: "synthetic-wallet".into(),
        resource: wallet_resource(),
        credential_ref: Some("00000000-0000-4000-8000-000000000001".into()),
        network: "ethereum:1".into(),
        address: "0x2222222222222222222222222222222222222222".into(),
        signing_enabled: true,
        assets: vec![native_asset()],
    });
    config
}

fn wallet_resource() -> ResourceId {
    ResourceId("ethereum:1:0x2222222222222222222222222222222222222222".into())
}

fn native_asset() -> AssetId {
    AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    }
}

fn adapter_config() -> BinanceConfig {
    let accounts = vec![
        BinanceAccount {
            name: MAIN.into(),
            resource: resource(1001),
            credential: CredentialHandle::new("test-only:main").unwrap(),
            role: AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        },
        BinanceAccount {
            name: CHILD_A.into(),
            resource: resource(1002),
            credential: CredentialHandle::new("test-only:a").unwrap(),
            role: AccountRole::Sub,
            parent: Some(MAIN.into()),
            email: Some(EMAIL_A.into()),
            wallet_type: "SPOT".into(),
        },
        BinanceAccount {
            name: CHILD_B.into(),
            resource: resource(1003),
            credential: CredentialHandle::new("test-only:b").unwrap(),
            role: AccountRole::Sub,
            parent: Some(MAIN.into()),
            email: Some(EMAIL_B.into()),
            wallet_type: "SPOT".into(),
        },
    ];
    BinanceConfig {
        accounts,
        destinations: vec![fin_binance::EthereumDestination {
            name: "synthetic-wallet".into(),
            resource: wallet_resource(),
            address: "0x2222222222222222222222222222222222222222".into(),
            chain_id: 1,
            assets: BTreeMap::from([("ETH".into(), native_asset())]),
            confirmations: 2,
        }],
    }
}
fn adapter(venue: Arc<SyntheticVenue>) -> Arc<BinanceAdapter> {
    Arc::new(
        BinanceAdapter::new(
            adapter_config(),
            venue,
            Some(Arc::new(SyntheticBinancePrice)),
            None,
        )
        .unwrap(),
    )
}

struct WalletSnapshotObserver {
    resource: ResourceId,
    address: String,
    balance: Arc<Mutex<String>>,
}

#[async_trait]
impl Adapter for WalletSnapshotObserver {
    fn definitions(&self) -> Vec<fin_domain::Definition> {
        vec![]
    }
    fn connections(&self) -> Vec<fin_domain::ConnectionMetadata> {
        vec![fin_domain::ConnectionMetadata {
            name: "synthetic-wallet".into(),
            provider: "ethereum".into(),
            resource: self.resource.clone(),
            role: Some("wallet".into()),
            parent: None,
            network: Some("ethereum:1".into()),
            address: Some(self.address.clone()),
            credential_ref: None,
            signing_enabled: true,
            details: json!({}),
        }]
    }
    async fn snapshot(&self) -> Result<fin_domain::StateSnapshot> {
        Ok(fin_domain::StateSnapshot {
            state: fin_domain::FinancialState {
                holdings: vec![ResourceAmount {
                    resource: self.resource.clone(),
                    amount: Amount {
                        asset: native_asset(),
                        atoms: Quantity::from_decimal(&self.balance.lock().unwrap(), 18).unwrap(),
                    },
                }],
                ..Default::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec![self.resource.0.clone()],
            pending: vec![],
        })
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn prepare(&self, _: &str, _: &Value) -> Result<fin_domain::PreparedOperation> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn revalidate(
        &self,
        _: &fin_domain::PreparedOperation,
    ) -> Result<fin_domain::PreparedOperation> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn submit(
        &self,
        _: &fin_domain::PreparedOperation,
        _: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<fin_domain::Submission> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
    async fn reconcile(
        &self,
        _: &fin_domain::OperationRecord,
    ) -> Result<fin_domain::Reconciliation> {
        Err(FinError::validation("Snapshot-only fixture."))
    }
}
fn runtime(
    db: &std::path::Path,
    venue: Arc<SyntheticVenue>,
    approval: Arc<SyntheticApproval>,
) -> Arc<fin_broker::BrokerRuntime> {
    compose_runtime(
        runtime_config(),
        RuntimeDependencies {
            store: Store::open(db).unwrap(),
            adapters: vec![adapter(venue)],
            authorizer: approval,
            valuator: Arc::new(SyntheticValuator),
        },
    )
    .unwrap()
}

async fn execute(
    broker: &fin_broker::BrokerRuntime,
    run_id: &str,
    id: &str,
    input: Value,
) -> Value {
    let response = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: id.into(),
                operation: "binance.transfer".into(),
                input,
                run_id: Some(run_id.into()),
            },
        })
        .await;
    assert!(response.ok, "{:?}", response.error);
    let result = response.result.unwrap();
    assert_eq!(result["status"], "completed", "{result}");
    result
}

#[tokio::test]
async fn two_child_spot_roundtrip_restores_allocations_and_reconciles_history() {
    assert!(!fin_binance::ExactDecimal::parse("0.005").unwrap().is_zero());
    let venue = Arc::new(SyntheticVenue::new());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("synthetic.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let adapter = adapter(venue.clone());
    let preview = fin_domain::Adapter::prepare(
        &*adapter,
        "binance.transfer",
        &transfer(STEP_A_IN, CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert!(
        preview.is_ok(),
        "{preview:?}; transcript={:?}",
        venue.state.lock().unwrap().requests
    );
    drop(preview);
    let broker = compose_runtime(
        runtime_config(),
        RuntimeDependencies {
            store: Store::open(&db).unwrap(),
            adapters: vec![adapter],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
        },
    )
    .unwrap();
    let plan = RunBeginRequest {
        allocations: vec![
            ResourceAmount {
                resource: resource(1002),
                amount: Amount {
                    asset: asset(),
                    atoms: Quantity::from_decimal("0.005", 36).unwrap(),
                },
            },
            ResourceAmount {
                resource: resource(1003),
                amount: Amount {
                    asset: asset(),
                    atoms: Quantity::from_decimal("0.010", 36).unwrap(),
                },
            },
        ],
        planned_operations: vec![
            step(STEP_A_IN, CHILD_A, MAIN, "0.005"),
            step(STEP_B_IN, CHILD_B, MAIN, "0.010"),
        ],
        borrowed_proceeds_reserve_usd_micros: "0".into(),
        return_cost_reserve_usd_micros: "0".into(),
        unwind: vec![
            step(STEP_A_OUT, MAIN, CHILD_A, "0.005"),
            step(STEP_B_OUT, MAIN, CHILD_B, "0.010"),
        ],
    };
    let started = broker
        .handle(BrokerRequest::RunBegin { request: plan })
        .await;
    assert!(started.ok, "{:?}", started.error);
    let run = started.result.unwrap();
    let run_id = run["id"].as_str().unwrap();
    let one = execute(
        &broker,
        run_id,
        STEP_A_IN,
        transfer(STEP_A_IN, CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert_eq!(one["status"], "completed", "{one}");
    let duplicate = execute(
        &broker,
        run_id,
        STEP_A_IN,
        transfer(STEP_A_IN, CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert_eq!(duplicate["id"], one["id"]);
    execute(
        &broker,
        run_id,
        STEP_B_IN,
        transfer(STEP_B_IN, CHILD_B, MAIN, "0.010"),
    )
    .await;
    execute(
        &broker,
        run_id,
        STEP_A_OUT,
        transfer(STEP_A_OUT, MAIN, CHILD_A, "0.005"),
    )
    .await;
    execute(
        &broker,
        run_id,
        STEP_B_OUT,
        transfer(STEP_B_OUT, MAIN, CHILD_B, "0.010"),
    )
    .await;
    let finished = broker
        .handle(BrokerRequest::RunFinish { id: run_id.into() })
        .await;
    assert!(finished.ok, "{:?}", finished.error);
    let finished_value = finished.result.unwrap();
    assert_eq!(finished_value["status"], "restored", "{finished_value}");
    let s = venue.state.lock().unwrap();
    assert_eq!(s.balances[EMAIL_A], "0.005");
    assert_eq!(s.balances[EMAIL_B], "0.010");
    assert_eq!(s.balances["main"], "0.000");
    assert_eq!(
        s.posts, 4,
        "duplicate request must not submit a second transfer"
    );
    assert_eq!(s.history.len(), 4);
    assert!(s.history.iter().all(|row| row["status"] == "SUCCESS"
        && row["clientTranId"].as_str().unwrap().starts_with("fin")));
    assert_eq!(approval.0.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn concurrent_duplicate_transfer_uses_one_approval_and_one_venue_submission() {
    let venue = Arc::new(SyntheticVenue::new());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("concurrent-duplicate.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = runtime(&db, venue.clone(), approval.clone());
    let start = broker
        .handle(BrokerRequest::RunBegin {
            request: RunBeginRequest {
                allocations: vec![ResourceAmount {
                    resource: resource(1002),
                    amount: Amount {
                        asset: asset(),
                        atoms: Quantity::from_decimal("0.005", 36).unwrap(),
                    },
                }],
                planned_operations: vec![step("concurrent-in", CHILD_A, MAIN, "0.005")],
                borrowed_proceeds_reserve_usd_micros: "0".into(),
                return_cost_reserve_usd_micros: "0".into(),
                unwind: vec![step("concurrent-out", MAIN, CHILD_A, "0.005")],
            },
        })
        .await;
    assert!(start.ok, "{:?}", start.error);
    let run_id = start.result.unwrap()["id"].as_str().unwrap().to_owned();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let call = |broker: Arc<fin_broker::BrokerRuntime>, barrier: Arc<tokio::sync::Barrier>| {
        let run_id = run_id.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            broker
                .handle(BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: "concurrent-in".into(),
                        operation: "binance.transfer".into(),
                        input: transfer("concurrent-in", CHILD_A, MAIN, "0.005"),
                        run_id: Some(run_id),
                    },
                })
                .await
        })
    };
    let first = call(broker.clone(), barrier.clone());
    let second = call(broker.clone(), barrier.clone());
    barrier.wait().await;
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    assert!(first.ok, "{:?}", first.error);
    assert!(second.ok, "{:?}", second.error);
    let first = first.result.unwrap();
    let second = second.result.unwrap();
    assert_eq!(first["id"], second["id"]);
    assert_eq!(first["status"], "completed");
    assert_eq!(second["status"], "completed");
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rate_limited_transfer_retry_returns_original_record_without_reposting() {
    let venue = Arc::new(SyntheticVenue::new());
    let client_id = fin_binance::ClientIdentity::transfer("rate-limited-in").unwrap();
    venue.state.lock().unwrap().rate_limit_request_id = Some(client_id.as_str().into());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("rate-limited.sqlite");
    let broker = runtime(
        &db,
        venue.clone(),
        Arc::new(SyntheticApproval(AtomicUsize::new(0))),
    );
    let started = broker
        .handle(BrokerRequest::RunBegin {
            request: RunBeginRequest {
                allocations: vec![ResourceAmount {
                    resource: resource(1002),
                    amount: Amount {
                        asset: asset(),
                        atoms: Quantity::from_decimal("0.005", 36).unwrap(),
                    },
                }],
                planned_operations: vec![step("rate-limited-in", CHILD_A, MAIN, "0.005")],
                borrowed_proceeds_reserve_usd_micros: "0".into(),
                return_cost_reserve_usd_micros: "0".into(),
                unwind: vec![step("rate-limited-out", MAIN, CHILD_A, "0.005")],
            },
        })
        .await;
    assert!(started.ok, "{:?}", started.error);
    let run_id = started.result.unwrap()["id"].as_str().unwrap().to_owned();
    let first = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "rate-limited-in".into(),
                operation: "binance.transfer".into(),
                input: transfer("rate-limited-in", CHILD_A, MAIN, "0.005"),
                run_id: Some(run_id.clone()),
            },
        })
        .await;
    let second = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "rate-limited-in".into(),
                operation: "binance.transfer".into(),
                input: transfer("rate-limited-in", CHILD_A, MAIN, "0.005"),
                run_id: Some(run_id),
            },
        })
        .await;
    assert_eq!(first.ok, second.ok, "first={first:?}; second={second:?}");
    assert_eq!(
        first.result.as_ref().unwrap()["id"],
        second.result.as_ref().unwrap()["id"],
        "first={first:?}; second={second:?}"
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
}

#[tokio::test]
async fn accepted_but_lost_transfer_response_recovers_original_client_id_after_restart() {
    let venue = Arc::new(SyntheticVenue::new());
    let client_id = fin_binance::ClientIdentity::transfer("lost-a-to-main").unwrap();
    venue.state.lock().unwrap().accept_then_timeout = Some(client_id.as_str().into());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("restart.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = runtime(&db, venue.clone(), approval.clone());
    let plan = RunBeginRequest {
        allocations: vec![ResourceAmount {
            resource: resource(1002),
            amount: Amount {
                asset: asset(),
                atoms: Quantity::from_decimal("0.005", 36).unwrap(),
            },
        }],
        planned_operations: vec![step("lost-a-to-main", CHILD_A, MAIN, "0.005")],
        borrowed_proceeds_reserve_usd_micros: "0".into(),
        return_cost_reserve_usd_micros: "0".into(),
        unwind: vec![step("restore-a", MAIN, CHILD_A, "0.005")],
    };
    let started = broker
        .handle(BrokerRequest::RunBegin { request: plan })
        .await;
    assert!(started.ok, "{:?}", started.error);
    let run_id = started.result.unwrap()["id"].as_str().unwrap().to_owned();
    let unknown_response = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "lost-a-to-main".into(),
                operation: "binance.transfer".into(),
                input: transfer("lost-a-to-main", CHILD_A, MAIN, "0.005"),
                run_id: Some(run_id.clone()),
            },
        })
        .await;
    assert!(unknown_response.ok, "{:?}", unknown_response.error);
    let unknown = unknown_response.result.unwrap();
    assert_eq!(unknown["status"], "unknown", "{unknown}");
    drop(broker);

    let recovered = runtime(&db, venue.clone(), approval.clone());
    recovered.engine.recover_interrupted().unwrap();
    let status = recovered
        .handle(BrokerRequest::Status {
            id: unknown["id"].as_str().unwrap().into(),
        })
        .await;
    assert!(status.ok, "{:?}", status.error);
    let settled = status.result.unwrap();
    assert_eq!(settled["status"], "completed", "{settled}");
    assert_eq!(settled["submission"]["reference"], client_id.as_str());
    let duplicate = execute(
        &recovered,
        &run_id,
        "lost-a-to-main",
        transfer("lost-a-to-main", CHILD_A, MAIN, "0.005"),
    )
    .await;
    assert_eq!(duplicate["id"], settled["id"]);
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "recovery must never replay an accepted POST"
    );
    execute(
        &recovered,
        &run_id,
        "restore-a",
        transfer("restore-a", MAIN, CHILD_A, "0.005"),
    )
    .await;
    let finished = recovered
        .handle(BrokerRequest::RunFinish { id: run_id })
        .await;
    assert!(finished.ok, "{:?}", finished.error);
    let finished_value = finished.result.unwrap();
    assert_eq!(finished_value["status"], "restored", "{finished_value}");
    assert_eq!(venue.state.lock().unwrap().balances[EMAIL_A], "0.005");
}

#[tokio::test]
async fn permission_precision_and_stale_price_guards_block_before_any_post() {
    use fin_domain::Adapter;
    let venue = Arc::new(SyntheticVenue::new());
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("guards.sqlite");
    let adapter = adapter(venue.clone());
    let broker = compose_runtime(
        runtime_config(),
        RuntimeDependencies {
            store: Store::open(&db).unwrap(),
            adapters: vec![adapter.clone()],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
        },
    )
    .unwrap();

    for (id, value) in [
        ("guard-permission", Some(false)),
        ("guard-unknown-permission", None),
    ] {
        venue.state.lock().unwrap().restrictions = value;
        let permission = broker
            .handle(BrokerRequest::Preview {
                operation: "binance.transfer".into(),
                input: transfer(id, CHILD_A, MAIN, "0.005"),
            })
            .await;
        assert!(!permission.ok && permission.error.as_ref().unwrap().code == "MISSING_PERMISSION");
    }
    venue.state.lock().unwrap().restrictions = Some(true);
    let precision = broker
        .handle(BrokerRequest::Preview {
            operation: "binance.transfer".into(),
            input: transfer("guard-precision", CHILD_A, MAIN, "0.0055"),
        })
        .await;
    assert!(!precision.ok && precision.error.as_ref().unwrap().code == "INVALID_PRECISION");
    let stale_adapter = Arc::new(
        BinanceAdapter::new(
            adapter_config(),
            venue.clone(),
            Some(Arc::new(StaleBinancePrice)),
            None,
        )
        .unwrap(),
    );
    let stale = stale_adapter
        .prepare(
            "binance.transfer",
            &transfer("guard-stale", CHILD_A, MAIN, "0.005"),
        )
        .await;
    assert!(stale.is_err() && stale.unwrap_err().code == "STALE_VALUATION");
    assert_eq!(venue.state.lock().unwrap().posts, 0);
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
}

struct CanonicalNativeCreditRpc {
    value: Mutex<String>,
    latest: Mutex<String>,
}

fn synthetic_oracle_updated_at() -> u128 {
    // Round 1 has one immutable publication time; advancing the observer must
    // not synthesize a different source for that same round on every read.
    (chrono::Utc::now().timestamp() - 30).try_into().unwrap()
}

struct SyntheticOracleClock(AtomicU64);

#[async_trait]
impl fin_ethereum::OracleObservationClock for SyntheticOracleClock {
    async fn unix_seconds(&self) -> Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct DepositPreviewRpc {
    oracle_updated_at: u128,
}
impl DepositPreviewRpc {
    fn new() -> Self {
        Self {
            oracle_updated_at: synthetic_oracle_updated_at(),
        }
    }
    fn abi_word(value: u128) -> [u8; 32] {
        let mut word = [0u8; 32];
        word[16..].copy_from_slice(&value.to_be_bytes());
        word
    }
    fn selector(signature: &str) -> [u8; 4] {
        fin_evm::alloy::primitives::keccak256(signature.as_bytes())[..4]
            .try_into()
            .unwrap()
    }
}

struct DepositExecutionRpc {
    oracle_updated_at: u128,
    transaction: Arc<Mutex<Option<fin_evm::FrozenTransaction>>>,
    hash: Mutex<Option<String>>,
    accepted: AtomicUsize,
    venue: Arc<SyntheticVenue>,
    journal: Arc<Mutex<Option<fin_domain::Submission>>>,
    durable_store_path: Option<std::path::PathBuf>,
    wallet_address: String,
    wallet_balance: Arc<Mutex<String>>,
    payout_amount: String,
}

struct RecordingSigner {
    signer: fin_evm::LocalEnvelopeSigner,
    transaction: Arc<Mutex<Option<fin_evm::FrozenTransaction>>>,
}

#[async_trait]
impl fin_evm::EnvelopeSigner for RecordingSigner {
    async fn sign_prepared(
        &self,
        transaction: &fin_evm::FrozenTransaction,
    ) -> Result<fin_evm::alloy::primitives::Bytes> {
        *self.transaction.lock().unwrap() = Some(transaction.clone());
        self.signer.sign_prepared(transaction).await
    }
}

#[derive(Default)]
struct SyntheticSubmissionJournal {
    inner: Arc<Mutex<Option<fin_domain::Submission>>>,
}

impl fin_domain::SubmissionJournal for SyntheticSubmissionJournal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn persist_submission(&self, submission: &fin_domain::Submission) -> Result<()> {
        let mut slot = self.inner.lock().unwrap();
        if slot.is_some() {
            return Err(FinError::new(
                "SYNTHETIC_DUPLICATE_SUBMISSION",
                "test",
                "A submission identity was persisted twice.",
                "Do not replay an accepted write.",
            ));
        }
        *slot = Some(submission.clone());
        Ok(())
    }
}

#[async_trait]
impl EvmRpc for DepositExecutionRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let deposit = "0x1111111111111111111111111111111111111111";
        let wallet = self.wallet_address.as_str();
        let payout_hash = format!("0x{}", "ee".repeat(32));
        let oracle = fin_ethereum::EthereumConfig::default().aave.oracle;
        match method {
            "eth_sendRawTransaction" => {
                let persisted = self.journal.lock().unwrap().clone().or_else(|| {
                    let path = self.durable_store_path.as_ref()?;
                    let store = fin_store::Store::open(path).ok()?;
                    let records = store.list_records().ok()?;
                    records.into_iter().find_map(|record| {
                        (record["request_id"] == "scoped-deposit-forward")
                            .then(|| serde_json::from_value(record["submission"].clone()).ok())
                            .flatten()
                    })
                });
                if persisted.is_none() {
                    return Err(FinError::new(
                        "JOURNAL_ORDER_INVALID",
                        "test",
                        "No pre-dispatch original identity was persisted.",
                        "Persist before sending.",
                    ));
                }
                if self.accepted.fetch_add(1, Ordering::SeqCst) != 0 {
                    return Err(FinError::new(
                        "DUPLICATE_BROADCAST",
                        "test",
                        "Synthetic write was broadcast twice.",
                        "Reconcile the original hash.",
                    ));
                }
                let raw =
                    hex::decode(params[0].as_str().unwrap().trim_start_matches("0x")).unwrap();
                let hash = format!("{:#x}", fin_evm::alloy::primitives::keccak256(raw));
                if persisted
                    .as_ref()
                    .is_some_and(|submission| submission.reference != hash)
                {
                    return Err(FinError::new(
                        "JOURNAL_IDENTITY_MISMATCH",
                        "test",
                        "Persisted transaction hash differs from accepted bytes.",
                        "Reconcile the original identity.",
                    ));
                }
                *self.hash.lock().unwrap() = Some(hash.clone());
                let transaction = self.transaction.lock().unwrap().clone().unwrap();
                self.venue
                    .record_deposit(&hash, &transaction.value.to_decimal(18));
                adjust_native_balance(
                    &self.wallet_balance,
                    &transaction.value.to_decimal(18),
                    false,
                );
                adjust_native_balance(&self.wallet_balance, "0.000042", false);
                Ok(json!(hash))
            }
            "eth_chainId" => Ok(json!("0x1")),
            "eth_blockNumber" => Ok(json!("0x65")),
            "eth_getTransactionReceipt" => {
                let hash = self.hash.lock().unwrap().clone();
                if payout_hash == params[0].as_str().unwrap_or_default() {
                    return Ok(
                        json!({"transactionHash":payout_hash,"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x77359400","logs":[]}),
                    );
                }
                if hash.as_deref() != params[0].as_str() {
                    return Ok(Value::Null);
                }
                Ok(
                    json!({"transactionHash":hash,"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64","status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x77359400","logs":[]}),
                )
            }
            "eth_getTransactionByHash" => {
                let hash = self.hash.lock().unwrap().clone();
                let transaction = self.transaction.lock().unwrap().clone();
                if payout_hash == params[0].as_str().unwrap_or_default() {
                    let value =
                        fin_evm::u256(&Quantity::from_decimal(&self.payout_amount, 18).unwrap())
                            .unwrap();
                    return Ok(
                        json!({"hash":payout_hash,"from":"0x3333333333333333333333333333333333333333","to":wallet,"nonce":"0x2","chainId":"0x1","type":"0x2","gas":"0x5208","value":format!("{value:#x}"),"input":"0x","maxFeePerGas":"0x77359400","maxPriorityFeePerGas":"0x989680","accessList":[],"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64"}),
                    );
                }
                if hash.as_deref() != params[0].as_str() || transaction.is_none() {
                    return Ok(Value::Null);
                }
                let tx = transaction.unwrap();
                let to_hex = |quantity: &str| {
                    let value = fin_evm::alloy::primitives::U256::from_str(quantity).unwrap();
                    format!("{value:#x}")
                };
                Ok(
                    json!({"hash":hash,"from":tx.sender,"to":tx.to,"nonce":format!("{:#x}",tx.nonce),"chainId":format!("{:#x}",tx.chain_id),"type":"0x2","gas":format!("{:#x}",tx.gas),"value":to_hex(tx.value.as_str()),"input":tx.calldata,"maxFeePerGas":to_hex(tx.max_fee_per_gas.as_str()),"maxPriorityFeePerGas":to_hex(tx.max_priority_fee_per_gas.as_str()),"accessList":[],"blockHash":format!("0x{}", "cd".repeat(32)),"blockNumber":"0x64"}),
                )
            }
            "eth_getBlockByNumber" => {
                let number = if params[0] == "latest" {
                    "0x65"
                } else {
                    params[0].as_str().unwrap()
                };
                let mut value = json!({"hash":format!("0x{}", "cd".repeat(32)),"number":number});
                value["baseFeePerGas"] = json!("0x3b9aca00");
                Ok(value)
            }
            "eth_getCode" => Ok(json!(if [deposit, wallet]
                .iter()
                .any(|address| params[0].as_str().unwrap().eq_ignore_ascii_case(address))
            {
                "0x"
            } else {
                "0x6000"
            })),
            "eth_getBalance" => Ok(json!("0x16345785d8a0000")),
            "eth_getTransactionCount" => Ok(json!("0x0")),
            "eth_estimateGas" => Ok(json!("0x5208")),
            "eth_maxPriorityFeePerGas" => Ok(json!("0x989680")),
            "eth_call" => {
                let to = params[0]["to"].as_str().unwrap_or("");
                let data = params[0]["data"].as_str().unwrap_or("0x");
                if to.eq_ignore_ascii_case(&oracle) && data.len() >= 10 {
                    let selector = hex::decode(&data[2..10]).unwrap();
                    let result = if selector == DepositPreviewRpc::selector("BASE_CURRENCY()") {
                        vec![0u8; 32]
                    } else if selector == DepositPreviewRpc::selector("BASE_CURRENCY_UNIT()") {
                        DepositPreviewRpc::abi_word(100_000_000).to_vec()
                    } else if selector == DepositPreviewRpc::selector("getAssetPrice(address)") {
                        DepositPreviewRpc::abi_word(200_000_000_000).to_vec()
                    } else if selector == DepositPreviewRpc::selector("getSourceOfAsset(address)") {
                        let mut word = [0u8; 32];
                        word[12..].copy_from_slice(&[0x33; 20]);
                        word.to_vec()
                    } else {
                        return Err(FinError::validation(
                            "Unexpected synthetic oracle selector.",
                        ));
                    };
                    Ok(json!(fin_evm::hex_bytes(&result)))
                } else if data.len() >= 10
                    && hex::decode(&data[2..10]).unwrap()
                        == DepositPreviewRpc::selector("latestRoundData()")
                {
                    let mut result = Vec::new();
                    for value in [
                        1,
                        200_000_000_000,
                        self.oracle_updated_at,
                        self.oracle_updated_at,
                        1,
                    ] {
                        result.extend_from_slice(&DepositPreviewRpc::abi_word(value));
                    }
                    Ok(json!(fin_evm::hex_bytes(&result)))
                } else {
                    Ok(json!("0x"))
                }
            }
            method => panic!("unexpected synthetic deposit execution RPC method {method}"),
        }
    }
}

#[async_trait]
impl EvmRpc for DepositPreviewRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let deposit = "0x1111111111111111111111111111111111111111";
        let oracle = fin_ethereum::EthereumConfig::default().aave.oracle;
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_getCode" => json!(
                if params[0].as_str().unwrap().eq_ignore_ascii_case(deposit) {
                    "0x"
                } else {
                    "0x6000"
                }
            ),
            "eth_getBalance" => json!("0x16345785d8a0000"),
            "eth_getTransactionCount" => json!("0x0"),
            "eth_estimateGas" => json!("0x5208"),
            "eth_maxPriorityFeePerGas" => json!("0x989680"),
            "eth_getBlockByNumber" => {
                json!({"number":"0x65","hash":format!("0x{}", "cd".repeat(32)),"baseFeePerGas":"0x3b9aca00"})
            }
            "eth_call" => {
                let request = &params[0];
                let data = request["data"].as_str().unwrap_or("0x");
                if request["to"]
                    .as_str()
                    .is_some_and(|to| to.eq_ignore_ascii_case(&oracle))
                    && data.len() >= 10
                {
                    let selector = hex::decode(&data[2..10]).unwrap();
                    let result = if selector == Self::selector("BASE_CURRENCY()") {
                        vec![0u8; 32]
                    } else if selector == Self::selector("BASE_CURRENCY_UNIT()") {
                        Self::abi_word(100_000_000).to_vec()
                    } else if selector == Self::selector("getAssetPrice(address)") {
                        Self::abi_word(200_000_000_000).to_vec()
                    } else if selector == Self::selector("getSourceOfAsset(address)") {
                        let mut word = [0u8; 32];
                        word[12..].copy_from_slice(&[0x33; 20]);
                        word.to_vec()
                    } else {
                        return Err(FinError::validation(
                            "Unexpected synthetic oracle selector.",
                        ));
                    };
                    json!(fin_evm::hex_bytes(&result))
                } else if data.len() >= 10 {
                    let selector = hex::decode(&data[2..10]).unwrap();
                    if selector == Self::selector("latestRoundData()") {
                        let mut result = Vec::new();
                        for value in [
                            1,
                            200_000_000_000,
                            self.oracle_updated_at,
                            self.oracle_updated_at,
                            1,
                        ] {
                            result.extend_from_slice(&Self::abi_word(value));
                        }
                        json!(fin_evm::hex_bytes(&result))
                    } else {
                        json!("0x")
                    }
                } else {
                    json!("0x")
                }
            }
            method => panic!("unexpected synthetic preview RPC method {method}"),
        })
    }
}

#[async_trait]
impl EvmRpc for CanonicalNativeCreditRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let tx_hash = format!("0x{}", "ab".repeat(32));
        let block_hash = format!("0x{}", "cd".repeat(32));
        let recipient = "0x1111111111111111111111111111111111111111";
        Ok(match method {
            "eth_chainId" => json!("0x1"),
            "eth_blockNumber" => json!(*self.latest.lock().unwrap()),
            "eth_getTransactionReceipt" => json!({
                "transactionHash":tx_hash,
                "blockHash":block_hash,
                "blockNumber":"0x64",
                "status":"0x1",
                "gasUsed":"0x5208",
                "effectiveGasPrice":"0x3b9aca00",
                "logs":[]
            }),
            "eth_getTransactionByHash" => json!({
                "hash":tx_hash,
                "from":"0x2222222222222222222222222222222222222222",
                "to":recipient,
                "value":*self.value.lock().unwrap(),
                "chainId":"0x1",
                "blockHash":block_hash,
                "blockNumber":"0x64"
            }),
            "eth_getBlockByNumber" => {
                assert_eq!(params[0], "0x64");
                json!({"hash":block_hash,"number":"0x64"})
            }
            "eth_getCode" => json!("0x"),
            "eth_getBalance" => json!("0x16345785d8a0000"),
            method => panic!("unexpected isolated payout proof RPC method {method}"),
        })
    }
}

#[tokio::test]
async fn canonical_native_payout_proof_checks_inclusion_finality_recipient_and_exact_amount() {
    let rpc = Arc::new(CanonicalNativeCreditRpc {
        value: Mutex::new("0x2386f26fc10000".into()), // 0.01 ETH exactly.
        latest: Mutex::new("0x65".into()),
    });
    let wallet = "0x1111111111111111111111111111111111111111";
    let ethereum = fin_ethereum::EthereumAdapter::new(
        fin_ethereum::EthereumConfig::default(),
        vec![fin_ethereum::WalletConnection {
            name: "synthetic-payout-recipient".into(),
            address: wallet.into(),
            signing_enabled: false,
        }],
        Arc::new(fin_evm::EvmRuntime::new(rpc.clone(), 1, None)),
    )
    .unwrap();
    let hash = format!("0x{}", "ab".repeat(32));
    let native = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let proof = ethereum
        .verify_external_credit(&hash, wallet, &native, "0.01", "0.01", 2)
        .await
        .unwrap();
    assert_eq!(proof.status, fin_domain::Status::Completed);
    assert_eq!(
        proof.amount_atoms.unwrap(),
        Quantity::from_decimal("0.01", 18).unwrap()
    );
    assert_eq!(
        proof.evidence["proof_method"],
        "canonical-direct-native-value"
    );

    let unconfirmed = CanonicalNativeCreditRpc {
        value: Mutex::new("0x2386f26fc10000".into()),
        latest: Mutex::new("0x64".into()),
    };
    let pending = fin_ethereum::EthereumAdapter::new(
        fin_ethereum::EthereumConfig::default(),
        vec![fin_ethereum::WalletConnection {
            name: "synthetic-payout-recipient".into(),
            address: wallet.into(),
            signing_enabled: false,
        }],
        Arc::new(fin_evm::EvmRuntime::new(Arc::new(unconfirmed), 1, None)),
    )
    .unwrap()
    .verify_external_credit(&hash, wallet, &native, "0.01", "0.01", 2)
    .await
    .unwrap();
    assert_eq!(pending.status, fin_domain::Status::Pending);

    *rpc.value.lock().unwrap() = "0x1".into();
    assert_eq!(
        ethereum
            .verify_external_credit(&hash, wallet, &native, "0.01", "0.01", 2)
            .await
            .unwrap_err()
            .code,
        "FUNDING_AMOUNT_MISMATCH"
    );
}

#[tokio::test]
async fn binance_deposit_preview_freezes_only_current_fixed_eth_target_and_exact_cross_asset_amount()
 {
    let venue = Arc::new(SyntheticVenue::new());
    let binance = adapter(venue.clone());
    let wallet_address = "0x2222222222222222222222222222222222222222";
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address.into(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                Arc::new(DepositPreviewRpc::new()),
                1,
                None,
            )),
        )
        .unwrap(),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let input = json!({"requestId":"synthetic-deposit-1","account":"main","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let prepared = composite.prepare("binance.deposit", &input).await.unwrap();
    assert_eq!(prepared.operation, "binance.deposit");
    assert_eq!(prepared.intent.expected.len(), 1);
    match &prepared.intent.expected[0] {
        fin_domain::EffectConstraint::Transfer {
            from,
            to,
            debit_asset,
            credit_asset,
            maximum_debit,
            minimum_credit,
        } => {
            assert_eq!(from.0, format!("ethereum:1:{wallet_address}"));
            assert_eq!(to.0, resource(1001).0);
            assert_eq!(debit_asset.network, "ethereum:1");
            assert_eq!(debit_asset.decimals, 18);
            assert_eq!(credit_asset.network, "binance");
            assert_eq!(credit_asset.reference, "ETH");
            assert_eq!(credit_asset.decimals, 36);
            assert_eq!(maximum_debit.to_decimal(18), "0.01");
            assert_eq!(minimum_credit.to_decimal(36), "0.01");
        }
        other => panic!("unexpected composite constraint {other:?}"),
    }

    let below_minimum = json!({"requestId":"synthetic-deposit-2","account":"main","wallet":"synthetic-wallet","amount":"0.0009","maxFeeEth":"0.001"});
    assert_eq!(
        composite
            .prepare("binance.deposit", &below_minimum)
            .await
            .unwrap_err()
            .code,
        "DEPOSIT_MINIMUM"
    );
    let invalid_wallet = json!({"requestId":"synthetic-deposit-3","account":"child-a","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    assert_eq!(
        composite
            .prepare("binance.deposit", &invalid_wallet)
            .await
            .unwrap_err()
            .code,
        "BINANCE_MAIN_REQUIRED"
    );
}

#[tokio::test]
async fn binance_deposit_return_route_proof_binds_fresh_address_and_cross_asset_projection() {
    use fin_domain::{
        ProjectedCash, ProjectedState, QuantityInterval, ReturnRouteContext, StateSnapshot,
    };

    let venue = Arc::new(SyntheticVenue::new());
    let binance = adapter(venue);
    let wallet_address = "0x2222222222222222222222222222222222222222";
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address.into(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                Arc::new(DepositPreviewRpc::new()),
                1,
                None,
            )),
        )
        .unwrap(),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let wallet = ResourceId(format!("ethereum:1:{wallet_address}"));
    let venue_resource = resource(1001);
    let native = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let venue_eth = AssetId {
        network: "binance".into(),
        reference: "ETH".into(),
        decimals: 36,
    };
    let baseline = StateSnapshot {
        state: fin_domain::FinancialState {
            holdings: vec![
                ResourceAmount {
                    resource: wallet.clone(),
                    amount: Amount {
                        asset: native.clone(),
                        atoms: Quantity::from_decimal("0.1", 18).unwrap(),
                    },
                },
                ResourceAmount {
                    resource: venue_resource.clone(),
                    amount: Amount {
                        asset: venue_eth.clone(),
                        atoms: Quantity::zero(),
                    },
                },
            ],
            ..Default::default()
        },
        observed_at: now(),
        complete: true,
        coverage: vec![wallet.0.clone(), venue_resource.0.clone()],
        pending: vec![],
    };
    let context = ReturnRouteContext {
        baseline,
        projected: ProjectedState {
            cash: vec![
                ProjectedCash {
                    resource: wallet.clone(),
                    asset: native.clone(),
                    amount: QuantityInterval {
                        minimum: Quantity::from_decimal("0.1", 18).unwrap(),
                        maximum: Quantity::from_decimal("0.1", 18).unwrap(),
                    },
                },
                ProjectedCash {
                    resource: venue_resource,
                    asset: venue_eth,
                    amount: QuantityInterval {
                        minimum: Quantity::zero(),
                        maximum: Quantity::zero(),
                    },
                },
            ],
            ..Default::default()
        },
        allocations: vec![ResourceAmount {
            resource: wallet,
            amount: Amount {
                asset: native.clone(),
                atoms: Quantity::from_decimal("0.1", 18).unwrap(),
            },
        }],
        asset_prices: vec![fin_domain::AssetPrice {
            asset: native,
            usd_micros_per_unit: Quantity::from_u64(2_000_000_000),
            unit_atoms: Quantity::from_u64(1_000_000_000_000_000_000),
            observed_at: now(),
            source: "synthetic fixture ETH/USD".into(),
        }],
        step_index: 0,
        prior_proofs: vec![],
        remaining_cost_budget_usd_micros: Quantity::from_u64(10_000_000),
    };
    let input = json!({"requestId":"synthetic-deposit-plan","account":"main","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let proof = composite
        .preflight_return_route("binance.deposit", &input, &context)
        .await
        .unwrap();
    assert_eq!(proof.operation, "binance.deposit");
    assert_eq!(proof.input_hash, fin_domain::binding_hash(&input).unwrap());
    assert!(proof.expires_at > proof.observed_at);
    assert!(proof.evidence["broker_verified_deposit_target"]["evidence_digest"].is_string());
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        fin_domain::ProjectedEffect::CashDelta { resource, asset, minimum_delta, maximum_delta }
            if resource == &ResourceId(format!("ethereum:1:{wallet_address}"))
                && asset.network == "ethereum:1"
                && minimum_delta.as_str() == "-10000000000000000"
                && maximum_delta.as_str() == "-10000000000000000"
    )));
    assert!(
        proof.effects.iter().any(|effect| matches!(effect,
            fin_domain::ProjectedEffect::CashDelta { resource, asset, minimum_delta, maximum_delta }
                if resource.0 == "binance:uid:1001:SPOT"
                    && asset.network == "binance"
                    && asset.decimals == 36
                    && minimum_delta.as_str() == "10000000000000000000000000000000000"
                    && maximum_delta.as_str() == "10000000000000000000000000000000000"
        )),
        "{:?}",
        proof.effects
    );
}

#[tokio::test]
async fn native_binance_deposit_settles_only_after_original_canonical_hash_and_usable_venue_credit()
{
    let venue = Arc::new(SyntheticVenue::new());
    let binance = adapter(venue.clone());
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let wallet_address = format!("{:#x}", signer.address());
    let wallet_balance = Arc::new(Mutex::new("0.1".to_owned()));
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let oracle_updated_at = synthetic_oracle_updated_at();
    let oracle_clock = Arc::new(SyntheticOracleClock(AtomicU64::new(
        u64::try_from(oracle_updated_at).unwrap() + 30,
    )));
    let rpc = Arc::new(DepositExecutionRpc {
        oracle_updated_at,
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        durable_store_path: None,
        wallet_address: wallet_address.clone(),
        wallet_balance: wallet_balance.clone(),
        payout_amount: "0.01".into(),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: wallet_address,
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                1,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap()
        .with_isolated_oracle_clock(oracle_clock.clone()),
    );
    let composite = fin_broker::BinanceEthereumDepositAdapter::new(binance, ethereum);
    let input = json!({"requestId":"synthetic-deposit-e2e","account":"main","wallet":"synthetic-wallet","amount":"0.01","maxFeeEth":"0.001"});
    let prepared = composite.prepare("binance.deposit", &input).await.unwrap();
    let expected_round = format!(":updated:{oracle_updated_at}");
    assert!(
        prepared
            .valuation
            .as_ref()
            .unwrap()
            .source
            .ends_with(&expected_round),
        "A fixed oracle round must retain its constructor timestamp independently of observation time."
    );
    // Advance only the observation clock, without sleeping or fabricating a
    // new oracle round. Exact prices/source and the frozen authorization must
    // remain identical at the final check.
    oracle_clock.0.fetch_add(1, Ordering::SeqCst);
    let refreshed = composite.revalidate(&prepared).await.unwrap();
    assert_eq!(
        fin_domain::authorization_binding(&refreshed),
        fin_domain::authorization_binding(&prepared)
    );
    let submission = composite.submit(&prepared, journal.clone()).await.unwrap();
    assert_eq!(submission.kind, "ethereum");
    assert_eq!(
        submission.reference,
        rpc.hash.lock().unwrap().clone().unwrap()
    );
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 1);

    let record = fin_domain::OperationRecord {
        id: "synthetic-operation-id".into(),
        request_id: "synthetic-deposit-e2e".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: "binance.deposit".into(),
        input,
        status: fin_domain::Status::Submitted,
        created_at: now(),
        updated_at: now(),
        preview: Some(prepared),
        approval: None,
        submission: Some(submission.clone()),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let result = composite.reconcile(&record).await.unwrap();
    assert_eq!(result.status, fin_domain::Status::Completed);
    let verified = result.verified.unwrap();
    assert_eq!(verified.submission, submission);
    assert_eq!(
        verified.proof_digest,
        fin_domain::verified_effect_digest(&verified).unwrap()
    );
    assert_eq!(verified.effects.len(), 2);
    assert!(verified.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Transfer { from, to, debit, credit }
            if from.0.starts_with("ethereum:1:")
                && to == &resource(1001)
                && debit.asset.network == "ethereum:1"
                && debit.asset.decimals == 18
                && debit.atoms.to_decimal(18) == "0.01"
                && credit.asset.network == "binance"
                && credit.asset.decimals == 36
                && credit.atoms.to_decimal(36) == "0.01"
    )));
    assert!(verified.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Cost { category, treatment: fin_domain::CostTreatment::AdditionalDebit, amount, .. }
            if category == "ethereum_gas" && amount.asset.network == "ethereum:1" && !amount.atoms.is_zero()
    )));
    assert_eq!(
        verified.evidence["venueHistory"]["records"][0]["creditState"],
        "credited"
    );
    assert_eq!(venue.state.lock().unwrap().balances["main"], "0.010");

    // Chain finality alone, locked venue credit, and a partial credit never
    // produce completed composite financial effects.
    venue.state.lock().unwrap().deposit_history[0]["status"] = json!(6);
    let locked = composite.reconcile(&record).await.unwrap();
    assert_eq!(locked.status, fin_domain::Status::Unknown);
    assert!(locked.verified.is_none());
    venue.state.lock().unwrap().deposit_history[0]["status"] = json!(1);
    venue.state.lock().unwrap().deposit_history[0]["amount"] = json!("0.009");
    let partial = composite.reconcile(&record).await.unwrap();
    assert_eq!(partial.status, fin_domain::Status::Unknown);
    assert!(partial.verified.is_none());

    venue.state.lock().unwrap().deposit_history[0]["amount"] = json!("0.01");
    venue.state.lock().unwrap().deposit_history[0]["travelRuleStatus"] = json!(2);
    let unknown_travel_state = composite.reconcile(&record).await.unwrap();
    assert_eq!(unknown_travel_state.status, fin_domain::Status::Unknown);
    assert!(unknown_travel_state.verified.is_none());

    venue.state.lock().unwrap().deposit_history[0]["travelRuleStatus"] = json!("1");
    let malformed_travel_state = composite.reconcile(&record).await.unwrap();
    assert_eq!(malformed_travel_state.status, fin_domain::Status::Unknown);
    assert!(malformed_travel_state.verified.is_none());
}

#[tokio::test]
async fn scoped_deposit_withdraw_roundtrip_is_broker_preflighted_executed_and_finished_after_net_costs()
 {
    let venue = Arc::new(SyntheticVenue::new());
    // A small captured main balance funds the separately charged network fee;
    // any fee below the approved cap remains within that original allocation.
    venue
        .state
        .lock()
        .unwrap()
        .balances
        .insert("main".into(), "0.001".into());
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let address = format!("{:#x}", signer.address());
    let wallet = ResourceId(format!("ethereum:1:{address}"));
    let wallet_balance = Arc::new(Mutex::new("0.1".to_owned()));
    *venue.wallet_balance.lock().unwrap() = Some(wallet_balance.clone());
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("deposit-withdraw.sqlite");
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let rpc = Arc::new(DepositExecutionRpc {
        oracle_updated_at: synthetic_oracle_updated_at(),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        durable_store_path: Some(store_path.clone()),
        wallet_address: address.clone(),
        wallet_balance: wallet_balance.clone(),
        payout_amount: "0.009".into(),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                1,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap(),
    );
    let payout = Arc::new(
        fin_broker::EthereumPayoutVerifier::new(BTreeMap::from([(1, ethereum.clone())])).unwrap(),
    );
    let mut binance_config = adapter_config();
    binance_config.destinations[0].address = address.clone();
    binance_config.destinations[0].resource = wallet.clone();
    let binance = Arc::new(
        BinanceAdapter::new(
            binance_config,
            venue.clone(),
            Some(Arc::new(SyntheticBinancePrice)),
            Some(payout),
        )
        .unwrap(),
    );
    let composite = Arc::new(fin_broker::BinanceEthereumDepositAdapter::new(
        binance.clone(),
        ethereum,
    ));
    let wallet_observer = Arc::new(WalletSnapshotObserver {
        resource: wallet.clone(),
        address: address.clone(),
        balance: wallet_balance.clone(),
    });
    let mut config = runtime_config();
    for connection in &mut config.connections {
        if let ConnectionConfig::Ethereum {
            resource,
            address: configured_address,
            ..
        } = connection
        {
            *resource = wallet.clone();
            *configured_address = address.clone();
        }
    }
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = compose_runtime(
        config,
        RuntimeDependencies {
            store: Store::open(&store_path).unwrap(),
            adapters: vec![binance, composite, wallet_observer],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
        },
    )
    .unwrap();

    let deposit = PlanStep {
        operation: "binance.deposit".into(),
        input: json!({"requestId":"scoped-deposit-forward","account":"main","wallet":"synthetic-wallet","amount":"0.009","maxFeeEth":"0.001"}),
        maximum_fee_usd_micros: "2000000".into(),
        description: "Fund the enrolled Binance account with exact native ETH".into(),
    };
    let withdrawal = PlanStep {
        operation: "binance.withdraw".into(),
        input: json!({"requestId":"scoped-withdraw-return","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.009","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"}),
        maximum_fee_usd_micros: "2000000".into(),
        description: "Return the credited Binance ETH to its source wallet".into(),
    };
    let start = broker
        .handle(BrokerRequest::RunBegin {
            request: RunBeginRequest {
                allocations: vec![
                    ResourceAmount {
                        resource: wallet.clone(),
                        amount: Amount {
                            asset: native_asset(),
                            atoms: Quantity::from_decimal("0.02", 18).unwrap(),
                        },
                    },
                    ResourceAmount {
                        resource: resource(1001),
                        amount: Amount {
                            asset: asset(),
                            atoms: Quantity::from_decimal("0.001", 36).unwrap(),
                        },
                    },
                ],
                planned_operations: vec![deposit],
                borrowed_proceeds_reserve_usd_micros: "0".into(),
                return_cost_reserve_usd_micros: "5000000".into(),
                unwind: vec![withdrawal],
            },
        })
        .await;
    assert!(
        start.ok,
        "run.begin failed before a write: {:?}",
        start.error
    );
    let run_id = start.result.unwrap()["id"].as_str().unwrap().to_owned();

    for (request_id, operation, input) in [
        (
            "scoped-deposit-forward",
            "binance.deposit",
            json!({"requestId":"scoped-deposit-forward","account":"main","wallet":"synthetic-wallet","amount":"0.009","maxFeeEth":"0.001"}),
        ),
        (
            "scoped-withdraw-return",
            "binance.withdraw",
            json!({"requestId":"scoped-withdraw-return","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.009","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.009"}),
        ),
    ] {
        let response = broker
            .handle(BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: request_id.into(),
                    operation: operation.into(),
                    input,
                    run_id: Some(run_id.clone()),
                },
            })
            .await;
        assert!(response.ok, "{operation} failed: {:?}", response.error);
        let result = response.result.unwrap();
        assert_eq!(
            result["status"], "completed",
            "{operation} result: {result}"
        );
    }
    assert_eq!(
        rpc.accepted.load(Ordering::SeqCst),
        1,
        "the Ethereum deposit was broadcast exactly once"
    );
    assert_eq!(venue.state.lock().unwrap().balances["main"], "0.000");
    assert_eq!(*wallet_balance.lock().unwrap(), "0.099958");
    let finish = broker.handle(BrokerRequest::RunFinish { id: run_id }).await;
    assert!(
        finish.ok,
        "run.finish failed despite net venue and chain costs: {:?}",
        finish.error
    );
    assert_eq!(
        approval.0.load(Ordering::SeqCst),
        2,
        "each financial write used a fresh synthetic authorization"
    );
}

#[tokio::test]
async fn two_child_consolidation_withdrawal_and_deposit_redistribution_restores_full_allocation() {
    let venue = Arc::new(SyntheticVenue::new());
    // The selected main-account amount is a separately tracked withdrawal-fee
    // reserve; the two child allocations are returned in full.
    venue
        .state
        .lock()
        .unwrap()
        .balances
        .insert("main".into(), "0.001".into());
    let signer = fin_evm::LocalEnvelopeSigner::from_private_key(
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let address = format!("{:#x}", signer.address());
    let wallet = ResourceId(format!("ethereum:1:{address}"));
    let wallet_balance = Arc::new(Mutex::new("0.1".to_owned()));
    *venue.wallet_balance.lock().unwrap() = Some(wallet_balance.clone());
    let temp = tempfile::tempdir().unwrap();
    let store_path = temp.path().join("two-child-funding-roundtrip.sqlite");
    let transaction = Arc::new(Mutex::new(None));
    let journal = Arc::new(SyntheticSubmissionJournal::default());
    let rpc = Arc::new(DepositExecutionRpc {
        oracle_updated_at: synthetic_oracle_updated_at(),
        transaction: transaction.clone(),
        hash: Mutex::new(None),
        accepted: AtomicUsize::new(0),
        venue: venue.clone(),
        journal: journal.inner.clone(),
        durable_store_path: Some(store_path.clone()),
        wallet_address: address.clone(),
        wallet_balance: wallet_balance.clone(),
        payout_amount: "0.015".into(),
    });
    let ethereum = Arc::new(
        fin_ethereum::EthereumAdapter::new(
            fin_ethereum::EthereumConfig::default(),
            vec![fin_ethereum::WalletConnection {
                name: "synthetic-wallet".into(),
                address: address.clone(),
                signing_enabled: true,
            }],
            Arc::new(fin_evm::EvmRuntime::new(
                rpc.clone(),
                1,
                Some(Arc::new(RecordingSigner {
                    signer,
                    transaction,
                })),
            )),
        )
        .unwrap(),
    );
    let payout = Arc::new(
        fin_broker::EthereumPayoutVerifier::new(BTreeMap::from([(1, ethereum.clone())])).unwrap(),
    );
    let mut binance_config = adapter_config();
    binance_config.destinations[0].address = address.clone();
    binance_config.destinations[0].resource = wallet.clone();
    let binance = Arc::new(
        BinanceAdapter::new(
            binance_config,
            venue.clone(),
            Some(Arc::new(SyntheticBinancePrice)),
            Some(payout),
        )
        .unwrap(),
    );
    let composite = Arc::new(fin_broker::BinanceEthereumDepositAdapter::new(
        binance.clone(),
        ethereum,
    ));
    let wallet_observer = Arc::new(WalletSnapshotObserver {
        resource: wallet.clone(),
        address: address.clone(),
        balance: wallet_balance.clone(),
    });
    let mut config = runtime_config();
    for connection in &mut config.connections {
        if let ConnectionConfig::Ethereum {
            resource,
            address: configured_address,
            ..
        } = connection
        {
            *resource = wallet.clone();
            *configured_address = address.clone();
        }
    }
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = compose_runtime(
        config,
        RuntimeDependencies {
            store: Store::open(&store_path).unwrap(),
            adapters: vec![binance, composite, wallet_observer],
            authorizer: approval.clone(),
            valuator: Arc::new(SyntheticValuator),
        },
    )
    .unwrap();

    let collect_a = step("journey-a-to-main", CHILD_A, MAIN, "0.005");
    let collect_b = step("journey-b-to-main", CHILD_B, MAIN, "0.010");
    let withdrawal = PlanStep {
        operation: "binance.withdraw".into(),
        input: json!({"requestId":"journey-withdraw","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.015","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.015"}),
        maximum_fee_usd_micros: "2000000".into(),
        description: "Withdraw the consolidated native ETH to the enrolled wallet".into(),
    };
    let deposit = PlanStep {
        operation: "binance.deposit".into(),
        input: json!({"requestId":"scoped-deposit-forward","account":"main","wallet":"synthetic-wallet","amount":"0.015","maxFeeEth":"0.001"}),
        maximum_fee_usd_micros: "2000000".into(),
        description: "Return native ETH to the same Binance main account".into(),
    };
    let restore_a = step("journey-main-to-a", MAIN, CHILD_A, "0.005");
    let restore_b = step("journey-main-to-b", MAIN, CHILD_B, "0.010");
    let start = broker
        .handle(BrokerRequest::RunBegin {
            request: RunBeginRequest {
                allocations: vec![
                    ResourceAmount {
                        resource: resource(1002),
                        amount: Amount {
                            asset: asset(),
                            atoms: Quantity::from_decimal("0.005", 36).unwrap(),
                        },
                    },
                    ResourceAmount {
                        resource: resource(1003),
                        amount: Amount {
                            asset: asset(),
                            atoms: Quantity::from_decimal("0.010", 36).unwrap(),
                        },
                    },
                    ResourceAmount {
                        resource: resource(1001),
                        amount: Amount {
                            asset: asset(),
                            atoms: Quantity::from_decimal("0.001", 36).unwrap(),
                        },
                    },
                    ResourceAmount {
                        resource: wallet.clone(),
                        amount: Amount {
                            asset: native_asset(),
                            atoms: Quantity::from_decimal("0.02", 18).unwrap(),
                        },
                    },
                ],
                planned_operations: vec![collect_a, collect_b, withdrawal],
                borrowed_proceeds_reserve_usd_micros: "0".into(),
                return_cost_reserve_usd_micros: "5000000".into(),
                unwind: vec![deposit, restore_a, restore_b],
            },
        })
        .await;
    assert!(start.ok, "run.begin failed before write: {:?}", start.error);
    let run_id = start.result.unwrap()["id"].as_str().unwrap().to_owned();

    for (id, operation, input) in [
        (
            "journey-a-to-main",
            "binance.transfer",
            transfer("journey-a-to-main", CHILD_A, MAIN, "0.005"),
        ),
        (
            "journey-b-to-main",
            "binance.transfer",
            transfer("journey-b-to-main", CHILD_B, MAIN, "0.010"),
        ),
        (
            "journey-withdraw",
            "binance.withdraw",
            json!({"requestId":"journey-withdraw","account":"main","destination":"synthetic-wallet","coin":"ETH","amount":"0.015","network":"ETH","walletType":"SPOT","maxFee":"0.001","minReceiveAmount":"0.015"}),
        ),
        (
            "scoped-deposit-forward",
            "binance.deposit",
            json!({"requestId":"scoped-deposit-forward","account":"main","wallet":"synthetic-wallet","amount":"0.015","maxFeeEth":"0.001"}),
        ),
        (
            "journey-main-to-a",
            "binance.transfer",
            transfer("journey-main-to-a", MAIN, CHILD_A, "0.005"),
        ),
        (
            "journey-main-to-b",
            "binance.transfer",
            transfer("journey-main-to-b", MAIN, CHILD_B, "0.010"),
        ),
    ] {
        let response = broker
            .handle(BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: id.into(),
                    operation: operation.into(),
                    input,
                    run_id: Some(run_id.clone()),
                },
            })
            .await;
        assert!(response.ok, "{operation} {id} failed: {:?}", response.error);
        assert_eq!(response.result.unwrap()["status"], "completed", "{id}");
    }
    assert_eq!(rpc.accepted.load(Ordering::SeqCst), 1);
    let finish = broker.handle(BrokerRequest::RunFinish { id: run_id }).await;
    assert!(finish.ok, "run.finish failed: {:?}", finish.error);
    assert_eq!(finish.result.unwrap()["status"], "restored");
    let venue_state = venue.state.lock().unwrap();
    assert_eq!(venue_state.balances[EMAIL_A], "0.005");
    assert_eq!(venue_state.balances[EMAIL_B], "0.010");
    assert_eq!(venue_state.balances["main"], "0.000");
    assert_eq!(venue_state.posts, 5);
    assert_eq!(*wallet_balance.lock().unwrap(), "0.099958");
    assert_eq!(approval.0.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn partial_second_consolidation_failure_replans_and_restores_first_child() {
    let venue = Arc::new(SyntheticVenue::new());
    let failed_id = fin_binance::ClientIdentity::transfer("consolidate-b").unwrap();
    venue.state.lock().unwrap().reject_request_id = Some(failed_id.as_str().into());
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("partial.sqlite");
    let approval = Arc::new(SyntheticApproval(AtomicUsize::new(0)));
    let broker = runtime(&db, venue.clone(), approval.clone());
    let start = broker
        .handle(BrokerRequest::RunBegin {
            request: RunBeginRequest {
                allocations: vec![
                    ResourceAmount {
                        resource: resource(1002),
                        amount: Amount {
                            asset: asset(),
                            atoms: Quantity::from_decimal("0.005", 36).unwrap(),
                        },
                    },
                    ResourceAmount {
                        resource: resource(1003),
                        amount: Amount {
                            asset: asset(),
                            atoms: Quantity::from_decimal("0.010", 36).unwrap(),
                        },
                    },
                ],
                planned_operations: vec![
                    step("consolidate-a", CHILD_A, MAIN, "0.005"),
                    step("consolidate-b", CHILD_B, MAIN, "0.010"),
                ],
                borrowed_proceeds_reserve_usd_micros: "0".into(),
                return_cost_reserve_usd_micros: "0".into(),
                unwind: vec![
                    step("return-a", MAIN, CHILD_A, "0.005"),
                    step("return-b", MAIN, CHILD_B, "0.010"),
                ],
            },
        })
        .await;
    assert!(start.ok, "{:?}", start.error);
    let run_id = start.result.unwrap()["id"].as_str().unwrap().to_owned();
    execute(
        &broker,
        &run_id,
        "consolidate-a",
        transfer("consolidate-a", CHILD_A, MAIN, "0.005"),
    )
    .await;
    let failed_response = broker
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "consolidate-b".into(),
                operation: "binance.transfer".into(),
                input: transfer("consolidate-b", CHILD_B, MAIN, "0.010"),
                run_id: Some(run_id.clone()),
            },
        })
        .await;
    assert!(failed_response.ok, "{:?}", failed_response.error);
    let uncertain = failed_response.result.unwrap();
    assert_eq!(uncertain["status"], "unknown");
    let reconciled = broker
        .handle(BrokerRequest::Status {
            id: uncertain["id"].as_str().unwrap().into(),
        })
        .await;
    assert!(reconciled.ok, "{:?}", reconciled.error);
    assert_eq!(reconciled.result.unwrap()["status"], "failed");

    let revision = broker
        .handle(BrokerRequest::RunReplan {
            id: run_id.clone(),
            remaining: vec![step("return-a", MAIN, CHILD_A, "0.005")],
        })
        .await;
    assert!(revision.ok, "{:?}", revision.error);
    execute(
        &broker,
        &run_id,
        "return-a",
        transfer("return-a", MAIN, CHILD_A, "0.005"),
    )
    .await;
    let finished = broker.handle(BrokerRequest::RunFinish { id: run_id }).await;
    assert!(finished.ok, "{:?}", finished.error);
    let finished_value = finished.result.unwrap();
    assert_eq!(finished_value["status"], "restored", "{finished_value}");
    let state = venue.state.lock().unwrap();
    assert_eq!(state.balances[EMAIL_A], "0.005");
    assert_eq!(state.balances[EMAIL_B], "0.010");
    assert_eq!(state.balances["main"], "0.000");
    assert_eq!(
        state.posts, 3,
        "failed accepted identity plus first/cleanup writes are not replayed"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
}
