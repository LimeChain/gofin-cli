//! Real CLI/broker Kraken cancellation with correlated fills and unrelated activity.
#[path = "support/outcome_evidence.rs"]
mod outcome_evidence;
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::{clock::Clock, *};
use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use gofin_kraken::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use gofin_protocol::BrokerRequest;
use gofin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic native approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct ExpiryApproval(AtomicUsize);
#[async_trait]
impl Authorizer for ExpiryApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        Ok(ApprovalAudit {
            mechanism: "synthetic native approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct ExpiringOrderAdapter {
    inner: ExchangeAdapter<KrakenProfile>,
    preparations: AtomicUsize,
}
#[async_trait]
impl Adapter for ExpiringOrderAdapter {
    fn definitions(&self) -> Vec<Definition> {
        self.inner.definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.inner.connections()
    }
    fn authorization_identities(&self) -> Vec<ConnectionMetadata> {
        self.inner.authorization_identities()
    }
    fn supports_approved_reprepare(&self, p: &PreparedOperation) -> bool {
        self.inner.supports_approved_reprepare(p)
    }
    fn supports_independent_submission(&self, p: &PreparedOperation) -> bool {
        self.inner.supports_independent_submission(p)
    }
    async fn read(&self, op: &str, input: &Value) -> Result<Value> {
        self.inner.read(op, input).await
    }
    async fn prepare(&self, op: &str, input: &Value) -> Result<PreparedOperation> {
        let mut p = self.inner.prepare(op, input).await?;
        if input.get("requestId").and_then(Value::as_str) == Some("quote-expiry")
            && self.preparations.fetch_add(1, Ordering::SeqCst) == 0
        {
            p.expires_at = (chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
        }
        Ok(p)
    }
    async fn revalidate(&self, p: &PreparedOperation) -> Result<PreparedOperation> {
        self.inner.revalidate(p).await
    }
    async fn submit(
        &self,
        p: &PreparedOperation,
        j: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.inner.submit(p, j).await
    }
    async fn reconcile(&self, r: &OperationRecord) -> Result<Reconciliation> {
        self.inner.reconcile(r).await
    }
}
struct Prices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: if asset.reference == "XXBT" {
                "20000"
            } else {
                "1"
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic".into(),
        })
    }
}
#[derive(Default)]
struct Venue {
    orders: Mutex<BTreeMap<String, (String, Value)>>,
    writes: AtomicUsize,
    unrelated_balance_changes: AtomicUsize,
    stall_first: std::sync::atomic::AtomicBool,
    lose_first: std::sync::atomic::AtomicBool,
    lose_after_accept: std::sync::atomic::AtomicBool,
    pause_preparation: std::sync::atomic::AtomicBool,
    missing_market_metadata: std::sync::atomic::AtomicBool,
    preparation_entered: std::sync::atomic::AtomicBool,
    pause_after_accept: std::sync::atomic::AtomicBool,
    after_accept_entered: std::sync::atomic::AtomicBool,
    release_fault: std::sync::atomic::AtomicBool,
    finish_first: std::sync::atomic::AtomicBool,
    first_arrived: tokio::sync::Notify,
    release_first: tokio::sync::Notify,
}
#[async_trait]
impl KrakenTransport for Venue {
    fn supports_independent_submission(&self) -> bool {
        true
    }
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e.mutating() {
            assert!(guard.expect("independent dispatch guard")());
        } else {
            assert!(guard.is_none());
        }
        let account = a.map(|a| a.name.as_str()).unwrap_or("");
        let result = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades"],"valid_until":"0","iban":format!("SYNTHETIC-{account}")})
            }
            E::KrakenPublicGetAssets => json!({"XXBT":{"decimals":10},"ZUSD":{"decimals":4}}),
            E::KrakenPublicGetAssetPairs => {
                if self.missing_market_metadata.load(Ordering::SeqCst) {
                    return Err(GoFinError::new(
                        "KRAKEN_MARKET_METADATA_UNAVAILABLE",
                        "prepare",
                        "Synthetic required market metadata is unavailable.",
                        "Retry when the venue metadata is available.",
                    ));
                }
                if self.pause_preparation.load(Ordering::SeqCst) {
                    self.preparation_entered.store(true, Ordering::SeqCst);
                    while !self.release_fault.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
                json!({"XXBTZUSD":{"altname":"XBTUSD","base":"XXBT","quote":"ZUSD","status":"online","lot_decimals":8,"pair_decimals":1,"cost_decimals":5,"ordermin":"0.0001"}})
            }
            E::KrakenPublicGetTicker => {
                json!({"XXBTZUSD":{"b":["20000","1","1"],"a":["20001","1","1"]}})
            }
            E::KrakenPrivatePostTradeVolume => {
                json!({"fees":{"XXBTZUSD":{"fee":"0.4"}},"fees_maker":{"XXBTZUSD":{"fee":"0.25"}}})
            }
            E::KrakenPrivatePostBalanceEx => {
                json!({"XXBT":{"balance":"1","hold_trade":"0"},"ZUSD":{"balance":if self.unrelated_balance_changes.load(Ordering::SeqCst)>0 {"700"} else {"1000"},"hold_trade":"0"}})
            }
            E::KrakenPrivatePostBalance => json!({"XXBT":"1","ZUSD":"1000"}),
            E::KrakenPrivatePostOpenPositions | E::KrakenPrivatePostCreditLines => json!({}),
            E::KrakenPrivatePostEarnAllocations => json!({"items":[]}),
            E::KrakenPrivatePostAddOrder => {
                let n = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 && self.stall_first.load(Ordering::SeqCst) {
                    self.first_arrived.notify_one();
                    self.release_first.notified().await;
                }
                if n == 1 && self.lose_first.load(Ordering::SeqCst) {
                    return Err(gofin_domain::GoFinError::new(
                        "KRAKEN_RESPONSE_TIMEOUT",
                        "transport",
                        "Synthetic response lost.",
                        "Observe the original order.",
                    ));
                }
                let id = format!("ORDER-{n}");
                let mut row = json!({"cl_ord_id":p["cl_ord_id"],"opentm":format!("{}.{:09}",chrono::Utc::now().timestamp(),chrono::Utc::now().timestamp_subsec_nanos()),
                    "descr":{"pair":"XBTUSD","type":"buy","price":"20000"},"status":"open","vol":"0.01","vol_exec":"0.005","cost":"100","fee":"0.2","trades":[format!("TRADE-{id}")]});
                if n == 1 && self.finish_first.load(Ordering::SeqCst) {
                    row["status"] = json!("canceled");
                }
                self.orders
                    .lock()
                    .unwrap()
                    .insert(id.clone(), (account.into(), row));
                if n == 1 && self.pause_after_accept.load(Ordering::SeqCst) {
                    self.after_accept_entered.store(true, Ordering::SeqCst);
                    while !self.release_fault.load(Ordering::SeqCst) {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
                if n == 1 && self.lose_after_accept.load(Ordering::SeqCst) {
                    return Err(GoFinError::new(
                        "KRAKEN_RESPONSE_TIMEOUT",
                        "transport",
                        "Synthetic response lost after acceptance.",
                        "Observe the original order.",
                    ));
                }
                json!({"txid":[id]})
            }
            E::KrakenPrivatePostAmendOrder => {
                self.writes.fetch_add(1, Ordering::SeqCst);
                assert_eq!(p["txid"], "ORDER-1");
                return Err(gofin_domain::GoFinError::new(
                    "KRAKEN_RESPONSE_TIMEOUT",
                    "transport",
                    "Synthetic amendment response lost.",
                    "Observe the original amendment.",
                ));
            }
            E::KrakenPrivatePostOrderAmends => json!({"amends":[]}),
            E::KrakenPrivatePostCancelOrder | E::KrakenPrivatePostCancelOrderBatch => {
                self.writes.fetch_add(1, Ordering::SeqCst);
                let mut orders = self.orders.lock().unwrap();
                let id = p["txid"]
                    .as_str()
                    .or_else(|| p["orders"][0]["txid"].as_str())
                    .expect("exact cancellation identity");
                let (owner, row) = orders.get_mut(id).unwrap();
                assert_eq!(owner, account);
                row["status"] = json!("canceled");
                json!({"count":1,"pending":false})
            }
            E::KrakenPrivatePostQueryOrders
            | E::KrakenPrivatePostOpenOrders
            | E::KrakenPrivatePostClosedOrders => {
                let rows: serde_json::Map<String, Value> = self
                    .orders
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(id, (owner, row))| {
                        owner == account
                            && match e {
                                E::KrakenPrivatePostQueryOrders => p["txid"]
                                    .as_str()
                                    .is_none_or(|ids| ids.split(',').any(|x| x == *id)),
                                E::KrakenPrivatePostOpenOrders => row["status"] == "open",
                                _ => row["status"] != "open",
                            }
                    })
                    .map(|(id, (_, row))| (id.clone(), row.clone()))
                    .collect();
                match e {
                    E::KrakenPrivatePostOpenOrders => json!({"open":rows}),
                    E::KrakenPrivatePostClosedOrders => json!({"closed":rows,"count":rows.len()}),
                    _ => json!(rows),
                }
            }
            E::KrakenPrivatePostQueryTrades => {
                let mut rows = serde_json::Map::new();
                for (id, (owner, _)) in self
                    .orders
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, (owner, _))| owner == account)
                {
                    let _ = owner;
                    rows.insert(format!("TRADE-{id}"),json!({"ordertxid":id,"pair":"XXBTZUSD","type":"buy","vol":"0.005","cost":"100","fee":"0.2","margin":"0","leverage":"0","ledgers":[format!("BASE-{id}"),format!("QUOTE-{id}")]}));
                }
                json!(rows)
            }
            E::KrakenPrivatePostQueryLedgers => {
                let mut rows = serde_json::Map::new();
                for (id, _) in self
                    .orders
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, (owner, _))| owner == account)
                {
                    rows.insert(format!("BASE-{id}"),json!({"refid":format!("TRADE-{id}"),"type":"trade","asset":"XXBT","amount":"0.005","fee":"0"}));
                    rows.insert(format!("QUOTE-{id}"),json!({"refid":format!("TRADE-{id}"),"type":"trade","asset":"ZUSD","amount":"-100","fee":"0.2"}));
                }
                json!(rows)
            }
            _ => panic!("unexpected controlled endpoint {e:?}"),
        };
        Ok(KrakenResponse::Json(json!({"error":[],"result":result})))
    }
}

