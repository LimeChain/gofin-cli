//! Offline synthetic USD-M settings. No secrets, account access or broadcasts.
use async_trait::async_trait;
use gofin_binance::*;
use gofin_domain::*;
use gofin_engine::{Engine, EngineOptions};
use gofin_store::Store;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Venue {
    leverage: AtomicUsize,
    margin: Mutex<String>,
    posts: AtomicUsize,
    hedge: AtomicBool,
    multi: AtomicBool,
    orders: AtomicBool,
    algo: AtomicBool,
    permission: AtomicBool,
    position: Mutex<String>,
    empty_positions: AtomicBool,
    lost: AtomicBool,
    no_apply: AtomicBool,
    config_unavailable: AtomicBool,
}
impl Venue {
    fn new() -> Self {
        Self {
            leverage: AtomicUsize::new(20),
            margin: Mutex::new("CROSSED".into()),
            posts: AtomicUsize::new(0),
            hedge: AtomicBool::new(false),
            multi: AtomicBool::new(false),
            orders: AtomicBool::new(false),
            algo: AtomicBool::new(false),
            permission: AtomicBool::new(true),
            position: Mutex::new("0".into()),
            empty_positions: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            no_apply: AtomicBool::new(false),
            config_unavailable: AtomicBool::new(false),
        }
    }
}
#[async_trait]
impl BinanceTransport for Venue {
    async fn request(
        &self,
        _: &CredentialHandle,
        r: BinanceRequest,
    ) -> std::result::Result<Value, TransportError> {
        r.validate().unwrap();
        use BinanceEndpoint as E;
        Ok(match r.endpoint {
            E::Account => json!({"uid":1,"accountType":"SPOT","balances":[]}),
            E::ApiRestrictions => json!({"enableFutures":self.permission.load(Ordering::SeqCst)}),
            E::FuturesAccountConfig => {
                json!({"canTrade":true,"dualSidePosition":self.hedge.load(Ordering::SeqCst),"multiAssetsMargin":self.multi.load(Ordering::SeqCst)})
            }
            E::FuturesExchangeInfo => {
                json!({"symbols":[{"symbol":"ETHUSDT","contractType":"PERPETUAL","status":"TRADING","baseAsset":"ETH","quoteAsset":"USDT","marginAsset":"USDT"},{"symbol":"BTCUSDC","contractType":"PERPETUAL","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDC","marginAsset":"USDC"}]})
            }
            E::FuturesSymbolConfig => {
                if self.config_unavailable.load(Ordering::SeqCst) {
                    return Err(TransportError::Timeout);
                }
                json!([{"symbol":"ETHUSDT","leverage":self.leverage.load(Ordering::SeqCst),"marginType":self.margin.lock().unwrap().clone(),"isAutoAddMargin":false,"maxNotionalValue":"1000000"}])
            }
            E::FuturesPositions => {
                if self.empty_positions.load(Ordering::SeqCst) {
                    return Ok(json!([]));
                }
                json!([{"symbol":"ETHUSDT","positionSide":"BOTH","positionAmt":self.position.lock().unwrap().clone(),"entryPrice":"2000","breakEvenPrice":"2001","markPrice":"1990","unRealizedProfit":"-0.01","liquidationPrice":"1000","notional":"-1.99","isolatedWallet":"1","initialMargin":"0.1","maintMargin":"0.01"}])
            }
            E::FuturesOpenOrders => {
                if self.orders.load(Ordering::SeqCst) {
                    json!([{"symbol":"ETHUSDT"}])
                } else {
                    json!([])
                }
            }
            E::FuturesOpenAlgoOrders => {
                if self.algo.load(Ordering::SeqCst) {
                    json!([{"symbol":"ETHUSDT"}])
                } else {
                    json!([])
                }
            }
            E::FuturesLeverageBrackets => {
                json!({"symbol":"ETHUSDT","brackets":[{"bracket":1,"initialLeverage":50}]})
            }
            E::FuturesAccount => {
                json!({"assets":[{"asset":"USDT","walletBalance":"10.000000000000000001","marginBalance":"9.900000000000000001","availableBalance":"5","unrealizedProfit":"-0.1","initialMargin":"4.9","maintMargin":"0.2"}]})
            }
            E::FuturesLeverage | E::FuturesMarginType => {
                self.posts.fetch_add(1, Ordering::SeqCst);
                if !self.no_apply.load(Ordering::SeqCst) {
                    if r.endpoint == E::FuturesLeverage {
                        self.leverage.store(
                            r.get("leverage").unwrap().parse().unwrap(),
                            Ordering::SeqCst,
                        );
                    } else {
                        *self.margin.lock().unwrap() = r.get("marginType").unwrap().into();
                    }
                }
                if self.lost.swap(false, Ordering::SeqCst) {
                    return Err(TransportError::Timeout);
                }
                if r.endpoint == E::FuturesLeverage {
                    json!({"symbol":"ETHUSDT","leverage":self.leverage.load(Ordering::SeqCst),"maxNotionalValue":"1000000"})
                } else {
                    json!({"code":200,"msg":"success"})
                }
            }
            _ => panic!("Unexpected synthetic endpoint {:?}", r.endpoint),
        })
    }
}
struct Prices;
#[async_trait]
impl BinancePriceProvider for Prices {
    async fn quote_usd(&self, coin: &str) -> Result<PriceQuote> {
        assert_eq!(coin, "USDT");
        Ok(PriceQuote {
            usd_per_unit: "0.9999".into(),
            observed_at: now(),
            source: "synthetic-price".into(),
        })
    }
}
struct Approval {
    count: AtomicUsize,
    deny: AtomicBool,
}
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.deny.load(Ordering::SeqCst) {
            return Err(GoFinError::new(
                "APPROVAL_DENIED",
                "approval",
                "Synthetic denial",
                "Retry explicitly.",
            ));
        }
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
fn adapter(v: Arc<Venue>) -> Arc<BinanceAdapter> {
    Arc::new(
        BinanceAdapter::new(
            BinanceConfig {
                accounts: vec![BinanceAccount {
                    name: "test".into(),
                    resource: ResourceId("binance:uid:1:SPOT".into()),
                    credential: CredentialHandle::new("synthetic-only").unwrap(),
                    role: AccountRole::Main,
                    parent: None,
                    email: None,
                    wallet_type: "SPOT".into(),
                }],
                destinations: vec![],
            },
            v,
            Some(Arc::new(Prices)),
            None,
        )
        .unwrap(),
    )
}
fn request(id: &str, margin: bool) -> OperationRequest {
    OperationRequest {
        request_id: id.into(),
        operation: if margin {
            "binance.futures.margin-type-set"
        } else {
            "binance.futures.leverage-set"
        }
        .into(),
        run_id: None,
        input: if margin {
            json!({"request_id":id,"account":"test","symbol":"ETHUSDT","margin_type":"ISOLATED"})
        } else {
            json!({"request_id":id,"account":"test","symbol":"ETHUSDT","leverage":3})
        },
    }
}
fn engine(a: Arc<BinanceAdapter>, auth: Arc<Approval>, store: Store) -> Engine {
    Engine::new(
        store,
        vec![a],
        auth,
        EngineOptions {
            max_movement_and_cost_usd_micros: "100000000".into(),
            max_run_allocation_usd_micros: "100000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
}
fn approval() -> Arc<Approval> {
    Arc::new(Approval {
        count: AtomicUsize::new(0),
        deny: AtomicBool::new(false),
    })
}

#[tokio::test]
async fn preview_is_read_only_and_settings_have_separate_approvals() {
    let v = Arc::new(Venue::new());
    let a = adapter(v.clone());
    let auth = approval();
    let r = request("leverage", false);
    let p = a.prepare(&r.operation, &r.input).await.unwrap();
    assert_eq!(
        p.intent.locks,
        vec![
            ResourceId("binance:uid:1:SPOT".into()),
            ResourceId("binance:uid:1:SPOT:USD_M".into())
        ]
    );
    assert_eq!(
        p.valuation.as_ref().unwrap().movement_usd_micros.as_str(),
        "0"
    );
    assert_eq!(v.posts.load(Ordering::SeqCst), 0);
    let e = engine(a, auth.clone(), Store::in_memory().unwrap());
    for r in [r, request("margin", true)] {
        let done = e.execute(r.clone()).await.unwrap();
        assert_eq!(done.status, Status::Completed, "{done:?}");
        assert!(matches!(
            &done.effects.as_ref().unwrap().effects[0],
            FinancialEffect::AuthorityChange { .. }
        ));
        assert_eq!(e.execute(r).await.unwrap().status, Status::Completed);
    }
    assert_eq!(auth.count.load(Ordering::SeqCst), 2);
    assert_eq!(v.posts.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn noops_modes_active_risk_permissions_and_limits_fail_before_post() {
    let v = Arc::new(Venue::new());
    let a = adapter(v.clone());
    let r = request("invalid", false);
    let mut noop = r.input.clone();
    noop["leverage"] = json!(20);
    assert_eq!(
        a.prepare(&r.operation, &noop).await.unwrap_err().code,
        "FUTURES_SETTING_UNCHANGED"
    );
    for toggle in [&v.hedge, &v.multi, &v.orders, &v.algo] {
        toggle.store(true, Ordering::SeqCst);
        assert!(a.prepare(&r.operation, &r.input).await.is_err());
        toggle.store(false, Ordering::SeqCst);
    }
    *v.position.lock().unwrap() = "-0.001".into();
    assert_eq!(
        a.prepare(&r.operation, &r.input).await.unwrap_err().code,
        "FUTURES_SETTING_ACTIVE_RISK"
    );
    *v.position.lock().unwrap() = "0".into();
    v.permission.store(false, Ordering::SeqCst);
    assert_eq!(
        a.prepare(&r.operation, &r.input).await.unwrap_err().code,
        "MISSING_PERMISSION"
    );
    v.permission.store(true, Ordering::SeqCst);
    let mut excess = r.input.clone();
    excess["leverage"] = json!(51);
    assert_eq!(
        a.prepare(&r.operation, &excess).await.unwrap_err().code,
        "FUTURES_LEVERAGE_LIMIT"
    );
    assert_eq!(v.posts.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn strict_inputs_declined_approval_and_expired_preview_never_post() {
    let v = Arc::new(Venue::new());
    let a = adapter(v.clone());
    let r = request("strict", false);
    for bad in [json!("3"), json!(0), json!(-1), json!(3.5), Value::Null] {
        let mut i = r.input.clone();
        i["leverage"] = bad;
        assert!(a.prepare(&r.operation, &i).await.is_err());
    }
    let mut i = r.input.clone();
    i["secret"] = json!("synthetic-only");
    assert!(a.prepare(&r.operation, &i).await.is_err());
    let mut i = request("margin", true);
    i.input["marginType"] = json!("cross");
    assert!(a.prepare(&i.operation, &i.input).await.is_err());
    let mut p = a.prepare(&r.operation, &r.input).await.unwrap();
    p.expires_at = "2020-01-01T00:00:00Z".into();
    assert_eq!(a.revalidate(&p).await.unwrap_err().code, "PREVIEW_EXPIRED");
    let auth = approval();
    auth.deny.store(true, Ordering::SeqCst);
    let e = engine(a, auth, Store::in_memory().unwrap());
    assert_ne!(e.execute(r).await.unwrap().status, Status::Completed);
    assert_eq!(v.posts.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn lost_response_survives_restart_and_proves_only_postcondition_without_replay() {
    let v = Arc::new(Venue::new());
    let a = adapter(v.clone());
    let auth = approval();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("synthetic.sqlite");
    let e = engine(a.clone(), auth.clone(), Store::open(&path).unwrap());
    v.lost.store(true, Ordering::SeqCst);
    v.no_apply.store(true, Ordering::SeqCst);
    let r = request("lost", false);
    let pending = e.execute(r.clone()).await.unwrap();
    assert_eq!(pending.status, Status::Unknown, "{pending:?}");
    drop(e);
    let e = engine(a, auth.clone(), Store::open(&path).unwrap());
    e.recover_interrupted().unwrap();
    assert_eq!(e.execute(r.clone()).await.unwrap().status, Status::Unknown);
    // An external actor can establish the desired state: proof explicitly does not attribute it to this request.
    v.leverage.store(3, Ordering::SeqCst);
    v.config_unavailable.store(true, Ordering::SeqCst);
    assert_eq!(e.execute(r.clone()).await.unwrap().status, Status::Unknown);
    v.config_unavailable.store(false, Ordering::SeqCst);
    let done = e.execute(r).await.unwrap();
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert_eq!(
        done.effects.unwrap().evidence["proofBasis"],
        "authenticated_setting_postcondition"
    );
    assert_eq!(v.posts.load(Ordering::SeqCst), 1);
    assert_eq!(auth.count.load(Ordering::SeqCst), 1);
}
