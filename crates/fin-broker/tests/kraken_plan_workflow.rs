//! Real Kraken adapter / broker named-plan rehearsal with independent synthetic venue evidence.
//! FIN_KRAKEN_PLAN_CLI=<fin-dev> additionally runs every public command through the CLI socket.
//! The wallet-funded deposit leg is covered separately by kraken_deposit's composite fixtures.
use async_trait::async_trait;
use fin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use fin_domain::{clock::Clock, financial_plans::*, *};
use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use fin_kraken::funding::{
    KrakenDestination, KrakenPayoutRequest, KrakenPayoutResult, KrakenRecipientVerifier,
};
use fin_kraken::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use fin_protocol::BrokerRequest;
use fin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

const WALLET: &str = "0x1111111111111111111111111111111111111111";
const DESTINATION: &str = "ethereum:8453:0x1111111111111111111111111111111111111111";
const ACCOUNT: &str = "kraken:synthetic-plan";
const TX: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";
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
        assert!(c.binding["plan"]["steps"].is_array());
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic Kraken plan only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl fin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<fin_pricing::UsdQuote> {
        Ok(fin_pricing::UsdQuote {
            usd_per_unit: match a.reference.as_str() {
                "XETH" | "native" => "2000",
                "USDC" => "1",
                _ => panic!("unexpected priced asset {a:?}"),
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "independent synthetic USD".into(),
        })
    }
}
// Change real protected USD quotes only after the buy has settled. The first
// withdrawal preparation sees $2000/ETH; its validation sees exactly one cent
// more movement. An optional next quote exceeds the protected $25 policy.
struct DriftPrices {
    armed: AtomicBool,
    calls: AtomicUsize,
    breach_policy: bool,
}
#[async_trait]
impl fin_pricing::UsdPriceSource for DriftPrices {
    async fn quote(&self, asset: &AssetId) -> Result<fin_pricing::UsdQuote> {
        let mut quote = fin_pricing::UsdPriceSource::quote(&Prices, asset).await?;
        if asset.reference == "XETH" && self.armed.load(Ordering::SeqCst) {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            quote.usd_per_unit = match call {
                0 => "2000",
                1 => "2001.694915254237288135593220338983",
                _ if self.breach_policy => "5000",
                _ => "2001.694915254237288135593220338983",
            }
            .into();
        }
        Ok(quote)
    }
}
#[derive(Default)]
struct Venue {
    calls: Mutex<Vec<(E, Value)>>,
    bought: AtomicBool,
    withdrawn: AtomicBool,
    receipt: AtomicBool,
    preparation_funds: AtomicBool,
    writes: AtomicUsize,
}
impl Venue {
    fn eth(&self) -> &'static str {
        if self.withdrawn.load(Ordering::SeqCst) {
            "0.0001"
        } else if self.bought.load(Ordering::SeqCst)
            || self.preparation_funds.load(Ordering::SeqCst)
        {
            "0.006"
        } else {
            "0"
        }
    }
    fn usdc(&self) -> &'static str {
        if self.bought.load(Ordering::SeqCst) {
            "7.952"
        } else {
            "20"
        }
    }
    fn order(&self) -> Value {
        let calls = self.calls.lock().unwrap();
        let input = &calls
            .iter()
            .find(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .unwrap()
            .1;
        json!({"cl_ord_id":input["cl_ord_id"],"opentm":format!("{}.{:09}", chrono::Utc::now().timestamp(), chrono::Utc::now().timestamp_subsec_nanos()),"descr":{"pair":"ETHUSDC","type":"buy"},"status":"closed","vol":"0.006","vol_exec":"0.006","cost":"12","fee":"0.048","trades":["SYNTHETIC-TRADE"]})
    }
}
#[async_trait]
impl KrakenTransport for Venue {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.calls.lock().unwrap().push((e, p.clone()));
        if e.mutating() {
            assert!(guard.expect("write guard")());
            self.writes.fetch_add(1, Ordering::SeqCst);
        } else {
            assert!(guard.is_none());
        }
        let result = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades","withdraw-funds"],"valid_until":"0","iban":"SYNTHETIC"})
            }
            E::KrakenPublicGetAssets => json!({"XETH":{"decimals":18},"USDC":{"decimals":6}}),
            E::KrakenPublicGetAssetPairs => {
                json!({"ETHUSDC":{"altname":"ETHUSDC","base":"XETH","quote":"USDC","status":"online","lot_decimals":8,"pair_decimals":2,"cost_decimals":6,"ordermin":"0.0001"}})
            }
            E::KrakenPublicGetTicker => {
                json!({"ETHUSDC":{"b":["1999","1","1"],"a":["2000","1","1"]}})
            }
            E::KrakenPrivatePostTradeVolume => {
                json!({"fees":{"ETHUSDC":{"fee":"0.4"}},"fees_maker":{"ETHUSDC":{"fee":"0.25"}}})
            }
            E::KrakenPrivatePostBalanceEx => {
                json!({"XETH":{"balance":self.eth(),"hold_trade":"0"},"USDC":{"balance":self.usdc(),"hold_trade":"0"}})
            }
            E::KrakenPrivatePostBalance => json!({"XETH":self.eth(),"USDC":self.usdc()}),
            E::KrakenPrivatePostOpenPositions | E::KrakenPrivatePostCreditLines => json!({}),
            E::KrakenPrivatePostEarnAllocations => json!({"items":[]}),
            E::KrakenPrivatePostAddOrder => {
                assert_eq!(p["volume"], "0.006");
                assert_eq!(p["price"], "2000");
                assert!(!self.bought.swap(true, Ordering::SeqCst));
                json!({"txid":["SYNTHETIC-ORDER"]})
            }
            E::KrakenPrivatePostQueryOrders => json!({"SYNTHETIC-ORDER":self.order()}),
            E::KrakenPrivatePostOpenOrders => json!({"open":{}}),
            E::KrakenPrivatePostClosedOrders => json!({"closed":{"SYNTHETIC-ORDER":self.order()}}),
            E::KrakenPrivatePostQueryTrades => {
                json!({"SYNTHETIC-TRADE":{"ordertxid":"SYNTHETIC-ORDER","pair":"ETHUSDC","type":"buy","vol":"0.006","cost":"12","fee":"0.048","margin":"0","leverage":"0","ledgers":["BUY-BASE","BUY-QUOTE"]}})
            }
            E::KrakenPrivatePostQueryLedgers => {
                json!({"BUY-BASE":{"refid":"SYNTHETIC-TRADE","type":"trade","asset":"XETH","amount":"0.006","fee":"0"},"BUY-QUOTE":{"refid":"SYNTHETIC-TRADE","type":"trade","asset":"USDC","amount":"-12","fee":"0.048"}})
            }
            E::KrakenPrivatePostWithdrawInfo => {
                json!({"method":"Ether (Base)","amount":"0.0058","fee":"0.0001","limit":self.eth()})
            }
            E::KrakenPrivatePostWithdrawAddresses => {
                json!([{"key":"synthetic-base","method":"Ether (Base)","verified":true,"address":WALLET}])
            }
            E::KrakenPrivatePostWithdrawMethods => {
                json!([{"method":"Ether (Base)","network":"Base","minimum":"0.0001"}])
            }
            E::KrakenPrivatePostWithdraw => {
                assert!(self.bought.load(Ordering::SeqCst));
                assert_eq!(p["amount"], "0.0059");
                assert!(!self.withdrawn.swap(true, Ordering::SeqCst));
                json!({"refid":"SYNTHETIC-WITHDRAWAL"})
            }
            E::KrakenPrivatePostWithdrawStatus => {
                json!([{"refid":"SYNTHETIC-WITHDRAWAL","status":"Success","asset":"XETH","method":"Ether (Base)","key":"synthetic-base","info":WALLET,"amount":"0.0058","fee":"0.0001","txid":TX}])
            }
            E::KrakenPrivatePostLedgers => {
                json!({"ledger":{"WITHDRAW-LEDGER":{"refid":"SYNTHETIC-WITHDRAWAL","type":"withdrawal","asset":"XETH","amount":"-0.0058","fee":"0.0001","balance":"0.0001"}}})
            }
            _ => panic!("Unexpected synthetic endpoint {e:?}: {p}"),
        };
        Ok(KrakenResponse::Json(json!({"error":[],"result":result})))
    }
}
#[async_trait]
impl KrakenRecipientVerifier for Venue {
    async fn resolve(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
        address: &str,
        selector: Option<&str>,
    ) -> Result<Option<KrakenDestination>> {
        assert_eq!((network, coin, address), ("Base", "XETH", WALLET));
        assert!(contract.is_none());
        assert_eq!(selector, Some(DESTINATION));
        Ok(Some(KrakenDestination {
            enrolled: true,
            resource: ResourceId(DESTINATION.into()),
            address: WALLET.into(),
            chain_id: 8453,
            network: None,
            cluster: None,
            mint: None,
            asset: AssetId {
                network: "ethereum:8453".into(),
                reference: "native".into(),
                decimals: 18,
            },
            confirmations: 2,
        }))
    }
    async fn verify(&self, r: KrakenPayoutRequest) -> Result<KrakenPayoutResult> {
        assert_eq!(r.tx_hash, TX);
        assert_eq!(
            (r.minimum.as_str(), r.maximum.as_str()),
            ("0.0058", "0.0058")
        );
        assert_eq!(r.destination.resource.0, DESTINATION);
        let ready = self.receipt.load(Ordering::SeqCst);
        Ok(KrakenPayoutResult {
            status: if ready {
                Status::Completed
            } else {
                Status::Pending
            },
            amount: if ready {
                Some(Quantity::from_decimal("0.0058", 18)?)
            } else {
                None
            },
            evidence: json!({"synthetic_independent_receipt":ready,"tx_hash":TX,"recipient":WALLET,"chain_id":8453}),
        })
    }
}
fn adapter(
    venue: Arc<Venue>,
    prices: Arc<dyn fin_pricing::UsdPriceSource>,
) -> Arc<ExchangeAdapter<KrakenProfile>> {
    Arc::new(ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![KrakenAccount {
                name: "synthetic-spot".into(),
                resource: ResourceId(ACCOUNT.into()),
                credential_ref: "synthetic-never-a-real-key".into(),
                physical_identity: "synthetic-identity".into(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: true,
                product: KrakenProduct::Spot,
            }],
            venue.clone(),
        )
        .unwrap()
        .with_prices(prices)
        .with_recipient_verifier(venue),
    ))
}
fn runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    approval: Arc<Approval>,
    prices: Arc<dyn fin_pricing::UsdPriceSource>,
    protect_policy: bool,
) -> Arc<fin_broker::BrokerRuntime> {
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy: BrokerPolicy {
                max_movement_usd_micros: if protect_policy {
                    "25000000".into()
                } else {
                    BrokerPolicy::beta().max_movement_usd_micros
                },
                ..BrokerPolicy::beta()
            },
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![adapter(venue, prices)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap()
}
include!("support/kraken_plan_client.rs");
fn exact_step(
    id: &str,
    p: &PreparedOperation,
    input: Value,
    credits: Value,
    deps: Vec<FinancialPlanFundingDependency>,
) -> FinancialPlanStepSpec {
    FinancialPlanStepSpec {
        step_id: id.into(),
        operation: p.operation.clone(),
        input,
        permitted_variations: json!({"mode":"exact","maximum_debits":p.intent.debits,"maximum_costs":p.intent.maximum_costs,"possible_credits":credits}),
        funding_dependencies: deps,
        action_deadline_seconds: None,
        completion: FinancialStepCompletion {
            operation_succeeded: true,
            principal_spend: None,
            minimum_net_receipt: None,
        },
    }
}
#[test]
fn kraken_limit_buy_funds_base_withdrawal_one_approval_pending_receipt_and_restart_replay() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse(None))
        })
        .unwrap()
        .join()
        .unwrap();
}
#[test]
fn kraken_one_cent_usd_drift_refreshes_unsubmitted_candidate_without_reapproval() {
    run_recovery_rehearsal(false);
}
#[test]
fn kraken_refreshed_candidate_cannot_cross_protected_policy_or_erase_consumption() {
    run_recovery_rehearsal(true);
}
fn run_recovery_rehearsal(breach_policy: bool) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse(Some(breach_policy)));
        })
        .unwrap()
        .join()
        .unwrap();
}
async fn rehearse(recovery: Option<bool>) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.sqlite");
    let venue = Arc::new(Venue::default());
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let drift = Arc::new(DriftPrices {
        armed: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        breach_policy: recovery == Some(true),
    });
    let prices: Arc<dyn fin_pricing::UsdPriceSource> = if recovery.is_some() {
        drift.clone()
    } else {
        Arc::new(Prices)
    };
    let a = adapter(venue.clone(), prices.clone());
    let buy = json!({"requestId":"SYNTHETIC-BUY","account":"synthetic-spot","pair":"ETHUSDC","type":"buy","ordertype":"limit","volume":"0.006","price":"2000","maximumFees":{"XETH":"0.00003","USDC":"0.05"},"minimumReceive":"0.00597","timeinforce":"IOC"});
    let withdraw = json!({"requestId":"SYNTHETIC-WITHDRAW","account":"synthetic-spot","asset":"XETH","key":"synthetic-base","amount":"0.0059","max_fee":"0.0001","destination":DESTINATION});
    let b = a.prepare("kraken.add-order", &buy).await.unwrap();
    venue.preparation_funds.store(true, Ordering::SeqCst);
    let w = a
        .prepare(E::KrakenPrivatePostWithdraw.operation(), &withdraw)
        .await
        .unwrap();
    venue.preparation_funds.store(false, Ordering::SeqCst);
    let eth = AssetId {
        network: "kraken".into(),
        reference: "XETH".into(),
        decimals: 36,
    };
    let native = AssetId {
        network: "ethereum:8453".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Synthetic Kraken buy then Base withdrawal".into(),
        summary: Some(
            "Buy 0.006 ETH with USDC and withdraw 0.0059 ETH including 0.0001 fee to Base.".into(),
        ),
        steps: vec![
            exact_step(
                "buy",
                &b,
                buy.clone(),
                json!([{"resource":ACCOUNT,"asset":eth}]),
                vec![],
            ),
            exact_step(
                "withdraw",
                &w,
                withdraw.clone(),
                json!([{"resource":DESTINATION,"asset":native}]),
                vec![FinancialPlanFundingDependency {
                    producing_step_id: "buy".into(),
                    resource: ResourceId(ACCOUNT.into()),
                    asset: eth,
                    funding_scope: None,
                    maximum_fraction_bps: None,
                }],
            ),
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: b
                .intent
                .debits
                .iter()
                .chain(&w.intent.debits)
                .cloned()
                .collect(),
            maximum_costs: b
                .intent
                .maximum_costs
                .iter()
                .chain(&w.intent.maximum_costs)
                .cloned()
                .collect(),
        },
    };
    venue.calls.lock().unwrap().clear();
    let client = Client::new(
        runtime(
            &path,
            venue.clone(),
            approval.clone(),
            prices.clone(),
            recovery == Some(true),
        ),
        temp.path(),
    )
    .await;
    let request = FinancialPlanAuthorizationRequest {
        request_id: "SYNTHETIC-PLAN".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    // The shared whole-plan preflight catches the actual zero-base-fee approval defect.
    let mut bad = request.clone();
    bad.plan.steps[0].input["maximumFees"]["XETH"] = json!("0");
    let failure = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &serde_json::to_string(&bad.plan).unwrap(),
                "--id",
                "SYNTHETIC-BAD",
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request: bad },
            false,
        )
        .await;
    assert!(
        failure.to_string().contains("KRAKEN_FEE_BOUND_EXCEEDED"),
        "{failure}"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
    let preview = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: request.clone(),
            },
            true,
        )
        .await;
    let mut request = request;
    request.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorized = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--id",
                "SYNTHETIC-PLAN",
                "--review-hash",
                request.expected_review_hash.as_ref().unwrap(),
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request },
            true,
        )
        .await;
    assert_eq!(
        venue.eth(),
        "0",
        "future-funded withdrawal cannot require ETH before buy"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert!(
        venue
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(e, _)| *e != E::KrakenPrivatePostBalanceEx),
        "preapproval must not admit execution against future balances"
    );
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    let early = OperationRequest {
        request_id: "SYNTHETIC-too-early".into(),
        operation: E::KrakenPrivatePostWithdraw.operation().into(),
        input: withdraw.clone(),
        run_id: None,
    };
    let rejected = client
        .call(
            args(&[
                "execute",
                &early.operation,
                "--input",
                &withdraw.to_string(),
                "--id",
                &early.request_id,
                "--plan",
                &plan_id,
                "--step",
                "withdraw",
                "--json",
            ]),
            BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "withdraw".into(),
                request: early,
            },
            false,
        )
        .await;
    assert!(
        rejected.to_string().contains("PLAN_STEP_NOT_ELIGIBLE"),
        "{rejected}"
    );
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
    let mut results = vec![];
    let mut consumed_before_withdrawal = None;
    for (step_id, op, input) in [
        ("buy", "kraken.add-order", buy),
        (
            "withdraw",
            E::KrakenPrivatePostWithdraw.operation(),
            withdraw,
        ),
    ] {
        let request = OperationRequest {
            request_id: format!("SYNTHETIC-{step_id}-attempt"),
            operation: op.into(),
            input: input.clone(),
            run_id: None,
        };
        let argv = args(&[
            "execute",
            op,
            "--input",
            &input.to_string(),
            "--id",
            &request.request_id,
            "--plan",
            &plan_id,
            "--step",
            step_id,
            "--json",
        ]);
        let call = BrokerRequest::ExecutePlanStep {
            plan_id: plan_id.clone(),
            step_id: step_id.into(),
            request,
        };
        if step_id == "withdraw" && recovery.is_some() {
            let plan: FinancialPlanAuthorizationRecord = serde_json::from_value(
                Store::open(&path)
                    .unwrap()
                    .financial_plan(&plan_id)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            consumed_before_withdrawal = Some(plan.usage.clone());
            drift.armed.store(true, Ordering::SeqCst);
        }
        let result = client.call(argv.clone(), call.clone(), true).await;
        if step_id == "withdraw" && recovery.is_some() {
            let store = Store::open(&path).unwrap();
            let plan: FinancialPlanAuthorizationRecord =
                serde_json::from_value(store.financial_plan(&plan_id).unwrap().unwrap()).unwrap();
            let attempts: Vec<_> = plan
                .attempts
                .iter()
                .filter(|attempt| attempt.step_id == "withdraw")
                .collect();
            assert_eq!(attempts.len(), 2, "{plan:?}");
            assert!(
                attempts
                    .iter()
                    .all(|attempt| attempt.logical_request_id.as_deref()
                        == Some("SYNTHETIC-withdraw-attempt"))
            );
            let failed: OperationRecord =
                serde_json::from_value(store.get_record(&attempts[0].attempt_id).unwrap().unwrap())
                    .unwrap();
            assert_eq!(failed.status, Status::Failed);
            assert!(
                failed.submission.is_none()
                    && failed.dispatch_evidence.is_none()
                    && failed.effects.is_none()
            );
            let failure = failed.error.as_ref().unwrap();
            assert_eq!(failure.code, "VALUE_BOUND_EXCEEDED");
            let details = failure.details.as_ref().unwrap();
            assert_eq!(details["reason"], "usd_ceiling_exceeded");
            assert_eq!(details["field"], "movement_usd_micros");
            assert_eq!(details["maximum_usd_micros"], "11800000");
            assert_eq!(details["observed_usd_micros"], "11810000");
            assert_ne!(failed.id, result["id"]);
            assert_eq!(store.reservation_count(&failed.id).unwrap(), 0);
            if recovery == Some(true) {
                assert_eq!(result["status"], "failed", "{result}");
                assert_eq!(result["error"]["code"], "MOVEMENT_LIMIT", "{result}");
                assert!(
                    result["submission"].is_null()
                        && result["dispatch_evidence"].is_null()
                        && result["effects"].is_null()
                );
                assert_eq!(plan.usage, *consumed_before_withdrawal.as_ref().unwrap());
                assert!(plan.usage.reservations.is_empty());
                assert_eq!(
                    venue.writes.load(Ordering::SeqCst),
                    1,
                    "only the earlier buy dispatched"
                );
                assert!(!venue.withdrawn.load(Ordering::SeqCst));
                assert_eq!(approval.0.load(Ordering::SeqCst), 1);
                drop(client);
                let client = Client::new(
                    runtime(&path, venue.clone(), approval.clone(), prices.clone(), true),
                    temp.path(),
                )
                .await;
                let replay = client.call(argv, call, true).await;
                assert_eq!(replay["id"], result["id"]);
                assert_eq!(replay["error"], result["error"]);
                assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
                assert_eq!(approval.0.load(Ordering::SeqCst), 1);
                return;
            }
            assert!(
                drift.calls.load(Ordering::SeqCst) >= 4,
                "fresh preparation and validation must both occur"
            );
        }
        assert_eq!(
            result["status"],
            if step_id == "buy" {
                "completed"
            } else {
                "pending"
            },
            "{result}"
        );
        assert!(!result["submission"].is_null());
        if step_id == "withdraw" {
            assert!(
                result["effects"].is_null(),
                "source acknowledgement must not claim an unobserved receipt"
            );
            assert!(result["error"].is_null());
        } else {
            let settled: OperationRecord = serde_json::from_value(result.clone()).unwrap();
            let effects = settled.effects.unwrap().effects;
            assert!(effects.iter().any(|e| matches!(e, FinancialEffect::Conversion {spent, received, ..} if spent.atoms.to_decimal(36) == "12" && received.atoms.to_decimal(36) == "0.006")));
            assert!(effects.iter().any(|e| matches!(e, FinancialEffect::Cost {amount, treatment:CostTreatment::AdditionalDebit, ..} if amount.asset.reference == "USDC" && amount.atoms.to_decimal(36) == "0.048")));
        }
        let duplicate = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(duplicate["id"], result["id"]);
        results.push((argv, call, result));
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    drop(client);
    let client = Client::new(
        runtime(
            &path,
            venue.clone(),
            approval.clone(),
            prices.clone(),
            recovery == Some(true),
        ),
        temp.path(),
    )
    .await;
    for (argv, call, result) in &results {
        let duplicate = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(duplicate["id"], result["id"]);
        assert_eq!(duplicate["submission"], result["submission"]);
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    venue.receipt.store(true, Ordering::SeqCst);
    let status = client
        .call(
            args(&["plans", "status", &plan_id, "--json"]),
            BrokerRequest::PlanStatus {
                plan_id: plan_id.clone(),
            },
            true,
        )
        .await;
    assert!(
        status["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["state"] == "completed"),
        "{status}"
    );
    assert!(
        status["usage"]["reservations"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{status}"
    );
    let id = results[1].2["id"].as_str().unwrap();
    let settled = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    let record: OperationRecord = serde_json::from_value(settled).unwrap();
    assert_eq!(record.status, Status::Completed);
    let effects = record.effects.unwrap().effects;
    assert!(effects.iter().any(|e| matches!(e, FinancialEffect::CashDebit {resource, amount} if resource.0 == ACCOUNT && amount.atoms.to_decimal(36) == "0.0059")));
    assert!(effects.iter().any(|e| matches!(e, FinancialEffect::UsableCredit {to, amount} if to.0 == DESTINATION && amount.atoms.to_decimal(18) == "0.0058")));
    assert!(effects.iter().any(|e| matches!(e, FinancialEffect::Cost {amount, treatment: CostTreatment::IncludedInDebit, ..} if amount.atoms.to_decimal(36) == "0.0001")));
    client
        .call(
            args(&["plans", "cancel", &plan_id, "--json"]),
            BrokerRequest::RevokePlan { plan_id },
            true,
        )
        .await;
    for (argv, call, result) in &results {
        let duplicate = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(duplicate["id"], result["id"]);
        assert_eq!(duplicate["submission"], result["submission"]);
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}