#[test]
fn missing_kraken_order_stays_uncertain_without_replay() {
    std::thread::Builder::new().stack_size(24 * 1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24 * 1024 * 1024).enable_all().build().unwrap().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("journal.sqlite");
            let venue = Arc::new(Venue::default());
            venue.lose_first.store(true, Ordering::SeqCst);
            let approval = Arc::new(Approval(AtomicUsize::new(0)));
            let client = Client::new(runtime(&path, venue.clone(), approval.clone()), dir.path()).await;
            let input = order_input("missing-order", "spot");
            let before_action = (approval.0.load(Ordering::SeqCst), venue.writes.load(Ordering::SeqCst));
            let original = execute(&client, "missing-order", E::KrakenPrivatePostAddOrder, input.clone()).await;
            let after_action = (approval.0.load(Ordering::SeqCst), venue.writes.load(Ordering::SeqCst));
            let id = original["id"].as_str().unwrap().to_owned();
            assert!(original["submission"].is_object(), "{original}");
            let observed = status(&client, &id).await;
            assert_eq!(observed["outcome"]["dispatch"], "uncertain", "{observed}");
            assert_eq!(observed["outcome"]["replay_safe"], false);
            let read=client.call(args(&["read",E::KrakenPrivatePostBalance.operation(),"--input",&json!({"account":"spot"}).to_string(),"--json"]),BrokerRequest::Read{operation:E::KrakenPrivatePostBalance.operation().into(),input:json!({"account":"spot"})},true).await;
            assert!(!read.is_null(), "{read}");
            let mut probes = Vec::new();
            for (scope, request, account) in [("same_account", "missing-same", "spot"), ("different_account", "missing-other", "other")] {
                let before = (approval.0.load(Ordering::SeqCst), venue.writes.load(Ordering::SeqCst));
                let probe = execute(&client, request, E::KrakenPrivatePostAddOrder, order_input(request, account)).await;
                assert!(probe["submission"].is_object(), "{probe}");
                let after = (approval.0.load(Ordering::SeqCst), venue.writes.load(Ordering::SeqCst));
                probes.push(outcome_evidence::admission_probe(scope,before,after,&probe));
            }
            drop(client);
            let client = Client::new(runtime(&path, venue.clone(), approval.clone()), dir.path()).await;
            let repeated = execute(&client, "missing-order", E::KrakenPrivatePostAddOrder, input).await;
            assert_eq!(repeated["id"], id);
            assert_eq!(repeated["outcome"]["replay_safe"], false);
            assert_eq!(venue.writes.load(Ordering::SeqCst), 3);
            assert_eq!(approval.0.load(Ordering::SeqCst), 3);
            if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
                assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
                let mut evidence=json!({"scenario":std::env::var("GOFIN_OUTCOME_SCENARIO").unwrap_or_else(|_|"outcome.a13.kraken".into()),"approval_count":after_action.0-before_action.0,"external_submission_count":after_action.1-before_action.1,"intervention_count":0,"remaining_external_uncertainty":true});
                outcome_evidence::attach_probes(&mut evidence,&probes);
                std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
            }
        });
    }).unwrap().join().unwrap();
}

