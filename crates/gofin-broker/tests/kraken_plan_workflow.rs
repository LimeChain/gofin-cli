//! Real Kraken adapter / broker native-action rehearsal with independent synthetic venue evidence.
//! GOFIN_KRAKEN_PLAN_CLI=<gofin-dev> additionally runs every public command through the CLI socket.
//! The wallet-funded deposit leg is covered separately by kraken_deposit's composite fixtures.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_domain::{clock::Clock, *};
use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use gofin_kraken::funding::{
    KrakenDestination, KrakenPayoutRequest, KrakenPayoutResult, KrakenRecipientVerifier,
};
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
        assert!(c.binding.to_string().contains("kraken."));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic Kraken native approval".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
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
// Change optional reporting prices after the buy settles. Native amounts and
// fee caps remain unchanged, including when reporting crosses the retired policy.
struct DriftPrices {
    armed: AtomicBool,
    calls: AtomicUsize,
    breach_policy: bool,
}
#[async_trait]
impl gofin_pricing::UsdPriceSource for DriftPrices {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        let mut quote = gofin_pricing::UsdPriceSource::quote(&Prices, asset).await?;
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
                assert!(
                    self.bought.load(Ordering::SeqCst)
                        || self.preparation_funds.load(Ordering::SeqCst)
                );
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
    prices: Arc<dyn gofin_pricing::UsdPriceSource>,
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
    prices: Arc<dyn gofin_pricing::UsdPriceSource>,
    protect_policy: bool,
) -> Arc<gofin_broker::BrokerRuntime> {
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
include!("support/kraken_refund_outcome.rs");

#[test]
fn kraken_limit_buy_funds_base_withdrawal_two_approvals_pending_receipt_and_restart() {
    run_rehearsal(None);
}
#[test]
fn kraken_reporting_price_drift_preserves_native_authority() {
    run_rehearsal(Some(false));
}
#[test]
fn kraken_retired_policy_cannot_block_native_approved_withdrawal() {
    run_rehearsal(Some(true));
}
fn run_rehearsal(drift: Option<bool>) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse(drift));
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
    let prices: Arc<dyn gofin_pricing::UsdPriceSource> = if recovery.is_some() {
        drift.clone()
    } else {
        Arc::new(Prices)
    };
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
    let buy = json!({"requestId":"SYNTHETIC-BUY","account":"synthetic-spot","pair":"ETHUSDC","type":"buy","ordertype":"limit","volume":"0.006","price":"2000","maximumFees":{"XETH":"0.00003","USDC":"0.05"},"minimumReceive":"0.00597","timeinforce":"IOC"});
    let withdraw = json!({"requestId":"SYNTHETIC-WITHDRAW","account":"synthetic-spot","asset":"XETH","key":"synthetic-base","amount":"0.0059","max_fee":"0.0001","destination":DESTINATION});
    // Missing native fee consent is rejected before any financial approval.
    let mut invalid = buy.clone();
    invalid["maximumFees"]["XETH"] = json!("0");
    let failure = client
        .call(
            args(&[
                "preview",
                "kraken.add-order",
                "--input",
                &invalid.to_string(),
                "--json",
            ]),
            BrokerRequest::Preview {
                operation: "kraken.add-order".into(),
                input: invalid,
            },
            false,
        )
        .await;
    assert!(
        failure.to_string().contains("KRAKEN_FEE_BOUND_EXCEEDED"),
        "{failure}"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 0);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
    assert_eq!(venue.eth(), "0");
    let mut results = vec![];
    for (id, operation, input) in [
        ("buy", "kraken.add-order", buy),
        (
            "withdraw",
            E::KrakenPrivatePostWithdraw.operation(),
            withdraw,
        ),
    ] {
        if id == "withdraw" && recovery.is_some() {
            drift.armed.store(true, Ordering::SeqCst);
        }
        let request = OperationRequest {
            request_id: format!("SYNTHETIC-{id}-attempt"),
            operation: operation.into(),
            input: input.clone(),
            run_id: None,
        };
        let argv = args(&[
            "execute",
            operation,
            "--input",
            &input.to_string(),
            "--id",
            &request.request_id,
            "--json",
        ]);
        let call = BrokerRequest::Execute { request };
        let result = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(
            result["status"],
            if id == "buy" { "completed" } else { "pending" },
            "{result}"
        );
        assert!(result["submission"].is_object());
        if id == "withdraw" {
            assert!(
                result["effects"].is_null(),
                "Source acceptance cannot establish destination receipt"
            );
            assert_eq!(result["outcome"]["phase"], "in_progress");
        } else {
            assert_eq!(result["outcome"]["result"], "executed");
            let proof: VerifiedEffects = serde_json::from_value(result["effects"].clone()).unwrap();
            assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Conversion {spent,received,..} if spent.atoms.to_decimal(36)=="12" && received.atoms.to_decimal(36)=="0.006")));
            assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Cost {amount,treatment:CostTreatment::AdditionalDebit,..} if amount.asset.reference=="USDC" && amount.atoms.to_decimal(36)=="0.048")));
        }
        let duplicate = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(duplicate["id"], result["id"]);
        results.push((argv, call, result));
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
    drop(client);
    let client = Client::new(
        runtime(
            &path,
            venue.clone(),
            approval.clone(),
            prices,
            recovery == Some(true),
        ),
        temp.path(),
    )
    .await;
    for (argv, call, original) in &results {
        let duplicate = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(duplicate["id"], original["id"]);
        assert_eq!(duplicate["submission"], original["submission"]);
    }
    venue.receipt.store(true, Ordering::SeqCst);
    let id = results[1].2["id"].as_str().unwrap();
    let settled = client
        .call(
            args(&["status", id, "--json"]),
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await;
    assert_eq!(settled["outcome"]["phase"], "done", "{settled}");
    assert_eq!(settled["outcome"]["replay_safe"], false);
    let proof: VerifiedEffects = serde_json::from_value(settled["effects"].clone()).unwrap();
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::CashDebit {resource,amount} if resource.0==ACCOUNT && amount.atoms.to_decimal(36)=="0.0059")));
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::UsableCredit {to,amount} if to.0==DESTINATION && amount.atoms.to_decimal(18)=="0.0058")));
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Cost {amount,treatment:CostTreatment::IncludedInDebit,..} if amount.atoms.to_decimal(36)=="0.0001")));
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
}
