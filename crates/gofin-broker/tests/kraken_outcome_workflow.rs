//! Real CLI/broker Kraken cancellation with correlated fills and unrelated activity.
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
                let row = json!({"cl_ord_id":p["cl_ord_id"],"opentm":format!("{}.{:09}",chrono::Utc::now().timestamp(),chrono::Utc::now().timestamp_subsec_nanos()),
                    "descr":{"pair":"XBTUSD","type":"buy","price":"20000"},"status":"open","vol":"0.01","vol_exec":"0.005","cost":"100","fee":"0.2","trades":[format!("TRADE-{id}")]});
                self.orders
                    .lock()
                    .unwrap()
                    .insert(id.clone(), (account.into(), row));
                json!({"txid":[id]})
            }
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
            let original = execute(&client, "missing-order", E::KrakenPrivatePostAddOrder, input.clone()).await;
            let id = original["id"].as_str().unwrap().to_owned();
            assert!(original["submission"].is_object(), "{original}");
            let observed = status(&client, &id).await;
            assert_eq!(observed["outcome"]["dispatch"], "uncertain", "{observed}");
            assert_eq!(observed["outcome"]["replay_safe"], false);
            for (request, account) in [("missing-same", "spot"), ("missing-other", "other")] {
                let probe = execute(&client, request, E::KrakenPrivatePostAddOrder, order_input(request, account)).await;
                assert!(probe["submission"].is_object(), "{probe}");
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
                std::fs::write(path, serde_json::to_vec_pretty(&json!({"scenario":"outcome.a13.kraken", "approval_count":1,"external_submission_count":1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false})).unwrap()).unwrap();
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
                    let mut ids = std::collections::BTreeSet::new();
                    for (id, account) in [
                        ("same-account-stall", "spot"),
                        ("other-account-stall", "other"),
                    ] {
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
                        std::fs::write(path, serde_json::to_vec_pretty(&json!({"scenario":"outcome.a6.kraken", "approval_count":3,"external_submission_count":3,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false})).unwrap()).unwrap();
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
    approval: Arc<Approval>,
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
        Arc::new(TestClock),
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
            let original=execute(&client,"quote-expiry",E::KrakenPrivatePostAddOrder,input.clone()).await;
            assert!(original["submission"].is_object(),"{original}");
            assert!(wrapper.preparations.load(Ordering::SeqCst)>=2);
            assert_eq!(approval.0.load(Ordering::SeqCst),1);
            assert_eq!(venue.writes.load(Ordering::SeqCst),1);
            assert_eq!(execute(&client,"quote-expiry",E::KrakenPrivatePostAddOrder,input).await["id"],original["id"]);
            for (id,account) in [("expiry-same","spot"),("expiry-other","other")] {
                assert!(execute(&client,id,E::KrakenPrivatePostAddOrder,order_input(id,account)).await["submission"].is_object());
            }
            assert_eq!(approval.0.load(Ordering::SeqCst),3); assert_eq!(venue.writes.load(Ordering::SeqCst),3);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") { assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some()); std::fs::write(path,serde_json::to_vec_pretty(&json!({"scenario":"outcome.a9.kraken","approval_count":1,"external_submission_count":1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false})).unwrap()).unwrap(); }
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
            let original=execute(&client,"partial",E::KrakenPrivatePostAddOrder,input.clone()).await;
            let id=original["id"].as_str().unwrap();
            let partial=status(&client,id).await;
            assert_eq!(partial["outcome"]["result"],"partial","{partial}");
            venue.unrelated_balance_changes.fetch_add(1,Ordering::SeqCst);
            // Probe while the original remainder is live, not only after cancellation.
            for (request,account) in [("same-account","spot"),("different-account","other")] {
                let probe=execute(&client,request,E::KrakenPrivatePostAddOrder,order_input(request,account)).await;
                assert!(probe["submission"].is_object(),"{probe}");
            }
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
            assert_eq!(approval.0.load(Ordering::SeqCst),4);
            assert_eq!(venue.writes.load(Ordering::SeqCst),4);
            if let Ok(path)=std::env::var("GOFIN_OUTCOME_EVIDENCE") {
                assert!(std::env::var_os("GOFIN_KRAKEN_PLAN_CLI").is_some());
                std::fs::write(path,serde_json::to_vec_pretty(&json!({"scenario":"outcome.a19.kraken","approval_count":2,"external_submission_count":2,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false})).unwrap()).unwrap();
            }
        });
    }).unwrap().join().unwrap();
}