#[test]
fn stalled_kraken_order_does_not_block_independent_cli_orders() {
    std::thread::Builder::new()
        .stack_size(24 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(24 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let dir = tempfile::tempdir().unwrap();
                    let venue = Arc::new(Venue::default());
                    venue.stall_first.store(true, Ordering::SeqCst);
                    let approval = Arc::new(Approval(AtomicUsize::new(0)));
                    let client = Arc::new(
                        Client::new(
                            runtime(
                                &dir.path().join("journal.sqlite"),
                                venue.clone(),
                                approval.clone(),
                            ),
                            dir.path(),
                        )
                        .await,
                    );
                    let first_client = client.clone();
                    let first = tokio::spawn(async move {
                        execute(
                            &first_client,
                            "stalled",
                            E::KrakenPrivatePostAddOrder,
                            order_input("stalled", "spot"),
                        )
                        .await
                    });
                    tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        venue.first_arrived.notified(),
                    )
                    .await
                    .unwrap();
                    let action_counts=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                    let mut probes=Vec::new();
                    let mut ids = std::collections::BTreeSet::new();
                    for (scope, id, account) in [
                        ("same_account", "same-account-stall", "spot"),
                        ("different_account", "other-account-stall", "other"),
                    ] {
                        let before=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            execute(
                                &client,
                                id,
                                E::KrakenPrivatePostAddOrder,
                                order_input(id, account),
                            ),
                        )
                        .await
                        .expect("independent action blocked");
                        assert!(result["submission"].is_object(), "{result}");
                        let after=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                        probes.push(outcome_evidence::admission_probe(scope,before,after,&result));
                        assert!(ids.insert(result["id"].as_str().unwrap().to_owned()));
                    }
                    assert_eq!(venue.writes.load(Ordering::SeqCst), 3);
                    venue.release_first.notify_one();
                    let result = first.await.unwrap();
                    assert!(result["submission"].is_object(), "{result}");
                    assert!(ids.insert(result["id"].as_str().unwrap().to_owned()));
                    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
                    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
                        assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
                        let mut evidence=json!({"scenario":"outcome.a6.kraken","approval_count":action_counts.0,"external_submission_count":action_counts.1,"intervention_count":0,"remaining_external_uncertainty":true});
                        outcome_evidence::attach_probes(&mut evidence,&probes);
                        std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
                    }
                });
        })
        .unwrap()
        .join()
        .unwrap();
}
include!("support/kraken_plan_client.rs");
fn runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    approval: Arc<dyn Authorizer>,
) -> Arc<gofin_broker::BrokerRuntime> {
    runtime_with_clock(path, venue, approval, Arc::new(TestClock))
}
fn runtime_with_clock(
    path: &std::path::Path,
    venue: Arc<Venue>,
    approval: Arc<dyn Authorizer>,
    clock: Arc<dyn Clock>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let accounts = ["spot", "other"].map(|name| KrakenAccount {
        name: name.into(),
        resource: ResourceId(format!("kraken:{name}")),
        credential_ref: format!("synthetic-{name}"),
        physical_identity: format!("SYNTHETIC-{name}"),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, accounts.into(), venue)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    );
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
            adapters: vec![Arc::new(adapter)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        clock,
    )
    .unwrap()
}

