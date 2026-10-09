//! Synthetic Engine order contract. No HTTP, production accounts, secrets or Touch ID.
use async_trait::async_trait;
use fin_binance::{
    AccountRole, BinanceAccount, BinanceAdapter, BinanceConfig, BinanceEndpoint as E,
    BinancePriceProvider, BinanceRequest, BinanceTransport, CredentialHandle, PriceQuote,
    TransportError,
};
use fin_domain::{
    ApprovalAudit, AuthorizationChallenge, Authorizer, FinError, OperationRecord, OperationRequest,
    ResourceId, Status, SubmissionJournal, now,
};
use fin_engine::{Engine, EngineOptions};
use fin_store::Store;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

const SCALE: i128 = 1_000_000_000_000_000_000;
const SPOT: &str = "binance:uid:1001:SPOT";
const ACCOUNT: &str = "synthetic-main";
#[path = "trading_contract/boundary_lifecycle.rs"]
mod boundary_lifecycle_tests;
#[path = "trading_contract/boundaries.rs"]
mod boundary_tests;
#[path = "trading_contract/plan_permissions.rs"]
mod plan_permission_tests;
fn units(value: &str) -> i128 {
    let negative = value.starts_with('-');
    let value = value.strip_prefix('-').unwrap_or(value);
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    assert!(fraction.len() <= 18);
    let mut result = whole.parse::<i128>().unwrap() * SCALE;
    if !fraction.is_empty() {
        result += fraction.parse::<i128>().unwrap() * 10i128.pow((18 - fraction.len()) as u32);
    }
    if negative {
        result = -result;
    }
    result
}
fn decimal(value: i128) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let value = value.abs();
    let whole = value / SCALE;
    let fraction = format!("{:018}", value % SCALE);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    }
}
#[derive(Clone)]
struct Fill {
    fraction: u32,
    fee_asset: &'static str,
    fee: &'static str,
    pnl: &'static str,
    timeout: bool,
    pending: bool,
}
impl Fill {
    fn full(fee_asset: &'static str, fee: &'static str) -> Self {
        Self {
            fraction: 1000,
            fee_asset,
            fee,
            pnl: "0",
            timeout: false,
            pending: false,
        }
    }
    fn partial() -> Self {
        Self {
            fraction: 500,
            ..Self::full("ETH", "0.00001")
        }
    }
    fn none() -> Self {
        Self {
            fraction: 0,
            ..Self::full("ETH", "0")
        }
    }
}
struct Order {
    row: Value,
    trades: Value,
}
struct VenueState {
    spot: BTreeMap<String, i128>,
    collateral: i128,
    futures_bnb: i128,
    position: i128,
    orders: BTreeMap<String, Order>,
    fills: VecDeque<Fill>,
    funding: Vec<Value>,
}
struct Venue {
    state: Mutex<VenueState>,
    posts: AtomicUsize,
    cancels: AtomicUsize,
    authorized_posts: AtomicUsize,
    seen: Mutex<Vec<BinanceRequest>>,
    filters_changed: AtomicBool,
    discount: AtomicBool,
    futures_burn: AtomicBool,
    mark_price: Mutex<String>,
    hedge: AtomicBool,
    other_exposure: AtomicBool,
    multi_asset: AtomicBool,
    leverage: AtomicUsize,
}
impl Venue {
    fn new(fills: Vec<Fill>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(VenueState {
                spot: BTreeMap::from([
                    ("ETH".into(), 0),
                    ("USDT".into(), units("60")),
                    ("BNB".into(), units("1")),
                ]),
                collateral: units("60"),
                futures_bnb: units("1"),
                position: 0,
                orders: BTreeMap::new(),
                fills: fills.into(),
                funding: Vec::new(),
            }),
            posts: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            authorized_posts: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            filters_changed: AtomicBool::new(false),
            discount: AtomicBool::new(false),
            futures_burn: AtomicBool::new(false),
            mark_price: Mutex::new("2000".into()),
            hedge: AtomicBool::new(false),
            other_exposure: AtomicBool::new(false),
            multi_asset: AtomicBool::new(false),
            leverage: AtomicUsize::new(1),
        })
    }
    fn market(&self, futures: bool) -> Value {
        let mut metadata = json!({"symbols":[{
            "symbol":"ETHUSDT","status":"TRADING","baseAsset":"ETH","quoteAsset":"USDT","marginAsset":"USDT","contractType":"PERPETUAL","isSpotTradingAllowed":true,"orderTypes":["LIMIT"],"timeInForce":["IOC","FOK"],
            "filters":[{"filterType":"PRICE_FILTER","minPrice":"0.01","maxPrice":"1000000","tickSize":if self.filters_changed.load(Ordering::SeqCst){"0.1"}else{"0.01"}},
            {"filterType":"LOT_SIZE","minQty":"0.000000000000000001","maxQty":"100","stepSize":"0.000000000000000001"},
            {"filterType":"MIN_NOTIONAL","minNotional":"5","notional":"5","applyToMarket":true}],"futures":futures
        }]});
        if !futures {
            // The real Spot exchangeInfo contract omits Futures timeInForce.
            metadata["symbols"][0]
                .as_object_mut()
                .unwrap()
                .remove("timeInForce");
        }
        metadata
    }
    fn position(&self, symbol: Option<&str>) -> Value {
        assert!(symbol.is_none() || symbol == Some("ETHUSDT"));
        let state = self.state.lock().unwrap();
        let mut rows = json!([{"symbol":"ETHUSDT","positionSide":"BOTH","positionAmt":decimal(state.position),"entryPrice":if state.position==0{"0"}else{"2000"},"breakEvenPrice":"2000","markPrice":"2000","unRealizedProfit":"0","notional":decimal(state.position*2000),"isolatedMargin":"40","isolatedWallet":"40","initialMargin":"40","maintMargin":"0.2","leverage":"1","marginType":"isolated","updateTime":chrono::Utc::now().timestamp_millis()}]);
        if symbol.is_none() && self.other_exposure.load(Ordering::SeqCst) {
            rows.as_array_mut().unwrap().push(json!({"symbol":"BTCUSDT","positionSide":"BOTH","positionAmt":"0.001","entryPrice":"60000","markPrice":"60000","unRealizedProfit":"0","notional":"60","isolatedMargin":"60","isolatedWallet":"60","updateTime":chrono::Utc::now().timestamp_millis()}));
        }
        rows
    }
    fn dispatch(&self, request: BinanceRequest, authorized: bool) -> Result<Value, TransportError> {
        request
            .validate()
            .expect("synthetic request uses closed, validated endpoints");
        self.seen.lock().unwrap().push(request.clone());
        match request.endpoint {
            E::ApiRestrictions => Ok(
                json!({"enableReading":true,"enableSpotAndMarginTrading":true,"enableFutures":true,"enableInternalTransfer":true,"permitsUniversalTransfer":true}),
            ),
            E::Account => {
                let state = self.state.lock().unwrap();
                Ok(
                    json!({"uid":1001,"accountType":"SPOT","canTrade":true,"balances":state.spot.iter().map(|(asset,free)|json!({"asset":asset,"free":decimal(*free),"locked":"0"})).collect::<Vec<_>>()}),
                )
            }
            E::SpotExchangeInfo => Ok(self.market(false)),
            E::FuturesExchangeInfo => Ok(self.market(true)),
            E::SpotCommission => Ok(
                json!({"symbol":"ETHUSDT","standardCommission":{"maker":"0.001","taker":"0.001","buyer":"0","seller":"0"},"taxCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"specialCommission":{"maker":"0","taker":"0","buyer":"0","seller":"0"},"discount":{"enabledForAccount":self.discount.load(Ordering::SeqCst),"enabledForSymbol":self.discount.load(Ordering::SeqCst),"discountAsset":"BNB","discount":"0.25"}}),
            ),
            E::FuturesCommission => Ok(
                json!({"symbol":"ETHUSDT","makerCommissionRate":"0.0002","takerCommissionRate":"0.001"}),
            ),
            E::FuturesAccountConfig => Ok(
                json!({"dualSidePosition":self.hedge.load(Ordering::SeqCst),"multiAssetsMargin":self.multi_asset.load(Ordering::SeqCst),"canTrade":true}),
            ),
            E::FuturesFeeBurn => Ok(json!({"feeBurn": self.futures_burn.load(Ordering::SeqCst)})),
            E::FuturesPositionMode => {
                Ok(json!({"dualSidePosition":self.hedge.load(Ordering::SeqCst)}))
            }
            E::FuturesSymbolConfig => Ok(
                json!([{"symbol":"ETHUSDT","marginType":"ISOLATED","leverage":self.leverage.load(Ordering::SeqCst),"isAutoAddMargin":false}]),
            ),
            E::FuturesPositions => Ok(self.position(request.get("symbol"))),
            E::FuturesAccount => {
                let state = self.state.lock().unwrap();
                let margin = state.position.abs() * 2000;
                let available = state.collateral - margin;
                let mut assets = vec![
                    json!({"asset":"USDT","walletBalance":decimal(state.collateral),"availableBalance":decimal(available),"marginBalance":decimal(state.collateral),"unrealizedProfit":"0","initialMargin":decimal(margin),"maintMargin":"0","maxWithdrawAmount":decimal(available)}),
                ];
                if self.futures_burn.load(Ordering::SeqCst) {
                    assets.push(json!({"asset":"BNB","walletBalance":decimal(state.futures_bnb),"availableBalance":decimal(state.futures_bnb),"marginBalance":decimal(state.futures_bnb),"unrealizedProfit":"0","initialMargin":"0","maintMargin":"0","maxWithdrawAmount":decimal(state.futures_bnb)}));
                }
                Ok(
                    json!({"canTrade":true,"totalWalletBalance":decimal(state.collateral),"availableBalance":decimal(available),"assets":assets,"positions":[]}),
                )
            }
            E::FuturesMarkPrice => Ok(
                json!({"symbol":"ETHUSDT","markPrice":self.mark_price.lock().unwrap().clone(),"indexPrice":"2000","time":chrono::Utc::now().timestamp_millis(),"lastFundingRate":"0","nextFundingTime":chrono::Utc::now().timestamp_millis()+28_800_000}),
            ),
            E::FuturesLeverageBrackets => Ok(
                json!([{"symbol":"ETHUSDT","brackets":[{"bracket":1,"initialLeverage":1,"notionalCap":"1000000","notionalFloor":"0","maintMarginRatio":"0.005","cum":"0"}]}]),
            ),
            E::SpotOpenOrders | E::FuturesOpenOrders | E::DepositHistory | E::WithdrawalHistory => {
                Ok(json!([]))
            }
            E::FuturesIncome => {
                let start = request
                    .get("startTime")
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let end = request
                    .get("endTime")
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(i64::MAX);
                Ok(json!(
                    self.state
                        .lock()
                        .unwrap()
                        .funding
                        .iter()
                        .filter(|row| row["time"].as_i64().unwrap() >= start
                            && row["time"].as_i64().unwrap() <= end)
                        .cloned()
                        .collect::<Vec<_>>()
                ))
            }
            E::UniversalTransferHistory => Ok(json!({"totalCount":0,"result":[]})),
            E::SpotPlaceOrder | E::FuturesPlaceOrder => {
                assert!(authorized, "fixture refuses unguarded order writes");
                self.posts.fetch_add(1, Ordering::SeqCst);
                self.authorized_posts.fetch_add(1, Ordering::SeqCst);
                let futures = request.endpoint == E::FuturesPlaceOrder;
                let client = request.get("newClientOrderId").unwrap().to_owned();
                let side = request.get("side").unwrap();
                let buy = side == "BUY";
                let mut state = self.state.lock().unwrap();
                assert!(
                    !state.orders.contains_key(&client),
                    "duplicate original client ID must never be sent"
                );
                let fill = state
                    .fills
                    .pop_front()
                    .expect("bounded scripted order write");
                let requested = units(request.get("quantity").unwrap());
                let quantity = requested * i128::from(fill.fraction) / 1000;
                let price = units(request.get("price").unwrap());
                let quote = quantity * price / SCALE;
                let fee = units(fill.fee);
                let pnl = units(fill.pnl);
                if futures {
                    state.position += if buy { quantity } else { -quantity };
                    state.collateral += pnl;
                    if fill.fee_asset == "BNB" {
                        state.futures_bnb -= fee;
                    } else {
                        state.collateral -= fee;
                    }
                } else {
                    *state.spot.get_mut("ETH").unwrap() += if buy { quantity } else { -quantity };
                    *state.spot.get_mut("USDT").unwrap() += if buy { -quote } else { quote };
                    *state.spot.get_mut(fill.fee_asset).unwrap() -= fee;
                }
                let order_id = 9007199254740993u64 + self.posts.load(Ordering::SeqCst) as u64;
                let timestamp = chrono::Utc::now().timestamp_millis();
                let row = json!({"symbol":"ETHUSDT","orderId":order_id,"clientOrderId":client,"status":if fill.pending{"PARTIALLY_FILLED"}else if fill.fraction==1000{"FILLED"}else{"EXPIRED"},"side":side,"type":"LIMIT","timeInForce":request.get("timeInForce").unwrap(),"price":decimal(price),"origQty":decimal(requested),"executedQty":decimal(quantity),"cummulativeQuoteQty":decimal(quote),"cumQuote":decimal(quote),"avgPrice":if quantity==0{"0"}else{"2000"},"reduceOnly":request.get("reduceOnly")==Some("true"),"positionSide":"BOTH","time":timestamp,"updateTime":timestamp});
                let trades = if quantity == 0 {
                    json!([])
                } else {
                    json!([{"symbol":"ETHUSDT","id":order_id+100,"orderId":order_id,"price":decimal(price),"qty":decimal(quantity),"quoteQty":decimal(quote),"commission":fill.fee,"commissionAsset":fill.fee_asset,"time":timestamp,"isBuyer":buy,"buyer":buy,"isMaker":false,"maker":false,"side":side,"positionSide":"BOTH","realizedPnl":fill.pnl}])
                };
                state.orders.insert(
                    client,
                    Order {
                        row: row.clone(),
                        trades,
                    },
                );
                if fill.timeout {
                    Err(TransportError::Timeout)
                } else {
                    Ok(row)
                }
            }
            E::SpotCancelOrder | E::FuturesCancelOrder => {
                assert!(
                    authorized,
                    "synthetic cancellation must use a fresh authorized guard"
                );
                self.cancels.fetch_add(1, Ordering::SeqCst);
                let id = request
                    .get("orderId")
                    .expect("cancel exact immutable venue ID");
                let mut state = self.state.lock().unwrap();
                let order = state
                    .orders
                    .values_mut()
                    .find(|order| order.row["orderId"].as_u64().unwrap().to_string() == id)
                    .unwrap();
                assert_eq!(order.row["status"], "PARTIALLY_FILLED");
                order.row["status"] = json!("CANCELED");
                if let Some(cancel_id) = request.get("newClientOrderId") {
                    order.row["origClientOrderId"] = order.row["clientOrderId"].clone();
                    order.row["clientOrderId"] = json!(cancel_id);
                }
                Ok(order.row.clone())
            }
            E::SpotOrder | E::FuturesOrder => {
                let state = self.state.lock().unwrap();
                let order = if let Some(id) = request.get("orderId") {
                    state
                        .orders
                        .values()
                        .find(|order| order.row["orderId"].as_u64().unwrap().to_string() == id)
                } else {
                    let id = request
                        .get("origClientOrderId")
                        .expect("reconciliation supplies an exact client ID");
                    state
                        .orders
                        .values()
                        .find(|order| order.row["clientOrderId"] == id)
                };
                match order {
                    Some(order) => Ok(order.row.clone()),
                    None => Err(TransportError::Rejected),
                }
            }
            E::SpotTrades | E::FuturesTrades => {
                let state = self.state.lock().unwrap();
                let id = request.get("orderId").unwrap();
                Ok(state
                    .orders
                    .values()
                    .find(|o| o.row["orderId"].as_u64().unwrap().to_string() == id)
                    .expect("trades correlated by immutable venue ID")
                    .trades
                    .clone())
            }
            _ => Err(TransportError::InvalidResponse),
        }
    }
}
#[async_trait]
impl BinanceTransport for Venue {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        assert_eq!(credential.as_str(), "test-only:synthetic-trading");
        self.dispatch(request, false)
    }
    async fn request_authorized(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value, TransportError> {
        assert_eq!(credential.as_str(), "test-only:synthetic-trading");
        journal
            .assert_authorized()
            .map_err(|_| TransportError::PermissionDenied)?;
        self.dispatch(request, true)
    }
}
struct Prices {
    changed: AtomicBool,
}
#[async_trait]
impl BinancePriceProvider for Prices {
    async fn quote_usd(&self, coin: &str) -> fin_domain::Result<PriceQuote> {
        Ok(PriceQuote {
            usd_per_unit: match coin {
                "ETH" => "2000",
                "BNB" => "300",
                "USDT" => {
                    if self.changed.load(Ordering::SeqCst) {
                        "1.01"
                    } else {
                        "1"
                    }
                }
                _ => panic!("unconfigured synthetic asset"),
            }
            .into(),
            observed_at: now(),
            source: "synthetic-only:fixed-prices".into(),
        })
    }
}
#[async_trait]
impl fin_engine::TrustedAssetPriceProvider for Prices {
    async fn asset_price(
        &self,
        asset: &fin_domain::AssetId,
    ) -> fin_domain::Result<fin_domain::AssetPrice> {
        assert_eq!(asset.network, "binance");
        let quote = self.quote_usd(&asset.reference).await?;
        Ok(fin_domain::AssetPrice {
            asset: asset.clone(),
            usd_micros_per_unit: fin_domain::Quantity::from_decimal(&quote.usd_per_unit, 6)?,
            unit_atoms: fin_domain::Quantity::from_decimal("1", asset.decimals)?,
            observed_at: quote.observed_at,
            source: quote.source,
        })
    }
}

#[derive(Clone, Copy)]
enum ApprovalMode {
    Accept,
    Deny,
    Expired,
}
struct Approval {
    calls: AtomicUsize,
    mode: ApprovalMode,
    mutate: Option<Arc<dyn Fn() + Send + Sync>>,
}
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> fin_domain::Result<ApprovalAudit> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(mutate) = &self.mutate {
            mutate();
        }
        if matches!(self.mode, ApprovalMode::Deny) {
            return Err(FinError::new(
                "AUTHORIZATION_DENIED",
                "approval",
                "Synthetic human denied this fixture.",
                "No financial action occurred.",
            ));
        }
        Ok(ApprovalAudit {
            mechanism: "test-only-synthetic-order-approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: if matches!(self.mode, ApprovalMode::Expired) {
                "2020-01-01T00:00:00Z".into()
            } else {
                challenge.expires_at.clone()
            },
        })
    }
}
fn config() -> BinanceConfig {
    BinanceConfig {
        accounts: vec![BinanceAccount {
            name: ACCOUNT.into(),
            resource: ResourceId(SPOT.into()),
            credential: CredentialHandle::new("test-only:synthetic-trading").unwrap(),
            role: AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        }],
        destinations: vec![],
    }
}
fn adapter(venue: Arc<Venue>, prices: Arc<Prices>) -> Arc<BinanceAdapter> {
    Arc::new(BinanceAdapter::new(config(), venue, Some(prices), None).unwrap())
}
fn approve(mode: ApprovalMode) -> Arc<Approval> {
    Arc::new(Approval {
        calls: AtomicUsize::new(0),
        mode,
        mutate: None,
    })
}
fn prices() -> Arc<Prices> {
    Arc::new(Prices {
        changed: AtomicBool::new(false),
    })
}
fn make_engine(
    venue: Arc<Venue>,
    prices: Arc<Prices>,
    authorizer: Arc<Approval>,
    store: Store,
) -> Engine {
    let cost_prices = prices.clone();
    Engine::new(
        store,
        vec![adapter(venue, prices)],
        authorizer,
        EngineOptions {
            max_run_allocation_usd_micros: "1000000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(cost_prices)
}
fn input(id: &str, side: &str, futures: bool) -> Value {
    let mut v = json!({"request_id":id,"account":ACCOUNT,"symbol":"ETHUSDT","side":side,"quantity":"0.02","limit_price":"2000","time_in_force":"IOC","maximum_fees":if futures||side=="SELL"{json!({"USDT":"0.05"})}else{json!({"ETH":"0.00003"})},"reduce_only":false});
    if futures {
        v["maximum_loss"] = json!("0.3");
        v["maximum_profit"] = json!("0.3");
    }
    v
}
fn request(input: Value, futures: bool) -> OperationRequest {
    OperationRequest {
        request_id: input["request_id"].as_str().unwrap().into(),
        operation: if futures {
            "binance.futures.order-place"
        } else {
            "binance.spot.order-place"
        }
        .into(),
        input,
        run_id: None,
    }
}
fn assert_no_post(record: &OperationRecord, venue: &Venue) {
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    assert!(record.submission.is_none());
    assert!(
        matches!(
            record.status,
            Status::Failed | Status::Rejected | Status::Cancelled | Status::Expired
        ),
        "{record:?}"
    );
}

#[tokio::test]
async fn spot_full_partial_and_unfilled_orders_settle_exact_fees() {
    for (fill, expected_eth, expected_usdt) in [
        (
            Fill::full("ETH", "0.000020000000000001"),
            "0.019979999999999999",
            "20",
        ),
        (Fill::partial(), "0.00999", "40"),
        (Fill::none(), "0", "60"),
    ] {
        let fee = fill.fee;
        let venue = Venue::new(vec![fill]);
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let record = engine
            .execute(request(input("synthetic-spot-fill", "BUY", false), false))
            .await
            .unwrap();
        assert_eq!(
            record.status,
            if fee == "0" {
                Status::Failed
            } else {
                Status::Completed
            },
            "{record:?}"
        );
        assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
        let state = venue.state.lock().unwrap();
        assert_eq!(decimal(state.spot["ETH"]), expected_eth);
        assert_eq!(decimal(state.spot["USDT"]), expected_usdt);
        let verified = record
            .effects
            .as_ref()
            .expect("terminal order requires verified effects");
        let charged = verified
            .effects
            .iter()
            .filter_map(|effect| match effect {
                fin_domain::FinancialEffect::Cost {
                    amount, treatment, ..
                } => {
                    assert_eq!(amount.asset.reference, "ETH");
                    assert_eq!(*treatment, fin_domain::CostTreatment::DeductedFromCredit);
                    Some(amount.atoms.to_decimal(36))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            charged,
            if fee == "0" {
                Vec::<String>::new()
            } else {
                vec![fee.to_owned()]
            }
        );
        if fee == "0" {
            assert_eq!(verified.evidence["noFill"], true);
        } else {
            assert_eq!(verified.evidence["balanceChangesVerified"], true);
        }
    }
}

#[tokio::test]
async fn spot_bnb_fee_is_reserved_and_original_timeout_recovers_after_restart_without_post() {
    let mut fill = Fill::full("BNB", "0.00005");
    fill.timeout = true;
    let venue = Venue::new(vec![fill]);
    venue.discount.store(true, Ordering::SeqCst);
    let auth = approve(ApprovalMode::Accept);
    let prices = prices();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-orders.sqlite");
    let engine = make_engine(
        venue.clone(),
        prices.clone(),
        auth.clone(),
        Store::open(&path).unwrap(),
    );
    let mut data = input("synthetic-timeout-client-id", "BUY", false);
    data["maximum_fees"]["BNB"] = json!("0.0001");
    let operation = request(data, false);
    let initial = engine.execute(operation.clone()).await.unwrap();
    assert_eq!(initial.status, Status::Unknown);
    let submission = initial.submission.clone().unwrap();
    drop(engine);
    let restarted = make_engine(
        venue.clone(),
        prices,
        auth.clone(),
        Store::open(&path).unwrap(),
    );
    let recovered = restarted.status(&initial.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Completed, "{recovered:?}");
    assert_eq!(recovered.submission, Some(submission));
    let duplicate = restarted.execute(operation).await.unwrap();
    assert_eq!(duplicate.id, initial.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    assert_eq!(decimal(venue.state.lock().unwrap().spot["BNB"]), "0.99995");
}

#[tokio::test]
async fn authorization_denial_and_expiry_never_send_orders() {
    for mode in [ApprovalMode::Deny, ApprovalMode::Expired] {
        let venue = Venue::new(vec![]);
        let auth = approve(mode);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let record = engine
            .execute(request(
                input("synthetic-denied-order", "BUY", false),
                false,
            ))
            .await
            .unwrap();
        assert_no_post(&record, &venue);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn filters_and_valuation_changes_after_approval_reject_without_post() {
    for change_price in [false, true] {
        let venue = Venue::new(vec![]);
        let prices = prices();
        let copied_venue = venue.clone();
        let copied_prices = prices.clone();
        let auth = Arc::new(Approval {
            calls: AtomicUsize::new(0),
            mode: ApprovalMode::Accept,
            mutate: Some(Arc::new(move || {
                if change_price {
                    copied_prices.changed.store(true, Ordering::SeqCst);
                } else {
                    copied_venue.filters_changed.store(true, Ordering::SeqCst);
                }
            })),
        });
        let engine = make_engine(
            venue.clone(),
            prices,
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let record = engine
            .execute(request(
                input("synthetic-material-order-change", "BUY", false),
                false,
            ))
            .await
            .unwrap();
        assert_no_post(&record, &venue);
        assert!(record.approval.is_some());
        assert_eq!(record.error.as_ref().unwrap().code, "PREVIEW_CHANGED");
    }
}

#[tokio::test]
async fn free_balance_is_checked_before_authorization() {
    for (quantity, free) in [("0.02", "10")] {
        let venue = Venue::new(vec![]);
        venue
            .state
            .lock()
            .unwrap()
            .spot
            .insert("USDT".into(), units(free));
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let mut data = input("synthetic-cap-or-funding", "BUY", false);
        data["quantity"] = json!(quantity);
        data["maximum_fees"]["ETH"] = json!("0.0001");
        let record = engine.execute(request(data, false)).await.unwrap();
        assert_no_post(&record, &venue);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn futures_long_and_short_open_then_reduce_only_close_account_for_fees_and_loss() {
    for open_side in ["BUY", "SELL"] {
        let mut close = Fill::full("USDT", "0.04");
        close.pnl = "-0.2";
        let venue = Venue::new(vec![Fill::full("USDT", "0.04"), close]);
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let opened = engine
            .execute(request(
                input("synthetic-futures-open", open_side, true),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        let mut close_input = input(
            "synthetic-futures-close",
            if open_side == "BUY" { "SELL" } else { "BUY" },
            true,
        );
        close_input["reduce_only"] = json!(true);
        let closed = engine.execute(request(close_input, true)).await.unwrap();
        assert_eq!(closed.status, Status::Completed, "{closed:?}");
        let state = venue.state.lock().unwrap();
        assert_eq!(state.position, 0);
        assert_eq!(decimal(state.collateral), "59.72");
        assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn futures_hedge_multiasset_leverage_margin_and_reversed_close_fail_before_approval() {
    for failure in 0..5 {
        let venue = Venue::new(vec![]);
        match failure {
            0 => venue.hedge.store(true, Ordering::SeqCst),
            1 => venue.multi_asset.store(true, Ordering::SeqCst),
            2 => venue.leverage.store(2, Ordering::SeqCst),
            3 => venue.state.lock().unwrap().collateral = units("10"),
            4 => venue.state.lock().unwrap().position = units("0.02"),
            _ => unreachable!(),
        };
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let mut data = input("synthetic-futures-invalid-mode", "BUY", true);
        if failure == 4 {
            data["reduce_only"] = json!(true);
        }
        let record = engine.execute(request(data, true)).await.unwrap();
        assert_no_post(&record, &venue);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
    }
}

fn priced_asset(coin: &str) -> fin_domain::AssetId {
    fin_domain::AssetId {
        network: "binance".into(),
        reference: coin.into(),
        decimals: 36,
    }
}
fn trusted_prices() -> Vec<fin_domain::AssetPrice> {
    [
        ("ETH", "2000000000"),
        ("USDT", "1000000"),
        ("BNB", "300000000"),
    ]
    .into_iter()
    .map(|(coin, price)| fin_domain::AssetPrice {
        asset: priced_asset(coin),
        usd_micros_per_unit: fin_domain::Quantity::new(price).unwrap(),
        unit_atoms: fin_domain::Quantity::from_decimal("1", 36).unwrap(),
        observed_at: now(),
        source: "synthetic-only:fixed-prices".into(),
    })
    .collect()
}
async fn round_trip_inputs(id: &str, futures: bool, open_side: &str) -> (Value, Value) {
    let mut open = input(&format!("{id}-open"), open_side, futures);
    let mut close = input(
        &format!("{id}-close"),
        if open_side == "BUY" { "SELL" } else { "BUY" },
        futures,
    );
    close["quantity"] = json!("all");
    close["maximum_quantity"] = json!("0.02");
    if futures {
        close["reduce_only"] = json!(true);
        for data in [&mut open, &mut close] {
            data["maximum_funding_loss"] = json!("0.1");
            data["maximum_funding_profit"] = json!("0.1");
        }
    } else {
        open["time_in_force"] = json!("FOK");
        close["time_in_force"] = json!("FOK");
    }
    (open, close)
}

#[tokio::test]
async fn ordinary_spot_buy_sell_settles_cash_and_exposure_net_exact_fees() {
    let venue = Venue::new(vec![
        Fill::full("ETH", "0.00002"),
        Fill::full("USDT", "0.03996"),
    ]);
    let auth = approve(ApprovalMode::Accept);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter(venue.clone(), prices())],
        auth.clone(),
        EngineOptions {
            max_run_allocation_usd_micros: "1000000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(prices());
    let id = "synthetic-ordinary-spot";
    let (open, close) = round_trip_inputs(id, false, "BUY").await;
    for data in [open, close] {
        let request = request(data, false);
        let settled = engine.execute(request).await.unwrap();
        assert_eq!(settled.status, Status::Completed, "{settled:?}");
        assert!(settled.run_id.is_none());
        assert!(settled.effects.is_some());
    }
    let records = engine.operations().unwrap();
    assert_eq!(records.len(), 2);
    assert_ne!(
        records[0].submission.as_ref().unwrap().reference,
        records[1].submission.as_ref().unwrap().reference
    );
    let state = venue.state.lock().unwrap();
    assert_eq!(state.spot["ETH"], 0);
    assert_eq!(decimal(state.spot["USDT"]), "59.92004");
    assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn ordinary_futures_open_close_settles_exposure_with_proven_fees_and_pnl_loss() {
    for open_side in ["BUY", "SELL"] {
        let mut close = Fill::full("USDT", "0.04");
        close.pnl = "-0.2";
        let venue = Venue::new(vec![Fill::full("USDT", "0.04"), close]);
        let auth = approve(ApprovalMode::Accept);
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter(venue.clone(), prices())],
            auth.clone(),
            EngineOptions {
                max_run_allocation_usd_micros: "1000000000".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .with_cost_origin_price_provider(prices());
        let id = "synthetic-ordinary-futures";
        let (open, close) = round_trip_inputs(id, true, open_side).await;
        for data in [open, close] {
            let request = request(data, true);
            let settled = engine.execute(request).await.unwrap();
            assert_eq!(settled.status, Status::Completed, "{settled:?}");
            assert!(settled.run_id.is_none());
            assert!(settled.effects.is_some());
        }
        let records = engine.operations().unwrap();
        assert_eq!(records.len(), 2);
        assert_ne!(
            records[0].submission.as_ref().unwrap().reference,
            records[1].submission.as_ref().unwrap().reference
        );
        let state = venue.state.lock().unwrap();
        assert_eq!(state.position, 0);
        assert_eq!(decimal(state.collateral), "59.72");
        assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn retired_run_id_refuses_new_write_before_approval() {
    let venue = Venue::new(vec![]);
    let auth = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::in_memory().unwrap(),
    );
    let mut operation = request(input("synthetic-retired-run-order", "BUY", false), false);
    operation.run_id = Some("synthetic-retired-run".into());
    let rejected = engine.execute(operation.clone()).await.unwrap();
    assert_eq!(rejected.status, Status::Rejected);
    assert_eq!(rejected.error.as_ref().unwrap().code, "RUN_RETIRED");
    assert!(rejected.preview.is_none());
    assert!(rejected.submission.is_none());
    let duplicate = engine.execute(operation).await.unwrap();
    assert_eq!(duplicate.id, rejected.id);
    assert_eq!(engine.operations().unwrap().len(), 1);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
    assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn spot_sell_cleanup_uses_observed_received_quantity_and_its_own_fresh_approval() {
    let venue = Venue::new(vec![
        Fill::full("ETH", "0.00002"),
        Fill::full("USDT", "0.03996"),
    ]);
    let auth = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::in_memory().unwrap(),
    );
    let opened = engine
        .execute(request(
            input("synthetic-buy-before-cleanup", "BUY", false),
            false,
        ))
        .await
        .unwrap();
    assert_eq!(opened.status, Status::Completed, "{opened:?}");
    let mut cleanup = input("synthetic-sell-cleanup", "SELL", false);
    cleanup["quantity"] = json!("all");
    cleanup["maximum_quantity"] = json!("0.02");
    let closed = engine.execute(request(cleanup, false)).await.unwrap();
    assert_eq!(closed.status, Status::Completed, "{closed:?}");
    let requests = venue.seen.lock().unwrap();
    let sent = requests
        .iter()
        .filter(|request| request.endpoint == E::SpotPlaceOrder)
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].get("quantity"), Some("0.01998"));
    assert_eq!(sent[1].get("side"), Some("SELL"));
    assert_ne!(
        sent[0].get("newClientOrderId"),
        sent[1].get("newClientOrderId")
    );
    assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
    let state = venue.state.lock().unwrap();
    assert_eq!(state.spot["ETH"], 0);
    assert_eq!(decimal(state.spot["USDT"]), "59.92004");
}

#[tokio::test]
async fn missing_fill_proof_retains_original_unknown_identity_without_another_write() {
    let mut fill = Fill::full("ETH", "0.00002");
    fill.timeout = true;
    let venue = Venue::new(vec![fill]);
    let auth = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::in_memory().unwrap(),
    );
    let operation = request(input("synthetic-missing-fill-proof", "BUY", false), false);
    let initial = engine.execute(operation.clone()).await.unwrap();
    assert_eq!(initial.status, Status::Unknown);
    let original = initial.submission.clone().unwrap();
    venue
        .state
        .lock()
        .unwrap()
        .orders
        .get_mut(&original.reference)
        .unwrap()
        .trades = json!([]);
    let inspected = engine.status(&initial.id, true).await.unwrap();
    assert_eq!(inspected.status, Status::Unknown);
    assert_eq!(inspected.submission, Some(original));
    assert!(inspected.effects.is_none());
    let duplicate = engine.execute(operation).await.unwrap();
    assert_eq!(duplicate.id, initial.id);
    assert_eq!(duplicate.status, Status::Unknown);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ordinary_policy_blocks_allocation_over_100_usd_before_approval() {
    let venue = Venue::new(vec![]);
    venue.state.lock().unwrap().collateral = units("200");
    let auth = approve(ApprovalMode::Accept);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter(venue.clone(), prices())],
        auth.clone(),
        EngineOptions {
            max_movement_and_cost_usd_micros: "0".into(),
            max_run_allocation_usd_micros: "100000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(prices());
    let mut open = input("synthetic-102-dollar-open", "BUY", true);
    open["quantity"] = json!("0.051");
    open["maximum_fees"] = json!({"USDT":"0.2"});
    let record = engine.execute(request(open, true)).await.unwrap();
    assert_no_post(&record, &venue);
    assert_eq!(record.error.as_ref().unwrap().code, "ALLOCATION_LIMIT");
    assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_cancellation_owns_no_fills_and_original_reconciles_partial_trade_once() {
    for futures in [false, true] {
        let mut fill = if futures {
            Fill::full("USDT", "0.02")
        } else {
            Fill::partial()
        };
        fill.fraction = 500;
        fill.pending = true;
        let venue = Venue::new(vec![fill]);
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let original_request = request(input("synthetic-open-remainder", "BUY", futures), futures);
        let original = engine.execute(original_request.clone()).await.unwrap();
        assert_eq!(original.status, Status::Pending, "{original:?}");
        assert!(original.effects.is_none());
        let identity = original.submission.clone().unwrap();
        let cancel_request = OperationRequest {
            request_id: "synthetic-cancel-remainder".into(),
            operation: if futures {
                "binance.futures.order-cancel"
            } else {
                "binance.spot.order-cancel"
            }
            .into(),
            input: json!({"request_id":"synthetic-cancel-remainder","account":ACCOUNT,"symbol":"ETHUSDT","operation_id":original.id,"original_submission":identity}),
            run_id: None,
        };
        let cancel = engine.execute(cancel_request.clone()).await.unwrap();
        assert_eq!(cancel.status, Status::Completed, "{cancel:?}");
        let effects = &cancel.effects.as_ref().unwrap().effects;
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(
                effects[0],
                fin_domain::FinancialEffect::SubmissionCancellation { .. }
            ),
            "{effects:?}"
        );
        let reconciled = engine.status(&original.id, true).await.unwrap();
        assert_eq!(reconciled.status, Status::Completed, "{reconciled:?}");
        assert_eq!(reconciled.submission, Some(identity));
        assert!(
            reconciled
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .any(|effect| matches!(effect, fin_domain::FinancialEffect::Cost { .. }))
        );
        let duplicate_original = engine.execute(original_request).await.unwrap();
        let duplicate_cancel = engine.execute(cancel_request).await.unwrap();
        assert_eq!(duplicate_original.id, original.id);
        assert_eq!(duplicate_cancel.id, cancel.id);
        assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
        assert_eq!(venue.cancels.load(Ordering::SeqCst), 1);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
    }
}

async fn fund_between_legs(venue: &Venue, income: &str) {
    // A different millisecond than the opening anchor models exchange funding
    // that settles between writes, without changing the recorded old cursor.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let mut state = venue.state.lock().unwrap();
    state.collateral += units(income);
    state.funding.push(json!({"symbol":"ETHUSDT","incomeType":"FUNDING_FEE","asset":"USDT","income":income,"tranId":9007199254741999u64,"time":chrono::Utc::now().timestamp_millis()}));
}

#[tokio::test]
async fn ordinary_funding_loss_and_profit_between_legs_settle_using_original_cursor() {
    for income in ["-0.05", "0.05"] {
        let venue = Venue::new(vec![Fill::full("USDT", "0.04"), Fill::full("USDT", "0.04")]);
        let auth = approve(ApprovalMode::Accept);
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter(venue.clone(), prices())],
            auth.clone(),
            EngineOptions {
                max_run_allocation_usd_micros: "1000000000".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .with_cost_origin_price_provider(prices());
        let id = "synthetic-funding-roundtrip";
        let (open, close) = round_trip_inputs(id, true, "BUY").await;
        let opening = request(open, true);
        let opened = engine.execute(opening).await.unwrap();
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        fund_between_legs(&venue, income).await;
        let closing = request(close, true);
        let closed = engine.execute(closing).await.unwrap();
        assert_eq!(closed.status, Status::Completed, "{closed:?}");
        let effects = &closed.effects.as_ref().unwrap().effects;
        let funding_atoms =
            fin_domain::Quantity::from_decimal(income.trim_start_matches('-'), 36).unwrap();
        let expected_delta = if income.starts_with('-') {
            fin_domain::SignedQuantity::negative(&funding_atoms)
        } else {
            fin_domain::SignedQuantity::positive(&funding_atoms)
        };
        assert!(effects.iter().any(|effect| matches!(effect, fin_domain::FinancialEffect::Settlement { category: fin_domain::SettlementCategory::Funding, delta, .. } if *delta == expected_delta)));
        assert_eq!(effects.iter().filter(|effect| matches!(effect, fin_domain::FinancialEffect::Cost { category, treatment: fin_domain::CostTreatment::IncludedInDebit, amount, .. } if category == "funding_loss" && amount.atoms == funding_atoms)).count(), usize::from(income.starts_with('-')));
        assert_eq!(venue.state.lock().unwrap().position, 0);
        assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            decimal(venue.state.lock().unwrap().collateral),
            decimal(units("60") + units(income) - units("0.08"))
        );
    }
}

#[tokio::test]
async fn funding_above_frozen_loss_cap_remains_unknown_without_budget_increase_or_replay() {
    let venue = Venue::new(vec![Fill::full("USDT", "0.04"), Fill::full("USDT", "0.04")]);
    let auth = approve(ApprovalMode::Accept);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter(venue.clone(), prices())],
        auth.clone(),
        EngineOptions {
            max_run_allocation_usd_micros: "1000000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(prices());
    let id = "synthetic-funding-cap";
    let (open, close) = round_trip_inputs(id, true, "BUY").await;
    let opening = request(open, true);
    let opened = engine.execute(opening).await.unwrap();
    assert_eq!(opened.status, Status::Completed, "{opened:?}");
    fund_between_legs(&venue, "-0.2").await;
    let closing = request(close, true);
    let outcome = engine.execute(closing.clone()).await.unwrap();
    assert_ne!(outcome.status, Status::Completed, "{outcome:?}");
    assert!(outcome.error.is_some() || outcome.last_reconciliation_evidence.is_some());
    let before = venue.posts.load(Ordering::SeqCst);
    let duplicate = engine.execute(closing).await.unwrap();
    assert_eq!(duplicate.id, outcome.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), before);
    assert!(outcome.effects.is_none());
    assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn low_limit_futures_sell_uses_fresh_mark_for_margin_and_personal_risk_ceiling() {
    let venue = Venue::new(vec![]);
    venue.state.lock().unwrap().collateral = units("200");
    *venue.mark_price.lock().unwrap() = "6000".into();
    let auth = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::in_memory().unwrap(),
    );
    let mut open = input("synthetic-low-limit-short", "SELL", true);
    open["limit_price"] = json!("500");
    open["maximum_fees"] = json!({"USDT":"0.15"});
    let record = engine.execute(request(open, true)).await.unwrap();
    assert_no_post(&record, &venue);
    assert_eq!(
        record.error.as_ref().unwrap().code,
        "MOVEMENT_LIMIT",
        "{record:?}"
    );
    assert_eq!(
        record
            .preview
            .as_ref()
            .unwrap()
            .valuation
            .as_ref()
            .unwrap()
            .movement_usd_micros
            .to_string(),
        "120000000"
    );
    assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn enabled_futures_bnb_burn_requires_reserved_fee_asset_and_preserves_its_wallet() {
    for reserve_bnb in [false, true] {
        let venue = Venue::new(if reserve_bnb {
            vec![Fill::full("BNB", "0.00005")]
        } else {
            vec![]
        });
        venue.futures_burn.store(true, Ordering::SeqCst);
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let mut data = input("synthetic-futures-bnb-fee", "BUY", true);
        if reserve_bnb {
            data["maximum_fees"]["BNB"] = json!("0.0001");
        }
        let record = engine.execute(request(data, true)).await.unwrap();
        if reserve_bnb {
            assert_eq!(record.status, Status::Completed, "{record:?}");
            assert_eq!(decimal(venue.state.lock().unwrap().futures_bnb), "0.99995");
            assert_eq!(decimal(venue.state.lock().unwrap().collateral), "60");
            assert!(record.effects.as_ref().unwrap().effects.iter().any(|effect|matches!(effect,fin_domain::FinancialEffect::Cost{amount,treatment:fin_domain::CostTreatment::AdditionalDebit,..} if amount.asset.reference=="BNB" && amount.atoms.to_decimal(36)=="0.00005")));
        } else {
            assert_no_post(&record, &venue);
            assert_eq!(record.error.as_ref().unwrap().code, "FEE_ASSET_UNRESERVED");
            assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn ordinary_usdm_ioc_partial_cancel_then_close_settles_original_once() {
    let mut partial = Fill::full("USDT", "0.02");
    partial.fraction = 500;
    partial.pending = true;
    let venue = Venue::new(vec![partial, Fill::full("USDT", "0.02")]);
    let auth = approve(ApprovalMode::Accept);
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter(venue.clone(), prices())],
        auth.clone(),
        EngineOptions {
            max_run_allocation_usd_micros: "1000000000".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_cost_origin_price_provider(prices());
    let id = "synthetic-ordinary-partial-cancel";
    let (open, close) = round_trip_inputs(id, true, "BUY").await;
    let opening = request(open, true);
    let original = engine.execute(opening.clone()).await.unwrap();
    assert_eq!(original.status, Status::Pending, "{original:?}");
    let identity = original.submission.clone().unwrap();
    assert!(original.effects.is_none());
    assert!(
        engine
            .operations()
            .unwrap()
            .iter()
            .any(|record| record.id == original.id && !record.status.terminal())
    );
    let cancellation = OperationRequest {
        request_id: "synthetic-ordinary-cancel-remainder".into(),
        operation: "binance.futures.order-cancel".into(),
        input: json!({"request_id":"synthetic-ordinary-cancel-remainder","account":ACCOUNT,"symbol":"ETHUSDT","operation_id":original.id,"original_submission":identity}),
        run_id: None,
    };
    let cancelled = engine.execute(cancellation.clone()).await.unwrap();
    assert_eq!(cancelled.status, Status::Completed, "{cancelled:?}");
    assert_eq!(cancelled.effects.as_ref().unwrap().effects.len(), 1);
    assert!(matches!(
        cancelled.effects.as_ref().unwrap().effects[0],
        fin_domain::FinancialEffect::SubmissionCancellation { .. }
    ));
    // The cancellation terminates the remainder; the original order still owns
    // its partial fills and all cash costs until independently reconciled.
    let waiting = engine.status(&original.id, false).await.unwrap();
    assert_eq!(waiting.status, Status::Pending, "{waiting:?}");
    assert!(waiting.effects.is_none());
    let partial_settlement = engine.status(&original.id, true).await.unwrap();
    assert_eq!(
        partial_settlement.status,
        Status::Completed,
        "{partial_settlement:?}"
    );
    assert_eq!(partial_settlement.submission, Some(identity));
    let duplicate_open = engine.execute(opening).await.unwrap();
    let duplicate_cancel = engine.execute(cancellation).await.unwrap();
    assert_eq!(duplicate_open.id, original.id);
    assert_eq!(duplicate_cancel.id, cancelled.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(venue.cancels.load(Ordering::SeqCst), 1);
    let closing = request(close, true);
    let cleanup = engine.execute(closing).await.unwrap();
    assert_eq!(cleanup.status, Status::Completed, "{cleanup:?}");
    {
        let requests = venue.seen.lock().unwrap();
        let orders = requests
            .iter()
            .filter(|request| request.endpoint == E::FuturesPlaceOrder)
            .collect::<Vec<_>>();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[1].get("quantity"), Some("0.01"));
        assert_eq!(orders[1].get("reduceOnly"), Some("true"));
    }
    let state = venue.state.lock().unwrap();
    assert_eq!(state.position, 0);
    assert_eq!(decimal(state.collateral), "59.96");
    assert_eq!(auth.calls.load(Ordering::SeqCst), 3);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
    assert_eq!(venue.cancels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_same_logical_order_requests_share_one_submission_and_one_approval() {
    let venue = Venue::new(vec![Fill::full("ETH", "0.00002")]);
    let auth = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::in_memory().unwrap(),
    );
    let operation = request(input("synthetic-concurrent-order", "BUY", false), false);
    let (first, second) =
        tokio::join!(engine.execute(operation.clone()), engine.execute(operation));
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.status, Status::Completed, "{first:?}");
    assert_eq!(second.status, Status::Completed, "{second:?}");
    assert_eq!(first.id, second.id);
    assert_eq!(first.submission, second.submission);
    assert_eq!(first.effects, second.effects);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn any_other_market_futures_exposure_blocks_new_opening_before_approval() {
    let venue = Venue::new(vec![]);
    venue.other_exposure.store(true, Ordering::SeqCst);
    let auth = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::in_memory().unwrap(),
    );
    let record = engine
        .execute(request(
            input("synthetic-other-market-exposure", "BUY", true),
            true,
        ))
        .await
        .unwrap();
    assert_no_post(&record, &venue);
    assert_eq!(record.error.as_ref().unwrap().code, "EXISTING_EXPOSURE");
    assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
    assert!(
        venue
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.endpoint == E::FuturesPositions
                && request.get("symbol").is_none()),
        "opening must inspect the entire account, not only the requested market"
    );
}

#[tokio::test]
async fn projected_other_market_derivative_interval_blocks_second_simultaneous_opening() {
    use fin_domain::Adapter;
    let venue = Venue::new(vec![]);
    let adapter = adapter(venue.clone(), prices());
    let baseline = adapter.snapshot().await.unwrap();
    let resource = ResourceId(format!("{SPOT}:USD_M"));
    let selected = fin_domain::ResourceAmount {
        resource: resource.clone(),
        amount: fin_domain::Amount {
            asset: priced_asset("USDT"),
            atoms: fin_domain::Quantity::from_decimal("45", 36).unwrap(),
        },
    };
    // The prior IOC opening can fill anywhere from zero through this maximum.
    // Even a zero lower bound must prevent another market from opening exposure.
    let context = fin_domain::ReturnRouteContext {
        baseline,
        projected: fin_domain::ProjectedState {
            cash: vec![fin_domain::ProjectedCash {
                resource: resource.clone(),
                asset: priced_asset("USDT"),
                amount: fin_domain::QuantityInterval {
                    minimum: selected.amount.atoms.clone(),
                    maximum: selected.amount.atoms.clone(),
                },
            }],
            positions: vec![fin_domain::ProjectedPosition {
                resource,
                position: "BTCUSDT:LONG".into(),
                asset: priced_asset("BTC"),
                position_kind: fin_domain::PositionKind::DerivativeLong,
                amount: fin_domain::QuantityInterval {
                    minimum: fin_domain::Quantity::zero(),
                    maximum: fin_domain::Quantity::from_decimal("0.001", 36).unwrap(),
                },
            }],
            authorities: vec![],
        },
        allocations: vec![selected],
        asset_prices: trusted_prices(),
        step_index: 1,
        prior_proofs: vec![],
        remaining_cost_budget_usd_micros: fin_domain::Quantity::new("1000000").unwrap(),
    };
    let error = adapter
        .preflight_return_route(
            "binance.futures.order-place",
            &input("synthetic-projected-second-opening", "BUY", true),
            &context,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "EXISTING_EXPOSURE");
    assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn available_futures_collateral_must_cover_margin_plus_all_fees_and_loss_caps() {
    for (mark, side, limit, available, fee) in [
        ("2000", "BUY", "2000", "40.34", "0.05"),
        ("6000", "SELL", "500", "120.44", "0.15"),
    ] {
        let venue = Venue::new(vec![]);
        venue.state.lock().unwrap().collateral = units(available);
        *venue.mark_price.lock().unwrap() = mark.into();
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let mut data = input("synthetic-combined-margin-reserves", side, true);
        data["limit_price"] = json!(limit);
        data["maximum_fees"] = json!({"USDT":fee});
        let record = engine.execute(request(data, true)).await.unwrap();
        assert_no_post(&record, &venue);
        assert_eq!(
            record.error.as_ref().unwrap().code,
            "INSUFFICIENT_MARGIN",
            "{record:?}"
        );
        assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn ordinary_partial_or_expired_ioc_close_retains_exposure_and_exact_identity() {
    for partial in [true, false] {
        let close = if partial {
            Fill {
                fraction: 500,
                ..Fill::full("USDT", "0.02")
            }
        } else {
            Fill::none()
        };
        let venue = Venue::new(vec![Fill::full("USDT", "0.04"), close]);
        let auth = approve(ApprovalMode::Accept);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic-partial-close.sqlite");
        let engine = Engine::new(
            Store::open(&path).unwrap(),
            vec![adapter(venue.clone(), prices())],
            auth.clone(),
            EngineOptions {
                max_run_allocation_usd_micros: "1000000000".into(),
                ..Default::default()
            },
        )
        .unwrap()
        .with_cost_origin_price_provider(prices());
        let id = "synthetic-unfinished-ioc-close";
        let (open, close) = round_trip_inputs(id, true, "BUY").await;
        let opening = request(open, true);
        let opened = engine.execute(opening).await.unwrap();
        assert_eq!(opened.status, Status::Completed, "{opened:?}");
        let closing = request(close, true);
        let settled = engine.execute(closing.clone()).await.unwrap();
        assert_eq!(settled.status, Status::Completed, "{settled:?}");
        let remaining = if partial { "0.01" } else { "0.02" };
        assert_eq!(decimal(venue.state.lock().unwrap().position), remaining);
        assert!(settled.effects.as_ref().unwrap().effects.iter().any(|effect|matches!(effect,fin_domain::FinancialEffect::PositionChange{anchor:Some(anchor),..} if anchor.principal.atoms.to_decimal(36)==remaining)));
        assert!(Store::open(&path).unwrap().get_run(id).unwrap().is_none());
        let duplicate = engine.execute(closing).await.unwrap();
        assert_eq!(duplicate.id, settled.id);
        assert_eq!(duplicate.effects, settled.effects);
        assert_eq!(venue.posts.load(Ordering::SeqCst), 2);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            decimal(venue.state.lock().unwrap().collateral),
            if partial { "59.94" } else { "59.96" }
        );
    }
}