#[test]
fn expired_kraken_candidate_refreshes_with_one_approval() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("expiry.sqlite");
            let venue=Arc::new(Venue::default()); let approval=Arc::new(ExpiryApproval(AtomicUsize::new(0)));
            let accounts=["spot","other"].map(|name| KrakenAccount{name:name.into(),resource:ResourceId(format!("kraken:{name}")),credential_ref:format!("synthetic-{name}"),physical_identity:format!("SYNTHETIC-{name}"),wallet_account_id:None,default_wallet_account_id:None,identity_verified:true,product:KrakenProduct::Spot});
            let inner=ExchangeAdapter::new(KrakenProfile::new(KrakenProduct::Spot,accounts.into(),venue.clone()).unwrap().with_prices(Arc::new(Prices)));
            let wrapper=Arc::new(ExpiringOrderAdapter{inner,preparations:AtomicUsize::new(0)});
            let runtime=compose_runtime_with_clock(BrokerConfigV2{version:2,networks:NetworkConfig::reviewed_evm_networks(),deployments:vec![],connections:vec![],policy:BrokerPolicy::beta(),approver_uid:unsafe{libc::geteuid()},extensions:BTreeMap::new()},RuntimeDependencies{store:Store::open(&path).unwrap(),adapters:vec![wrapper.clone()],authorizer:approval.clone(),valuator:Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),bridge:None},Arc::new(TestClock)).unwrap();
            let client=Client::new(runtime,dir.path()).await;
            let input=order_input("quote-expiry","spot");
            let before_action=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
            let original=execute(&client,"quote-expiry",E::KrakenPrivatePostAddOrder,input.clone()).await;
            let after_action=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
            assert!(original["submission"].is_object(),"{original}");
            assert!(wrapper.preparations.load(Ordering::SeqCst)>=2);
            assert_eq!(approval.0.load(Ordering::SeqCst),1);
            assert_eq!(venue.writes.load(Ordering::SeqCst),1);
            assert_eq!(execute(&client,"quote-expiry",E::KrakenPrivatePostAddOrder,input).await["id"],original["id"]);
            let mut probes=Vec::new();
            for (scope,id,account) in [("same_account","expiry-same","spot"),("different_account","expiry-other","other")] {
                let before=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                let probe=execute(&client,id,E::KrakenPrivatePostAddOrder,order_input(id,account)).await;
                let after=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                probes.push(outcome_evidence::admission_probe(scope,before,after,&probe));
            }
            assert_eq!(approval.0.load(Ordering::SeqCst),3); assert_eq!(venue.writes.load(Ordering::SeqCst),3);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") { assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some()); let mut evidence=json!({"scenario":"outcome.a9.kraken","approval_count":after_action.0-before_action.0,"external_submission_count":after_action.1-before_action.1,"intervention_count":0,"remaining_external_uncertainty":true}); outcome_evidence::attach_probes(&mut evidence,&probes); std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap(); }
        });
    }).unwrap().join().unwrap();
}
async fn execute(client: &Client, id: &str, e: E, input: Value) -> Value {
    client
        .call(
            vec![
                "execute".into(),
                e.operation().into(),
                "--id".into(),
                id.into(),
                "--input".into(),
                input.to_string(),
                "--json".into(),
            ],
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: id.into(),
                    operation: e.operation().into(),
                    input,
                    run_id: None,
                },
            },
            true,
        )
        .await
}
async fn status(client: &Client, id: &str) -> Value {
    client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await
}
fn order_input(id: &str, account: &str) -> Value {
    json!({"requestId":id,"account":account,"pair":"XBTUSD","type":"buy","ordertype":"limit","volume":"0.01","price":"20000","maximumFees":{"XXBT":"0.0001","ZUSD":"1"},"timeinforce":"GTC"})
}
#[test]
fn partial_kraken_cancellation_retains_fills_without_blocking_other_orders() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("journal.sqlite");
            let venue=Arc::new(Venue::default()); let approval=Arc::new(Approval(AtomicUsize::new(0)));
            let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
            let input=order_input("partial","spot");
            let before_action=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
            let original=execute(&client,"partial",E::KrakenPrivatePostAddOrder,input.clone()).await;
            let id=original["id"].as_str().unwrap();
            let partial=status(&client,id).await;
            assert_eq!(partial["outcome"]["result"],"partial","{partial}");
            venue.unrelated_balance_changes.fetch_add(1,Ordering::SeqCst);
            // Probe while the original remainder is live, not only after cancellation.
            let mut probes=Vec::new();
            for (scope,request,account) in [("same_account","same-account","spot"),("different_account","different-account","other")] {
                let before=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                let probe=execute(&client,request,E::KrakenPrivatePostAddOrder,order_input(request,account)).await;
                assert!(probe["submission"].is_object(),"{probe}");
                let after=(approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                probes.push(outcome_evidence::admission_probe(scope,before,after,&probe));
            }
            let edit=execute(&client,"edit",E::KrakenPrivatePostAmendOrder,json!({"requestId":"edit","account":"spot","operationId":id,"originalSubmission":original["submission"],"txid":"ORDER-1","order_qty":"0.01","limit_price":"21000","maximumFees":{"XXBT":"0.0001","ZUSD":"1"}})).await;
            assert!(edit["submission"].is_object(), "{edit}");
            assert_eq!(edit["outcome"]["dispatch"],"uncertain", "{edit}");
            let approvals_before_replacement=approval.0.load(Ordering::SeqCst);
            let writes_before_replacement=venue.writes.load(Ordering::SeqCst);
            let replacement=execute(&client,"edit-again",E::KrakenPrivatePostAmendOrder,json!({"requestId":"edit-again","account":"spot","operationId":id,"originalSubmission":original["submission"],"txid":"ORDER-1","order_qty":"0.01","limit_price":"22000","maximumFees":{"XXBT":"0.0001","ZUSD":"1"}})).await;
            assert!(replacement["submission"].is_null(), "uncertain amendment must block a replacement: {replacement}");
            assert_eq!(venue.writes.load(Ordering::SeqCst),writes_before_replacement);
            assert_eq!(approval.0.load(Ordering::SeqCst),approvals_before_replacement);
            let live=status(&client,id).await;
            assert_eq!(live["outcome"]["result"],"partial", "{live}");
            let cancel=execute(&client,"cancel",E::KrakenPrivatePostCancelOrder,json!({"requestId":"cancel","account":"spot","operationId":id,"originalSubmission":original["submission"],"txid":"ORDER-1"})).await;
            let cancel=status(&client,cancel["id"].as_str().unwrap()).await;
            assert_eq!(cancel["outcome"]["phase"],"done","{cancel}");
            let terminal=status(&client,id).await;
            assert_eq!(terminal["outcome"]["phase"],"done","{terminal}");
            assert_eq!(terminal["outcome"]["result"],"executed","{terminal}");
            assert_eq!(terminal["outcome"]["replay_safe"],false);
            assert_eq!(partial["effects"]["effects"][0]["received"]["atoms"], Quantity::from_decimal("0.005",36).unwrap().to_string());
            assert_eq!(terminal["effects"]["effects"],partial["effects"]["effects"]);
            drop(client);
            let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
            let repeated=execute(&client,"partial",E::KrakenPrivatePostAddOrder,input).await;
            assert_eq!(repeated["id"],id);
            assert_eq!(approval.0.load(Ordering::SeqCst),5);
            assert_eq!(venue.writes.load(Ordering::SeqCst),5);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
                assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
                let probe_approvals:usize=probes.iter().map(|p|p["approval_count"].as_u64().unwrap() as usize).sum();
                let probe_writes:usize=probes.iter().map(|p|p["external_submission_count"].as_u64().unwrap() as usize).sum();
                let mut evidence=json!({"scenario":"outcome.a19.kraken","approval_count":approval.0.load(Ordering::SeqCst)-before_action.0-probe_approvals,"external_submission_count":venue.writes.load(Ordering::SeqCst)-before_action.1-probe_writes,"intervention_count":0,"remaining_external_uncertainty":true});
                outcome_evidence::attach_probes(&mut evidence,&probes); std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
            }
        });
    }).unwrap().join().unwrap();
}

#[test]
fn accepted_order_lost_response_reconciles_without_replay_or_observation_approval() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("accepted-loss.sqlite");
            let venue=Arc::new(Venue::default()); venue.lose_after_accept.store(true,Ordering::SeqCst);
            let approval=Arc::new(Approval(AtomicUsize::new(0))); let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
            let counters=|| (approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
            let before=counters(); let input=order_input("accepted-loss","spot");
            let original=execute(&client,"accepted-loss",E::KrakenPrivatePostAddOrder,input.clone()).await; let after=counters();
            assert!(original["submission"].is_object(),"{original}"); let id=original["id"].as_str().unwrap();
            let mut probes=Vec::new();
            for (scope,name,account) in [("same_account","accepted-same","spot"),("different_account","accepted-other","other")] {
                let before=counters(); let probe=execute(&client,name,E::KrakenPrivatePostAddOrder,order_input(name,account)).await;
                probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));
            }
            drop(client);
            let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
            let before_observe=counters(); let observed=status(&client,id).await;
            assert_eq!(observed["outcome"]["result"],"partial","{observed}");
            venue.orders.lock().unwrap().get_mut("ORDER-1").unwrap().1["status"]=json!("canceled");
            let terminal=status(&client,id).await; assert_eq!(terminal["outcome"]["phase"],"done","{terminal}");
            assert_eq!(counters(),before_observe);
            let repeated=execute(&client,"accepted-loss",E::KrakenPrivatePostAddOrder,input).await;
            assert_eq!(repeated["submission"],original["submission"]); assert_eq!(counters(),before_observe);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") { let mut evidence=json!({"scenario":std::env::var("GOFIN_OUTCOME_SCENARIO").unwrap_or_else(|_|"outcome.a2.kraken".into()),"approval_count":after.0-before.0,"external_submission_count":after.1-before.1,"intervention_count":0,"remaining_external_uncertainty":false}); outcome_evidence::attach_probes(&mut evidence,&probes); std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap(); }
        });
    }).unwrap().join().unwrap();
}

#[test]
fn pending_exchange_order_polls_with_guidance_and_does_not_block_other_actions() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("pending-poll.sqlite");
            let venue=Arc::new(Venue::default()); let approval=Arc::new(Approval(AtomicUsize::new(0))); let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
            let counters=|| (approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
            let before=counters(); let original=execute(&client,"pending-poll",E::KrakenPrivatePostAddOrder,order_input("pending-poll","spot")).await; let after=counters();
            let id=original["id"].as_str().unwrap(); let observation=status(&client,id).await;
            assert_eq!(observation["outcome"]["phase"],"in_progress","{observation}");
            assert!(observation["outcome"]["next_check_after"].is_string(),"{observation}");
            assert_eq!(counters(),after);
            let mut probes=Vec::new();
            for (scope,name,account) in [("same_account","pending-same","spot"),("different_account","pending-other","other")] {
                let before=counters(); let probe=execute(&client,name,E::KrakenPrivatePostAddOrder,order_input(name,account)).await;
                probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));
            }
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") { let mut evidence=json!({"scenario":"outcome.a3.kraken","approval_count":after.0-before.0,"external_submission_count":after.1-before.1,"intervention_count":0,"remaining_external_uncertainty":true}); outcome_evidence::attach_probes(&mut evidence,&probes); std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap(); }
        });
    }).unwrap().join().unwrap();
}

#[derive(Default)]
struct ReviewApproval {
    count: AtomicUsize,
    reviews: Mutex<Vec<gofin_domain::approval_review::ApprovalReview>>,
}
#[async_trait]
impl Authorizer for ReviewApproval {
    fn requires_financial_review(&self) -> bool {
        true
    }
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.reviews
            .lock()
            .unwrap()
            .push(c.review.clone().expect("financial review"));
        Ok(ApprovalAudit {
            mechanism: "synthetic".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
#[test]
fn uncertain_exchange_order_is_shown_on_later_same_account_approval() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("advisory.sqlite");let venue=Arc::new(Venue::default());venue.lose_first.store(true,Ordering::SeqCst);let approval=Arc::new(ReviewApproval::default());
            let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
            let counters=|| (approval.count.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
            let before=counters();let original=execute(&client,"advisory-original",E::KrakenPrivatePostAddOrder,order_input("advisory-original","spot")).await;let after=counters();
            let id=original["id"].as_str().unwrap();let original_observed=status(&client,id).await;assert_eq!(original_observed["outcome"]["dispatch"],"uncertain","{original_observed}");
            let mut probes=Vec::new();
            for (scope,name,account) in [("same_account","advisory-same","spot"),("different_account","advisory-other","other")] {
                let before=counters();let probe=execute(&client,name,E::KrakenPrivatePostAddOrder,order_input(name,account)).await;
                probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));
            }
            let reviews=approval.reviews.lock().unwrap();let advisory=reviews[1].advisories.iter().find(|a|a.operation_id==id).expect("original uncertain action shown to approver");assert!(!advisory.amounts.is_empty());drop(reviews);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") { let mut evidence=json!({"scenario":"outcome.a22.kraken","approval_count":after.0-before.0,"external_submission_count":after.1-before.1,"intervention_count":0,"remaining_external_uncertainty":true});outcome_evidence::attach_probes(&mut evidence,&probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap(); }
        });
    }).unwrap().join().unwrap();
}

struct AdjustableClock(Mutex<chrono::DateTime<chrono::Utc>>);
impl Clock for AdjustableClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().unwrap()
    }
}
#[test]
fn human_dismissed_exchange_order_remains_unsafe_to_replay() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("dismiss.sqlite");let venue=Arc::new(Venue::default());venue.lose_first.store(true,Ordering::SeqCst);
            let approval=Arc::new(Approval(AtomicUsize::new(0)));let clock=Arc::new(AdjustableClock(Mutex::new(chrono::Utc::now())));
            let client=Client::new(runtime_with_clock(&path,venue.clone(),approval.clone(),clock.clone()),dir.path()).await;
            let counters=|| (approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));let before=counters();
            let input=order_input("dismiss-original","spot");let original=execute(&client,"dismiss-original",E::KrakenPrivatePostAddOrder,input.clone()).await;let after=counters();let id=original["id"].as_str().unwrap();
            let mut probes=Vec::new();for (scope,name,account) in [("same_account","dismiss-same","spot"),("different_account","dismiss-other","other")] {let before=counters();let probe=execute(&client,name,E::KrakenPrivatePostAddOrder,order_input(name,account)).await;probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));}
            *clock.0.lock().unwrap()=chrono::Utc::now()+chrono::Duration::days(8);
            let unconfirmed=status(&client,id).await;assert_eq!(unconfirmed["outcome"]["phase"],"unconfirmed","{unconfirmed}");assert_eq!(unconfirmed["outcome"]["replay_safe"],false);
            let dismissed=client.runtime.engine.dismiss_operation(id).unwrap();assert_eq!(dismissed["human_dismissed"],true);assert_eq!(dismissed["outcome"]["replay_safe"],false);
            let open=client.call(args(&["operations","--open","--json"]),BrokerRequest::OpenOperations,true).await;assert!(!open.as_array().unwrap().iter().any(|row|row["id"]==id));
            let prior=counters();let replay=execute(&client,"dismiss-original",E::KrakenPrivatePostAddOrder,input).await;assert_eq!(replay["submission"],original["submission"]);assert_eq!(replay["outcome"]["replay_safe"],false);assert_eq!(counters(),prior);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a23.kraken","approval_count":after.0-before.0,"external_submission_count":after.1-before.1,"intervention_count":0,"human_action_count":1,"remaining_external_uncertainty":true});outcome_evidence::attach_probes(&mut evidence,&probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        });
    }).unwrap().join().unwrap();
}

#[test]
fn malformed_exchange_history_is_isolated_from_new_orders() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("malformed.sqlite");
            let mut store=Store::open(&path).unwrap();store.create_request("corrupt-history","corrupt-request","corrupt-fingerprint","kraken.add-order",None,"unknown",&json!({"id":"corrupt-history"}),&now()).unwrap();drop(store);
            let conn=rusqlite::Connection::open(&path).unwrap();conn.execute("UPDATE operations SET record_json='{' WHERE id='corrupt-history'",[]).unwrap();drop(conn);
            let venue=Arc::new(Venue::default());let approval=Arc::new(Approval(AtomicUsize::new(0)));let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
            let damaged=status(&client,"corrupt-history").await;assert_eq!(damaged["error"]["code"],"history_unreadable");assert_eq!(damaged["outcome"]["replay_safe"],false);
            let counters=|| (approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));let before=counters();let primary=execute(&client,"after-corrupt",E::KrakenPrivatePostAddOrder,order_input("after-corrupt","spot")).await;let after=counters();assert!(primary["submission"].is_object());
            let mut probes=Vec::new();for (scope,name,account) in [("same_account","corrupt-same","spot"),("different_account","corrupt-other","other")] {let before=counters();let probe=execute(&client,name,E::KrakenPrivatePostAddOrder,order_input(name,account)).await;probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));}
            let damaged=status(&client,"corrupt-history").await;assert_eq!(damaged["outcome"]["replay_safe"],false);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a5.kraken","approval_count":after.0-before.0,"external_submission_count":after.1-before.1,"intervention_count":0,"remaining_external_uncertainty":true});outcome_evidence::attach_probes(&mut evidence,&probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        });
    }).unwrap().join().unwrap();
}

#[derive(Default)]
struct RestartApproval {
    count: AtomicUsize,
    pause: std::sync::atomic::AtomicBool,
    entered: std::sync::atomic::AtomicBool,
    release: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl Authorizer for RestartApproval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.pause.load(Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        Ok(ApprovalAudit {
            mechanism: "synthetic".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
fn restart_cli(socket: &std::path::Path, argv: Vec<String>) -> tokio::process::Child {
    let binary = std::env::var("GOFIN_KRAKEN_PLAN_CLI").expect("public CLI required");
    tokio::process::Command::new(binary)
        .args([
            "--identity",
            "synthetic-kraken-plan",
            "--build-id",
            "synthetic-kraken-build",
        ])
        .args(argv)
        .env("GOFIN_DEV_IDENTITY", "synthetic-kraken-plan")
        .env("GOFIN_DEV_BUILD_ID", "synthetic-kraken-build")
        .env("GOFIN_DEV_SOCKET", socket)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
async fn wait_restart_barrier(flag: &std::sync::atomic::AtomicBool) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("restart barrier not reached");
}
#[test]
fn exchange_cli_disconnect_restart_preserves_each_admission_boundary() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let mut total=(0usize,0usize);let mut probes=Vec::new();let mut windows=serde_json::Map::new();
            for phase in ["preparation","approval","dispatch","reporting"] {
                let dir=tempfile::tempdir().unwrap();let path=dir.path().join("restart.sqlite");let venue=Arc::new(Venue::default());let approval=Arc::new(RestartApproval::default());
                venue.pause_preparation.store(phase=="preparation",Ordering::SeqCst);
                venue.pause_after_accept.store(phase=="dispatch",Ordering::SeqCst);
                venue.finish_first.store(phase=="reporting",Ordering::SeqCst);
                approval.pause.store(phase=="approval",Ordering::SeqCst);
                let reporting=Arc::new((std::sync::atomic::AtomicBool::new(false),std::sync::atomic::AtomicBool::new(false)));
                let client=Client::with_reporting_barrier(runtime(&path,venue.clone(),approval.clone()),dir.path(),(phase=="reporting").then_some(reporting.clone())).await;
                let input=order_input("restart-original","spot");let argv=args(&["execute",E::KrakenPrivatePostAddOrder.operation(),"--id","restart-original","--input",&input.to_string(),"--json"]);
                let mut child=restart_cli(&client.socket,argv);
                let reached=match phase {"preparation"=>&venue.preparation_entered,"approval"=>&approval.entered,"dispatch"=>&venue.after_accept_entered,_=>&reporting.0};
                wait_restart_barrier(reached).await;child.kill().await.unwrap();let _=child.wait().await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                venue.release_fault.store(true,Ordering::SeqCst);venue.pause_preparation.store(false,Ordering::SeqCst);venue.pause_after_accept.store(false,Ordering::SeqCst);
                approval.release.store(true,Ordering::SeqCst);approval.pause.store(false,Ordering::SeqCst);reporting.1.store(true,Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let saved=Store::open(&path).unwrap().by_request("restart-original").unwrap().unwrap();
                let submission=Store::open(&path).unwrap().submission(&saved.operation_id).unwrap();
                assert_eq!(submission.is_some(),matches!(phase,"dispatch"|"reporting"),"{phase}");
                assert_eq!(venue.writes.load(Ordering::SeqCst),usize::from(matches!(phase,"dispatch"|"reporting")),"{phase}");
                let action_approvals=approval.count.load(Ordering::SeqCst);let action_writes=venue.writes.load(Ordering::SeqCst);
                assert_eq!(action_approvals,usize::from(phase!="preparation"),"{phase}: no approval after a preparation disconnect");
                total.0+=action_approvals;total.1+=action_writes;
                windows.insert(phase.into(),json!({"approval_count":action_approvals,"external_submission_count":action_writes}));
                drop(client);let client=Client::new(runtime(&path,venue.clone(),approval.clone()),dir.path()).await;
                let before_observe=(approval.count.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));let observed=status(&client,&saved.operation_id).await;
                assert_eq!(observed["id"],saved.operation_id);assert_eq!((approval.count.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst)),before_observe);
                if let Some(submission)=submission {assert_eq!(observed["submission"]["reference"],submission.reference);assert_eq!(observed["outcome"]["replay_safe"],false);} else {assert!(observed["submission"].is_null());assert_eq!(observed["outcome"]["dispatch"],"not_dispatched");}
                if phase=="reporting" {assert_eq!(observed["outcome"]["phase"],"done","{observed}");}
                let counters=|| (approval.count.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));
                for (scope,name,account) in [("same_account","restart-same","spot"),("different_account","restart-other","other")] {let before=counters();let probe=execute(&client,name,E::KrakenPrivatePostAddOrder,order_input(name,account)).await;probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));}
            }
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a20.kraken","approval_count":total.0,"external_submission_count":total.1,"intervention_count":0,"remaining_external_uncertainty":true,"restart_barriers":["preparation","approval before admission","accepted submission before response","confirmed before response"],"restart_counts":windows});outcome_evidence::attach_probes(&mut evidence,&probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        });
    }).unwrap().join().unwrap();
}

#[test]
fn missing_exchange_market_metadata_stops_before_approval_and_submission() {
    std::thread::Builder::new().stack_size(24*1024*1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).thread_stack_size(24*1024*1024).enable_all().build().unwrap().block_on(async {
            let dir=tempfile::tempdir().unwrap();let venue=Arc::new(Venue::default());venue.missing_market_metadata.store(true,Ordering::SeqCst);let approval=Arc::new(Approval(AtomicUsize::new(0)));
            let client=Client::new(runtime(&dir.path().join("missing-market.sqlite"),venue.clone(),approval.clone()),dir.path()).await;
            let counters=|| (approval.0.load(Ordering::SeqCst),venue.writes.load(Ordering::SeqCst));let before=counters();
            let failed=execute(&client,"missing-market",E::KrakenPrivatePostAddOrder,order_input("missing-market","spot")).await;let after=counters();
            assert_eq!(failed["error"]["code"],"KRAKEN_MARKET_METADATA_UNAVAILABLE","{failed}");assert!(failed["submission"].is_null());assert_eq!(before,after);
            venue.missing_market_metadata.store(false,Ordering::SeqCst);let mut probes=Vec::new();
            for (scope,name,account) in [("same_account","market-same","spot"),("different_account","market-other","other")] {let before=counters();let probe=execute(&client,name,E::KrakenPrivatePostAddOrder,order_input(name,account)).await;probes.push(outcome_evidence::admission_probe(scope,before,counters(),&probe));}
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {let mut evidence=json!({"scenario":"outcome.a1.kraken","approval_count":after.0-before.0,"external_submission_count":after.1-before.1,"intervention_count":0,"remaining_external_uncertainty":false,"required_metadata_failure":"KRAKEN_MARKET_METADATA_UNAVAILABLE"});outcome_evidence::attach_probes(&mut evidence,&probes);std::fs::write(path,serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();}
        });
    }).unwrap().join().unwrap();
}
