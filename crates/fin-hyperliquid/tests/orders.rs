use async_trait::async_trait;
use fin_domain::financial_plans::{
    FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
    FinancialPlanStepSpec, FinancialStepCompletion,
};
use fin_domain::{
    Adapter, ApprovalAudit, AssetPrice, AuthorizationChallenge, FinError, OperationRequest,
    Quantity, Status,
};
use fin_engine::{Engine, EngineOptions};
use fin_exchange::{
    ExchangeAdapter, ExchangeProfile,
    ccxt::{
        HyperliquidAction, HyperliquidActionEnvelope, HyperliquidInfoRequest, HyperliquidOrder,
    },
};
use fin_hyperliquid::{
    DeferredExecutionState, HyperliquidAccount, HyperliquidNonces, HyperliquidPrices,
    HyperliquidProfile, HyperliquidTransport, asset, usdc,
};
use fin_store::{Store, execution_state::ExecutionStateRepository};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

const OWNER: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TX: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

#[derive(Default)]
struct Venue {
    dispatches: AtomicUsize,
    fills: Mutex<Vec<Value>>,
    orders: Mutex<BTreeMap<String, Value>>,
    lose_response: bool,
    delisted: bool,
    rejections: Vec<Option<String>>,
    max_builder_fee: Option<u64>,
    last_action: Mutex<Option<HyperliquidActionEnvelope>>,
    account_mode: Mutex<String>,
    spot_total: Option<String>,
    spot_hold: Option<String>,
    available_to_trade: Option<String>,
    max_trade_size: Option<String>,
    leverage: Option<u64>,
    positions: Mutex<Option<Vec<Value>>>,
}

#[async_trait]
impl HyperliquidTransport for Venue {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value, FinError> {
        Ok(match request {
            HyperliquidInfoRequest::MetaAndAssetCtxs => json!([
                {"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":50,"isDelisted":self.delisted}]},
                [{"markPx":"10","midPx":"10","oraclePx":"10"}]
            ]),
            HyperliquidInfoRequest::SpotMetaAndAssetCtxs => json!([
                {"tokens":[
                    {"name":"ETH","index":1,"weiDecimals":18,"szDecimals":4,"tokenId":"0xeeee"},
                    {"name":"USDC","index":0,"weiDecimals":8,"szDecimals":8,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054"}
                ],"universe":[{"name":"ETH/USDC","index":0,"tokens":[1,0]}]},
                [{"midPx":"10"}]
            ]),
            HyperliquidInfoRequest::ClearinghouseState { .. } => {
                json!({"assetPositions":self.positions.lock().unwrap().clone().unwrap_or_default(),"withdrawable":"1000"})
            }
            HyperliquidInfoRequest::SpotClearinghouseState { .. } => json!({"balances":[
                {"coin":"USDC","token":0,"total":self.spot_total.as_deref().unwrap_or("1000"),"hold":self.spot_hold.as_deref().unwrap_or("0")},
                {"coin":"ETH","token":1,"total":"1000","hold":"0"}
            ]}),
            HyperliquidInfoRequest::PerpDexs => json!([null]),
            HyperliquidInfoRequest::FrontendOpenOrders { .. } => json!([]),
            HyperliquidInfoRequest::ActiveAssetData { owner, coin } => json!({
                "user":owner,"coin":coin,
                "leverage":{"type":"cross","value":self.leverage.unwrap_or(10)},
                "availableToTrade":[self.available_to_trade.as_deref().unwrap_or("1000"),self.available_to_trade.as_deref().unwrap_or("1000")],
                "maxTradeSzs":[self.max_trade_size.as_deref().unwrap_or("100"),self.max_trade_size.as_deref().unwrap_or("100")],
                "markPx":"10"
            }),
            HyperliquidInfoRequest::UserRole { .. } => json!({"role":"user"}),
            HyperliquidInfoRequest::UserAbstractionState { .. } => {
                let mode = self.account_mode.lock().unwrap();
                json!(if mode.is_empty() {
                    "disabled"
                } else {
                    mode.as_str()
                })
            }
            HyperliquidInfoRequest::UserDexAbstractionState { .. } => json!(false),
            HyperliquidInfoRequest::UserFillsByTime { .. } => {
                self.fills.lock().unwrap().clone().into()
            }
            HyperliquidInfoRequest::MaxBuilderFee { .. } => {
                json!(self.max_builder_fee.unwrap_or_default())
            }
            HyperliquidInfoRequest::OrderStatusByClientId {
                client_order_id, ..
            } => self
                .orders
                .lock()
                .unwrap()
                .get(&client_order_id)
                .cloned()
                .unwrap_or_else(|| json!({"status":"unknownOid"})),
            other => panic!("unexpected synthetic info request: {other:?}"),
        })
    }

    async fn dispatch(
        &self,
        _account: &HyperliquidAccount,
        envelope: &HyperliquidActionEnvelope,
        _journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<Value, FinError> {
        let dispatch_index = self.dispatches.fetch_add(1, Ordering::SeqCst);
        *self.last_action.lock().unwrap() = Some(envelope.clone());
        let HyperliquidAction::Order { orders, .. } = &envelope.action else {
            panic!("order fixture dispatched a non-order action")
        };
        let mut statuses = Vec::new();
        for (n, order) in orders.iter().enumerate() {
            if let Some(error) = self.rejections.get(n).and_then(Option::as_ref) {
                statuses.push(json!({"error":error}));
                continue;
            }
            self.orders.lock().unwrap().insert(
                order.client_order_id.clone().unwrap(),
                order_status(
                    order,
                    100 + dispatch_index as u64 * 100 + n as u64,
                    "open",
                    &order.size,
                ),
            );
            statuses.push(json!({"resting":{"oid":100+dispatch_index as u64 * 100+n as u64}}));
        }
        if self.lose_response {
            return Err(FinError::new(
                "SYNTHETIC_TIMEOUT",
                "submission",
                "response lost after dispatch",
                "reconcile original request",
            ));
        }
        Ok(json!({"status":"ok","response":{"type":"order","data":{"statuses":statuses}}}))
    }
}

fn order_status(order: &HyperliquidOrder, oid: u64, status: &str, remaining: &str) -> Value {
    json!({"status":"order","order":{"status":status,"order":{
        "coin":if order.asset == 0 {"BTC"} else {"@0"},
        "side":if order.is_buy {"B"} else {"A"},
        "cloid":order.client_order_id,
        "origSz":order.size,
        "sz":remaining,
        "oid":oid
    }}})
}

#[derive(Default)]
struct Prices;

#[async_trait]
impl HyperliquidPrices for Prices {
    async fn usdc_price(&self) -> Result<AssetPrice, FinError> {
        let at = fin_domain::now();
        Ok(AssetPrice {
            asset: usdc(),
            usd_micros_per_unit: Quantity::from_decimal("1000000", 0).unwrap(),
            unit_atoms: Quantity::from_decimal("1", 36).unwrap(),
            observed_at: at,
            source: "synthetic-usdc-usd".into(),
        })
    }
}

struct MutablePrices(Arc<AtomicU64>);

#[async_trait]
impl HyperliquidPrices for MutablePrices {
    async fn usdc_price(&self) -> Result<AssetPrice, FinError> {
        Ok(AssetPrice {
            asset: usdc(),
            usd_micros_per_unit: Quantity::from_u64(self.0.load(Ordering::SeqCst)),
            unit_atoms: Quantity::from_decimal("1", 36).unwrap(),
            observed_at: fin_domain::now(),
            source: "synthetic-usdc-usd".into(),
        })
    }
}

struct Approval;

#[async_trait]
impl fin_domain::Authorizer for Approval {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit, FinError> {
        let approved_at = fin_domain::now();
        Ok(ApprovalAudit {
            mechanism: "synthetic-test-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at,
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct MutatingApproval {
    price: Arc<AtomicU64>,
    after_approval_price: u64,
    venue: Arc<Venue>,
    positions_after_approval: Option<Vec<Value>>,
}

#[async_trait]
impl fin_domain::Authorizer for MutatingApproval {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit, FinError> {
        self.price
            .store(self.after_approval_price, Ordering::SeqCst);
        if let Some(positions) = &self.positions_after_approval {
            *self.venue.positions.lock().unwrap() = Some(positions.clone());
        }
        let approved_at = fin_domain::now();
        Ok(ApprovalAudit {
            mechanism: "synthetic-test-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at,
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn fill(oid: u64, tid: u64, coin: &str, side: &str, size: &str, price: &str) -> Value {
    fill_economics(oid, tid, coin, side, size, price, ("0.01", "0"))
}

fn fill_economics(
    oid: u64,
    tid: u64,
    coin: &str,
    side: &str,
    size: &str,
    price: &str,
    economics: (&str, &str),
) -> Value {
    let (fee, closed_pnl) = economics;
    json!({"oid":oid,"tid":tid,"hash":TX,"coin":coin,"side":side,
        "time":chrono::Utc::now().timestamp_millis() as u64,
        "sz":size,"px":price,"feeToken":"USDC","fee":fee,"closedPnl":closed_pnl})
}

fn input(request_id: &str, market_type: &str, symbol: &str, size: &str, price: &str) -> Value {
    json!({"request_id":request_id,"account":"main","symbol":symbol,
        "market_type":market_type,"side":"buy","order_type":"limit","size":size,
        "limit_price":price,"min_price":price,"time_in_force":"gtc","max_notional_usdc":"20",
        "max_fee_usdc":"1","max_realized_loss_usdc":"0"})
}

async fn engine(path: &std::path::Path, venue: Arc<Venue>) -> Engine {
    engine_with(path, venue, Arc::new(Prices), Arc::new(Approval)).await
}

async fn engine_with(
    path: &std::path::Path,
    venue: Arc<Venue>,
    prices: Arc<dyn HyperliquidPrices>,
    approval: Arc<dyn fin_domain::Authorizer>,
) -> Engine {
    let store = Store::open(path).unwrap();
    let shared_store = store.share();
    let nonce_state = Arc::new(DeferredExecutionState::default());
    nonce_state
        .attach(Arc::new(ExecutionStateRepository(shared_store.clone())))
        .unwrap();
    let profile = HyperliquidProfile::new(
        vec![HyperliquidAccount {
            name: "main".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-ref".into()),
            signing_enabled: true,
            execution_scope: Default::default(),
        }],
        venue,
        prices,
        nonce_state,
    )
    .unwrap();
    let adapter: Arc<dyn Adapter> = Arc::new(ExchangeAdapter::new(profile));
    Engine::new_with_shared_store(
        shared_store,
        vec![adapter],
        approval,
        EngineOptions::default(),
    )
    .unwrap()
}

fn request(
    request_id: &str,
    market_type: &str,
    symbol: &str,
    size: &str,
    price: &str,
) -> OperationRequest {
    OperationRequest {
        request_id: request_id.into(),
        operation: "hyperliquid.orders.place".into(),
        input: input(request_id, market_type, symbol, size, price),
        run_id: None,
    }
}

#[tokio::test]
async fn resting_partial_order_survives_restart_and_settles_only_new_fill_deltas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("orders.sqlite");
    let venue = Arc::new(Venue::default());
    let first_engine = engine(&path, venue.clone()).await;
    let request = request("progressive-order", "perpetual", "BTC/USDC:USDC", "1", "10");
    let submitted = first_engine.execute(request.clone()).await.unwrap();
    assert_eq!(submitted.status, Status::Pending, "{submitted:#?}");
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);

    let status = venue.orders.lock().unwrap().values().next().unwrap()["order"]["order"]["oid"]
        .as_u64()
        .unwrap();
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(status, 1, "BTC", "B", "0.4", "10"));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0.6");
    });
    let partial = first_engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(partial.status, Status::Pending);
    let partial_sequence =
        partial.effects.as_ref().unwrap().evidence["settlement_progress"]["sequence"]
            .as_u64()
            .unwrap();
    assert_eq!(
        partial.effects.as_ref().unwrap().evidence["settlement_progress"]["event_digests"]
            .as_object()
            .unwrap()
            .len(),
        1
    );

    // The next bounded venue response is at its native 2,000-row cap and no
    // longer contains the earlier matching receipt. Durable receipts from the
    // previous verified snapshot must still complete the order history.
    *venue.fills.lock().unwrap() = (0..1999)
        .map(|index| fill(10_000 + index, index, "BTC", "B", "0.01", "10"))
        .collect();

    drop(first_engine);
    let resumed = engine(&path, venue.clone()).await;
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(status, 2, "BTC", "B", "0.6", "10"));
    venue
        .orders
        .lock()
        .unwrap()
        .values_mut()
        .for_each(|row| row["order"]["order"]["sz"] = json!("0"));
    venue
        .orders
        .lock()
        .unwrap()
        .values_mut()
        .for_each(|row| row["order"]["status"] = json!("filled"));
    let terminal = resumed.status(&submitted.id, true).await.unwrap();
    assert_eq!(terminal.status, Status::Completed);
    assert_eq!(
        venue.dispatches.load(Ordering::SeqCst),
        1,
        "recovery must never resubmit the write"
    );
    let final_progress = &terminal.effects.as_ref().unwrap().evidence["settlement_progress"];
    assert!(final_progress["sequence"].as_u64().unwrap() > partial_sequence);
    assert_eq!(
        final_progress["event_digests"].as_object().unwrap().len(),
        2
    );
    let long_delta = terminal
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .find_map(|effect| match effect {
            fin_domain::FinancialEffect::PositionChange {
                position, delta, ..
            } if position.ends_with(":long") => Some(delta.magnitude().clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(long_delta, Quantity::from_decimal("1", 36).unwrap());
    let pnl_legs: Vec<_> = terminal
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .filter_map(|effect| match effect {
            fin_domain::FinancialEffect::Settlement { delta, .. } => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(pnl_legs.len(), 2);
    assert!(pnl_legs.iter().all(|delta| delta.is_zero()));
}

#[tokio::test]
async fn partial_loss_then_profit_and_fee_then_rebate_keep_gross_effects_monotone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gross-progress.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let mut request = request("gross-progress", "perpetual", "BTC/USDC:USDC", "1", "10");
    request.input["max_realized_loss_usdc"] = json!("1");
    let submitted = engine.execute(request).await.unwrap();
    let oid = venue.orders.lock().unwrap().values().next().unwrap()["order"]["order"]["oid"]
        .as_u64()
        .unwrap();

    venue.fills.lock().unwrap().push(fill_economics(
        oid,
        11,
        "BTC",
        "B",
        "0.4",
        "10",
        ("0.01", "-0.2"),
    ));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0.6");
    });
    let partial = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(partial.status, Status::Pending);
    let partial_effects = &partial.effects.as_ref().unwrap().effects;
    assert!(partial_effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Settlement { delta, .. } if delta.as_str() == "-200000000000000000000000000000000000"
    )));
    assert!(partial_effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Cost { category, amount, .. }
            if category == "realized_loss" && amount.atoms.to_decimal(36) == "0.2"
    )));

    venue.fills.lock().unwrap().push(fill_economics(
        oid,
        12,
        "BTC",
        "B",
        "0.6",
        "10",
        ("-0.005", "0.1"),
    ));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0");
        row["order"]["status"] = json!("filled");
    });
    let terminal = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(terminal.status, Status::Completed);
    let effects = &terminal.effects.as_ref().unwrap().effects;
    assert!(effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Settlement { delta, .. } if delta.as_str() == "-200000000000000000000000000000000000"
    )));
    assert!(effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Settlement { delta, .. } if delta.as_str() == "100000000000000000000000000000000000"
    )));
    assert!(effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Cost { category, amount, .. }
            if category == "realized_loss" && amount.atoms.to_decimal(36) == "0.2"
    )));
    assert!(effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Cost { category, amount, .. }
            if category == "trading_fee_paid" && amount.atoms.to_decimal(36) == "0.01"
    )));
    assert!(effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::UsableCredit { amount, .. }
            if amount.asset == usdc() && amount.atoms.to_decimal(36) == "0.005"
    )));
}

#[tokio::test]
async fn opposing_same_market_batch_fills_have_sequential_position_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opposing-batch.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let mut buy = input("opposing-buy", "perpetual", "BTC/USDC:USDC", "1", "10");
    buy["side"] = json!("buy");
    let mut sell = input("opposing-sell", "perpetual", "BTC/USDC:USDC", "1", "10");
    sell["side"] = json!("sell");
    let request = OperationRequest {
        request_id: "opposing-batch".into(),
        operation: "hyperliquid.orders.batch".into(),
        input: json!({"request_id":"opposing-batch","account":"main","orders":[buy,sell]}),
        run_id: None,
    };
    let submitted = engine.execute(request).await.unwrap();
    assert_eq!(submitted.status, Status::Pending);
    let ids = venue
        .orders
        .lock()
        .unwrap()
        .values()
        .map(|row| {
            (
                row["order"]["order"]["side"].as_str().unwrap().to_owned(),
                row["order"]["order"]["oid"].as_u64().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    venue.fills.lock().unwrap().extend([
        fill(ids["B"], 21, "BTC", "B", "1", "10"),
        fill(ids["A"], 22, "BTC", "A", "1", "10"),
    ]);
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0");
        row["order"]["status"] = json!("filled");
    });
    let terminal = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(terminal.status, Status::Completed);
    let anchors = terminal
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .filter_map(|effect| match effect {
            fin_domain::FinancialEffect::PositionChange {
                position,
                delta,
                anchor: Some(anchor),
                ..
            } => Some((
                position.clone(),
                delta.as_str().to_owned(),
                anchor.evidence["start_position"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                anchor.evidence["position_atoms"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(anchors.len(), 2);
    assert_eq!(
        anchors[0],
        (
            "BTC/USDC:USDC:long".into(),
            "0".into(),
            "0".into(),
            "0".into()
        )
    );
    assert_eq!(
        anchors[1],
        (
            "BTC/USDC:USDC:short".into(),
            "0".into(),
            "0".into(),
            "0".into()
        )
    );
    let long_anchor = terminal
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .find_map(|effect| match effect {
            fin_domain::FinancialEffect::PositionChange {
                position,
                anchor: Some(anchor),
                ..
            } if position.ends_with(":long") => Some(anchor),
            _ => None,
        })
        .unwrap();
    assert_eq!(long_anchor.evidence["orders"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn delisted_out_of_band_and_misconfigured_trigger_orders_never_dispatch() {
    let cases = ["delisted", "out_of_band", "stray_trigger"];
    for case in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("invalid-{case}.sqlite"));
        let venue = Arc::new(Venue {
            delisted: case == "delisted",
            ..Venue::default()
        });
        let engine = engine(&path, venue.clone()).await;
        let mut request = request(
            &format!("invalid-{case}"),
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        );
        match case {
            "out_of_band" => {
                request.input["min_price"] = json!("11");
                request.input["max_price"] = json!("12");
            }
            "stray_trigger" => request.input["trigger_price"] = json!("9"),
            _ => {}
        }
        let result = engine.execute(request).await.unwrap();
        assert_eq!(result.status, Status::Failed, "{case} must fail closed");
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0, "{case}");
    }
}

#[tokio::test]
async fn native_balance_and_margin_capacity_guards_block_without_dispatch() {
    let cases = [
        (
            "spot-held-balance",
            "spot",
            "ETH/USDC",
            Some("1000"),
            Some("995"),
            None,
            None,
            "1",
        ),
        (
            "perp-one-x-capacity",
            "perpetual",
            "BTC/USDC:USDC",
            None,
            None,
            Some("100"),
            None,
            "1",
        ),
        (
            "perp-size-capacity",
            "perpetual",
            "BTC/USDC:USDC",
            None,
            None,
            Some("1000"),
            Some("0.5"),
            "1",
        ),
        (
            "native-minimum",
            "perpetual",
            "BTC/USDC:USDC",
            None,
            None,
            Some("1000"),
            Some("100"),
            "0.5",
        ),
    ];
    for (id, market_type, symbol, spot_total, spot_hold, available, max_size, size) in cases {
        let dir = tempfile::tempdir().unwrap();
        let venue = Arc::new(Venue {
            spot_total: spot_total.map(str::to_owned),
            spot_hold: spot_hold.map(str::to_owned),
            available_to_trade: available.map(str::to_owned),
            max_trade_size: max_size.map(str::to_owned),
            ..Venue::default()
        });
        let engine = engine(&dir.path().join(format!("{id}.sqlite")), venue.clone()).await;
        let result = engine
            .execute(request(id, market_type, symbol, size, "10"))
            .await
            .unwrap();
        assert_eq!(result.status, Status::Failed, "{id}");
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0, "{id}");
    }
}

#[tokio::test]
async fn fresh_capacity_revalidation_keeps_bindings_stable_when_still_sufficient() {
    let profile = |venue: Arc<Venue>| {
        HyperliquidProfile::new(
            vec![HyperliquidAccount {
                name: "main".into(),
                owner: OWNER.into(),
                credential_ref: Some("synthetic-ref".into()),
                signing_enabled: true,
                execution_scope: Default::default(),
            }],
            venue,
            Arc::new(Prices),
            Arc::new(DeferredExecutionState::default()),
        )
        .unwrap()
    };

    let spot_input = input("capacity-stable-spot", "spot", "ETH/USDC", "1", "10");
    let approved_spot = profile(Arc::new(Venue::default()))
        .prepare("hyperliquid.orders.place", &spot_input)
        .await
        .unwrap();
    // Native free balance falls substantially but remains above the frozen
    // amount-plus-fee requirement. Only volatile readings change.
    let fresh_spot = profile(Arc::new(Venue {
        spot_total: Some("90".into()),
        spot_hold: Some("30".into()),
        ..Venue::default()
    }))
    .prepare("hyperliquid.orders.place", &spot_input)
    .await
    .unwrap();
    assert_eq!(approved_spot.binding, fresh_spot.binding);

    let perp_input = input(
        "capacity-stable-perp",
        "perpetual",
        "BTC/USDC:USDC",
        "1",
        "10",
    );
    let approved_perp = profile(Arc::new(Venue::default()))
        .prepare("hyperliquid.orders.place", &perp_input)
        .await
        .unwrap();
    let fresh_perp = profile(Arc::new(Venue {
        available_to_trade: Some("500".into()),
        max_trade_size: Some("50".into()),
        ..Venue::default()
    }))
    .prepare("hyperliquid.orders.place", &perp_input)
    .await
    .unwrap();
    assert_eq!(approved_perp.binding, fresh_perp.binding);

    let shrunken = profile(Arc::new(Venue {
        available_to_trade: Some("10".into()),
        ..Venue::default()
    }));
    assert!(
        shrunken
            .prepare("hyperliquid.orders.place", &perp_input)
            .await
            .is_err(),
        "a fresh capacity below the frozen requirement must block dispatch"
    );

    let changed_leverage = profile(Arc::new(Venue {
        leverage: Some(5),
        ..Venue::default()
    }))
    .prepare("hyperliquid.orders.place", &perp_input)
    .await
    .unwrap();
    assert_ne!(approved_perp.binding, changed_leverage.binding);
}

#[tokio::test]
async fn lower_fresh_usd_quote_keeps_approved_ceiling_and_order_binding() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue::default());
    let price = Arc::new(AtomicU64::new(1_000_000));
    let approval: Arc<dyn fin_domain::Authorizer> = Arc::new(MutatingApproval {
        price: price.clone(),
        after_approval_price: 995_000,
        venue: venue.clone(),
        positions_after_approval: None,
    });
    let engine = engine_with(
        &dir.path().join("lower-quote.sqlite"),
        venue.clone(),
        Arc::new(MutablePrices(price)),
        approval,
    )
    .await;
    let result = engine
        .execute(request(
            "lower-quote",
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Pending, "{result:#?}");
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(
        result
            .preview
            .as_ref()
            .unwrap()
            .valuation
            .as_ref()
            .unwrap()
            .movement_usd_micros
            .as_str(),
        "20000000",
        "revalidation must retain the original cent-rounded approved ceiling"
    );
}

#[tokio::test]
async fn higher_fresh_usd_quote_fails_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue::default());
    let price = Arc::new(AtomicU64::new(1_000_000));
    let approval: Arc<dyn fin_domain::Authorizer> = Arc::new(MutatingApproval {
        price: price.clone(),
        after_approval_price: 1_010_000,
        venue: venue.clone(),
        positions_after_approval: None,
    });
    let engine = engine_with(
        &dir.path().join("higher-quote.sqlite"),
        venue.clone(),
        Arc::new(MutablePrices(price)),
        approval,
    )
    .await;
    let result = engine
        .execute(request(
            "higher-quote",
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed, "{result:#?}");
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0);
    assert!(result.submission.is_none());
}

#[tokio::test]
async fn usd_ceiling_does_not_allow_position_anchor_changes() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue::default());
    let price = Arc::new(AtomicU64::new(1_000_000));
    let approval: Arc<dyn fin_domain::Authorizer> = Arc::new(MutatingApproval {
        price: price.clone(),
        after_approval_price: 995_000,
        venue: venue.clone(),
        positions_after_approval: Some(vec![json!({"position":{
            "coin":"BTC","szi":"1","marginUsed":"2",
            "leverage":{"type":"isolated","value":10}
        }})]),
    });
    let engine = engine_with(
        &dir.path().join("changed-position.sqlite"),
        venue.clone(),
        Arc::new(MutablePrices(price)),
        approval,
    )
    .await;
    let result = engine
        .execute(request(
            "changed-position",
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed, "{result:#?}");
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0);
    assert!(result.submission.is_none());
}

#[tokio::test]
async fn newly_approved_ceiling_rounds_up_to_the_next_cent() {
    let venue = Arc::new(Venue::default());
    let price = Arc::new(AtomicU64::new(1_000_001));
    let profile = HyperliquidProfile::new(
        vec![HyperliquidAccount {
            name: "main".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-ref".into()),
            signing_enabled: true,
            execution_scope: Default::default(),
        }],
        venue,
        Arc::new(MutablePrices(price)),
        Arc::new(DeferredExecutionState::default()),
    )
    .unwrap();
    let prepared = profile
        .prepare(
            "hyperliquid.orders.place",
            &input("cent-ceiling", "perpetual", "BTC/USDC:USDC", "1", "10"),
        )
        .await
        .unwrap();
    assert_eq!(
        prepared
            .valuation
            .as_ref()
            .unwrap()
            .movement_usd_micros
            .as_str(),
        "20010000",
        "the $20.00002 movement must be approved at the next cent, $20.01"
    );
}

#[tokio::test]
async fn legacy_approval_without_ceiling_metadata_is_not_upgraded_on_refresh() {
    let venue = Arc::new(Venue::default());
    let price = Arc::new(AtomicU64::new(1_000_000));
    let profile = HyperliquidProfile::new(
        vec![HyperliquidAccount {
            name: "main".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-ref".into()),
            signing_enabled: true,
            execution_scope: Default::default(),
        }],
        venue,
        Arc::new(MutablePrices(price.clone())),
        Arc::new(DeferredExecutionState::default()),
    )
    .unwrap();

    // This unbound preparation models a pre-ceiling persisted approval.
    let legacy = profile
        .prepare_action(
            "hyperliquid.orders.place",
            &input("legacy-ceiling", "perpetual", "BTC/USDC:USDC", "1", "10"),
        )
        .await
        .unwrap();
    assert!(legacy.binding.get(fin_domain::value_bounds::KEY).is_none());

    // A lower live quote must not cause the refresh path to silently add a
    // ceiling that the original approval never displayed.
    price.store(995_000, Ordering::SeqCst);
    let refreshed = profile.refresh_prepared(&legacy).await.unwrap();
    assert!(
        refreshed
            .binding
            .get(fin_domain::value_bounds::KEY)
            .is_none(),
        "legacy approval must retain its original binding semantics"
    );
    assert_eq!(legacy.binding, refreshed.binding);
    assert_ne!(
        legacy.valuation.as_ref().unwrap().movement_usd_micros,
        refreshed.valuation.as_ref().unwrap().movement_usd_micros,
        "legacy refresh uses current evidence without freezing a new ceiling"
    );
    assert!(refreshed.valuation.is_some());
}

#[tokio::test]
async fn unified_spot_order_fails_closed_when_perpetual_collateral_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue {
        positions: Mutex::new(Some(vec![json!({"position":{
            "coin":"BTC","szi":"1","marginUsed":"2",
            "leverage":{"type":"isolated","value":10}
        }})])),
        ..Venue::default()
    });
    *venue.account_mode.lock().unwrap() = "unifiedAccount".into();
    let engine = engine(&dir.path().join("unified-collateral.sqlite"), venue.clone()).await;
    let result = engine
        .execute(request(
            "unified-spot-with-open-position",
            "spot",
            "ETH/USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn builder_fee_requires_approval_market_cap_and_explicit_reserve() {
    const BUILDER: &str = "0x2222222222222222222222222222222222222222";

    // An approved request is encoded into the single native order action.
    let dir = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue {
        max_builder_fee: Some(75),
        ..Venue::default()
    });
    let approved_engine = engine(&dir.path().join("builder-approved.sqlite"), venue.clone()).await;
    let mut req = request("builder-approved", "perpetual", "BTC/USDC:USDC", "1", "10");
    req.input["builder_fee"] = json!({"address":BUILDER,"fee_tenths_bps":75});
    let result = approved_engine.execute(req).await.unwrap();
    assert_eq!(result.status, Status::Pending);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    let envelope = venue.last_action.lock().unwrap().clone().unwrap();
    assert!(matches!(envelope.action,
        HyperliquidAction::Order { builder_fee: Some(ref fee), .. }
            if fee.address == BUILDER && fee.fee_tenths_bps == 75
    ));

    // Missing owner approval, a rate above the native perps maximum, and a
    // fee reserve below the exact notional-derived minimum all fail closed.
    let cases = [
        ("builder-unapproved", None, 75, "1"),
        ("builder-over-cap", Some(101), 101, "1"),
        ("builder-under-reserved", Some(75), 75, "0.0149"),
    ];
    for (id, approved, rate, max_fee) in cases {
        let dir = tempfile::tempdir().unwrap();
        let venue = Arc::new(Venue {
            max_builder_fee: approved,
            ..Venue::default()
        });
        let engine = engine(&dir.path().join(format!("{id}.sqlite")), venue.clone()).await;
        let mut req = request(id, "perpetual", "BTC/USDC:USDC", "1", "10");
        req.input["builder_fee"] = json!({"address":BUILDER,"fee_tenths_bps":rate});
        req.input["max_fee_usdc"] = json!(max_fee);
        let result = engine.execute(req).await.unwrap();
        assert_eq!(result.status, Status::Failed, "{id}");
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0, "{id}");
    }
}

#[tokio::test]
async fn native_batch_rejects_mixed_builder_fees_before_dispatch() {
    const BUILDER: &str = "0x2222222222222222222222222222222222222222";
    let dir = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue {
        max_builder_fee: Some(75),
        ..Venue::default()
    });
    let engine = engine(&dir.path().join("mixed-builder.sqlite"), venue.clone()).await;
    let mut first = input("mixed-builder-a", "perpetual", "BTC/USDC:USDC", "1", "10");
    first["builder_fee"] = json!({"address":BUILDER,"fee_tenths_bps":75});
    let second = input("mixed-builder-b", "perpetual", "BTC/USDC:USDC", "1", "10");
    let req = OperationRequest {
        request_id: "mixed-builder".into(),
        operation: "hyperliquid.orders.batch".into(),
        input: json!({"request_id":"mixed-builder","account":"main","orders":[first,second]}),
        run_id: None,
    };
    let result = engine.execute(req).await.unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exact_order_status_errors_prove_ioc_and_balance_rejections() {
    for (request_id, error) in [
        (
            "ioc-no-fill",
            "Order could not be matched at the requested price",
        ),
        ("insufficient-balance", "Insufficient margin"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{request_id}.sqlite"));
        let venue = Arc::new(Venue {
            rejections: vec![Some(error.into())],
            ..Venue::default()
        });
        let engine = engine(&path, venue.clone()).await;
        let mut request = request(request_id, "perpetual", "BTC/USDC:USDC", "1", "10");
        request.input["time_in_force"] = json!("ioc");
        let result = engine.execute(request).await.unwrap();
        assert_eq!(result.status, Status::Failed);
        assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(result.effects.as_ref().unwrap().effects.len(), 0);
        assert_eq!(
            result.effects.as_ref().unwrap().evidence["dispatch_rejections"][0]["error"],
            error
        );
    }
}

#[tokio::test]
async fn mixed_batch_rejection_preserves_accepted_fill_effects_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed-rejection.sqlite");
    let venue = Arc::new(Venue {
        rejections: vec![Some("Insufficient margin".into()), None],
        ..Venue::default()
    });
    let engine = engine(&path, venue.clone()).await;
    let mut rejected = input("mixed-rejected", "perpetual", "BTC/USDC:USDC", "1", "10");
    rejected["side"] = json!("buy");
    let mut accepted = input("mixed-accepted", "perpetual", "BTC/USDC:USDC", "1", "10");
    accepted["side"] = json!("sell");
    let request = OperationRequest {
        request_id: "mixed-rejection".into(),
        operation: "hyperliquid.orders.batch".into(),
        input: json!({"request_id":"mixed-rejection","account":"main","orders":[rejected,accepted]}),
        run_id: None,
    };
    let submitted = engine.execute(request).await.unwrap();
    assert_eq!(submitted.status, Status::Pending);
    let oid = venue.orders.lock().unwrap().values().next().unwrap()["order"]["order"]["oid"]
        .as_u64()
        .unwrap();
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(oid, 31, "BTC", "A", "1", "10"));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0");
        row["order"]["status"] = json!("filled");
    });
    let terminal = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(terminal.status, Status::Failed);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    let proof = terminal.effects.as_ref().unwrap();
    assert_eq!(proof.evidence["partial_outcome"]["kind"], "mixed_batch");
    assert_eq!(
        proof.evidence["dispatch_rejections"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::PositionChange { position, delta, .. }
            if position.ends_with(":short") && delta.as_str() == "1000000000000000000000000000000000000"
    )));
    assert!(proof.effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Cost { category, amount, .. }
            if category == "trading_fee_paid" && amount.atoms.to_decimal(36) == "0.01"
    )));
    let recovered = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Failed);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn logical_retry_reconciles_pending_order_without_another_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("duplicate.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let request = request("one-logical-write", "perpetual", "BTC/USDC:USDC", "1", "10");
    let first = engine.execute(request.clone()).await.unwrap();
    let retry = engine.execute(request).await.unwrap();
    assert_eq!(first.id, retry.id);
    assert_eq!(retry.status, Status::Pending);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_submit_response_recovers_by_cloid_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lost-response.sqlite");
    let venue = Arc::new(Venue {
        lose_response: true,
        ..Venue::default()
    });
    let engine = engine(&path, venue.clone()).await;
    let first = engine
        .execute(request(
            "lost-response",
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    assert_eq!(first.status, Status::Unknown);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
    let reconciled = engine.status(&first.id, true).await.unwrap();
    assert_eq!(reconciled.status, Status::Pending, "{reconciled:#?}");
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn spot_full_fill_is_a_conversion_and_invalid_precision_never_dispatches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spot.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;

    let invalid = request("bad-precision", "spot", "ETH/USDC", "0.00001", "10");
    assert!(
        engine
            .preview(&invalid.operation, &invalid.input)
            .await
            .is_err()
    );
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0);

    let good = request("spot-full", "spot", "ETH/USDC", "1.25", "10");
    let submitted = engine.execute(good).await.unwrap();
    let (oid, coin) = {
        let orders = venue.orders.lock().unwrap();
        let native = orders.values().next().unwrap();
        (
            native["order"]["order"]["oid"].as_u64().unwrap(),
            native["order"]["order"]["coin"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    };
    let mut spot_fill = fill(oid, 19, &coin, "B", "1.25", "10");
    spot_fill["feeToken"] = json!("ETH");
    spot_fill["fee"] = json!("0.001");
    venue.fills.lock().unwrap().push(spot_fill);
    {
        let mut orders = venue.orders.lock().unwrap();
        let row = orders.values_mut().next().unwrap();
        row["order"]["order"]["sz"] = json!("0");
        row["order"]["status"] = json!("filled");
    }
    let final_record = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(final_record.status, Status::Completed);
    assert!(
        final_record
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| {
                matches!(effect, fin_domain::FinancialEffect::Conversion { spent, received, .. }
            if spent.asset == usdc() && received.asset == asset("0xeeee"))
            })
    );
    assert!(final_record.effects.as_ref().unwrap().effects.iter().any(|effect| {
        matches!(effect, fin_domain::FinancialEffect::Cost { amount, treatment, category, .. }
            if amount.asset == asset("0xeeee")
                && amount.atoms.to_decimal(36) == "0.001"
                && *treatment == fin_domain::CostTreatment::DeductedFromCredit
                && category == "trading_fee_paid")
    }));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn account_mode_is_frozen_before_dispatch_and_unified_cash_keeps_positions_separate() {
    let dir = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue::default());

    // Planning under Separate and switching to Unified before the guarded
    // dispatch check must fail without a venue action.
    let profile = HyperliquidProfile::new(
        vec![HyperliquidAccount {
            name: "main".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-ref".into()),
            signing_enabled: true,
            execution_scope: Default::default(),
        }],
        venue.clone(),
        Arc::new(Prices),
        Arc::new(DeferredExecutionState::default()),
    )
    .unwrap();
    let prepared = profile
        .prepare(
            "hyperliquid.orders.place",
            &input("mode-race", "spot", "ETH/USDC", "1", "10"),
        )
        .await
        .unwrap();
    *venue.account_mode.lock().unwrap() = "unifiedAccount".into();
    assert!(profile.plan_submission(&prepared).await.is_err());
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 0);

    // A fresh Unified preparation reserves and settles spot/perpetual cash in
    // the one unified wallet while derivative positions retain their existing
    // perpetual identity.
    let venue = Arc::new(Venue::default());
    *venue.account_mode.lock().unwrap() = "unifiedAccount".into();
    let engine = engine(&dir.path().join("unified.sqlite"), venue.clone()).await;
    let spot_input = input("unified-spot", "spot", "ETH/USDC", "1", "10");
    let spot_preview = engine
        .preview("hyperliquid.orders.place", &spot_input)
        .await
        .unwrap();
    let unified_resource =
        "hyperliquid:mainnet:0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:unified:primary";
    assert!(
        spot_preview
            .intent
            .debits
            .iter()
            .all(|debit| debit.resource.0 == unified_resource)
    );
    let spot = engine
        .execute(request("unified-spot", "spot", "ETH/USDC", "1", "10"))
        .await
        .unwrap();
    let (spot_oid, spot_coin) = {
        let row = venue
            .orders
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        (
            row["order"]["order"]["oid"].as_u64().unwrap(),
            row["order"]["order"]["coin"].as_str().unwrap().to_owned(),
        )
    };
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(spot_oid, 71, &spot_coin, "B", "1", "10"));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0");
        row["order"]["status"] = json!("filled");
    });
    let spot_done = engine.status(&spot.id, true).await.unwrap();
    assert_eq!(spot_done.status, Status::Completed);
    assert!(
        spot_done
            .effects
            .as_ref()
            .unwrap()
            .effects
            .iter()
            .any(|effect| matches!(effect,
                fin_domain::FinancialEffect::Conversion { resource, .. }
                    if resource.0 == unified_resource
            ))
    );

    let perp_input = input("unified-perp", "perpetual", "BTC/USDC:USDC", "1", "10");
    let perp_preview = engine
        .preview("hyperliquid.orders.place", &perp_input)
        .await
        .unwrap();
    assert!(
        perp_preview
            .intent
            .debits
            .iter()
            .all(|debit| debit.resource.0 == unified_resource)
    );
    assert!(
        perp_preview
            .intent
            .expected
            .iter()
            .any(|constraint| matches!(constraint,
                fin_domain::EffectConstraint::DerivativePosition { resource, .. }
                    if resource.0.contains(":perpetual:primary")
            ))
    );
    let perp = engine
        .execute(request(
            "unified-perp",
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    let perp_oid = venue
        .orders
        .lock()
        .unwrap()
        .values()
        .find(|row| row["order"]["order"]["coin"] == "BTC")
        .unwrap()["order"]["order"]["oid"]
        .as_u64()
        .unwrap();
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(perp_oid, 72, "BTC", "B", "1", "10"));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0");
        row["order"]["status"] = json!("filled");
    });
    let perp_done = engine.status(&perp.id, true).await.unwrap();
    assert_eq!(perp_done.status, Status::Completed);
    let effects = &perp_done.effects.as_ref().unwrap().effects;
    assert!(effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::PositionChange { resource, .. }
            if resource.0.contains(":perpetual:primary")
    )));
    assert!(effects.iter().any(|effect| matches!(effect,
        fin_domain::FinancialEffect::Cost { location: fin_domain::AccountingLocation::Cash { resource }, .. }
            if resource.0 == unified_resource
    )));
}

#[tokio::test]
async fn matching_fill_with_wrong_side_or_market_cannot_settle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wrong-receipt.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let submitted = engine
        .execute(request(
            "wrong-fill",
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    let oid = venue.orders.lock().unwrap().values().next().unwrap()["order"]["order"]["oid"]
        .as_u64()
        .unwrap();
    // Matching order ID with contradictory venue identity is a rejected proof,
    // even though the venue still reports the order as resting.
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(oid, 20, "ETH", "A", "0.5", "10"));
    let rejected = engine.status(&submitted.id, true).await.unwrap();
    assert_ne!(rejected.status, Status::Completed);
    assert_eq!(rejected.effects, submitted.effects);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changed_content_for_an_already_proved_fill_keeps_prior_effects_reserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tampered-progress.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let submitted = engine
        .execute(request(
            "tampered-progress",
            "perpetual",
            "BTC/USDC:USDC",
            "1",
            "10",
        ))
        .await
        .unwrap();
    let oid = venue.orders.lock().unwrap().values().next().unwrap()["order"]["order"]["oid"]
        .as_u64()
        .unwrap();
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(oid, 21, "BTC", "B", "0.4", "10"));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0.6");
    });
    let accepted = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(accepted.status, Status::Pending);
    let accepted_effects = accepted.effects.clone().unwrap();
    let accepted_reservations = Store::open(&path)
        .unwrap()
        .reservations(&submitted.id)
        .unwrap();
    assert!(!accepted_reservations.is_empty());

    // The event identity is unchanged (same oid/tid/hash), but the venue row
    // now claims a different quantity. It cannot rewrite accepted progress.
    venue.fills.lock().unwrap()[0]["sz"] = json!("0.8");
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0.2");
    });
    let tampered = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(tampered.status, Status::Unknown);
    assert_eq!(tampered.effects.as_ref(), Some(&accepted_effects));
    assert_eq!(
        Store::open(&path)
            .unwrap()
            .reservations(&submitted.id)
            .unwrap(),
        accepted_reservations,
        "tampered later evidence must retain the previously accepted reservation state"
    );
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn confirmed_cancel_without_fills_is_failed_not_a_fabricated_fill() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no-fill.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let submitted = engine
        .execute(request("no-fills", "perpetual", "BTC/USDC:USDC", "1", "10"))
        .await
        .unwrap();
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("1");
        row["order"]["status"] = json!("canceled");
    });
    let failed = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert!(failed.effects.as_ref().unwrap().effects.is_empty());
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exact_plan_orders_are_sequenced_and_revocation_still_allows_pending_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plan.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let first_input = input("plan-order-1", "spot", "ETH/USDC", "1", "10");
    let second_input = input("plan-order-2", "spot", "ETH/USDC", "1", "10");
    let prepared = engine
        .preview("hyperliquid.orders.place", &first_input)
        .await
        .unwrap();
    let mut aggregate_debits = prepared.intent.debits.clone();
    for debit in &mut aggregate_debits {
        debit.amount.atoms = debit.amount.atoms.checked_add(&debit.amount.atoms).unwrap();
    }
    let exact_costs = prepared.intent.maximum_costs.clone();
    let mut aggregate_costs = exact_costs.clone();
    for cost in &mut aggregate_costs {
        cost.amount.atoms = cost.amount.atoms.checked_add(&cost.amount.atoms).unwrap();
    }
    let possible_credits: Vec<_> = prepared
        .intent
        .expected
        .iter()
        .filter_map(|expected| match expected {
            fin_domain::EffectConstraint::Conversion {
                resource, output, ..
            } => Some(fin_domain::financial_plans::FinancialPlanFundingIdentity {
                resource: resource.clone(),
                asset: output.clone(),
            }),
            _ => None,
        })
        .collect();
    let exact_scope = json!({
        "mode":"exact",
        "maximum_debits":prepared.intent.debits,
        "maximum_costs":exact_costs,
        "possible_credits":possible_credits
    });
    let step = |step_id: &str, input: Value| FinancialPlanStepSpec {
        step_id: step_id.into(),
        operation: "hyperliquid.orders.place".into(),
        input,
        permitted_variations: exact_scope.clone(),
        completion: FinancialStepCompletion {
            principal_spend: None,
            minimum_net_receipt: None,
            operation_succeeded: true,
        },
        funding_dependencies: vec![],
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "two bounded synthetic orders".into(),
        steps: vec![
            step("first", first_input.clone()),
            step("second", second_input.clone()),
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: aggregate_debits,
            maximum_costs: aggregate_costs,
        },
    };
    let mut authorization = FinancialPlanAuthorizationRequest {
        request_id: "plan-authorization".into(),
        plan,
        valid_for_seconds: 600,
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    authorization.expected_review_hash = Some(
        engine
            .preview_financial_plan(authorization.clone())
            .await
            .unwrap()
            .review_hash,
    );
    let approved = engine
        .authorize_financial_plan(authorization)
        .await
        .unwrap();
    let first = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "first",
            OperationRequest {
                request_id: "plan-first-attempt".into(),
                operation: "hyperliquid.orders.place".into(),
                input: first_input,
                run_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(first.status, Status::Pending);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);

    let second = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "second",
            OperationRequest {
                request_id: "plan-second-too-early".into(),
                operation: "hyperliquid.orders.place".into(),
                input: second_input,
                run_id: None,
            },
        )
        .await;
    assert!(
        second.is_err(),
        "the next exact plan step waits for verified completion"
    );
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);

    engine
        .revoke_financial_plan(&approved.plan_id)
        .await
        .unwrap();
    let oid = venue.orders.lock().unwrap().values().next().unwrap()["order"]["order"]["oid"]
        .as_u64()
        .unwrap();
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(oid, 41, "@0", "B", "1", "10"));
    venue.orders.lock().unwrap().values_mut().for_each(|row| {
        row["order"]["order"]["sz"] = json!("0");
        row["order"]["status"] = json!("filled");
    });
    let reconciled = engine.status(&first.id, true).await.unwrap();
    assert_eq!(reconciled.status, Status::Completed);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);

    let after_revoke = engine
        .execute_financial_plan_step(
            &approved.plan_id,
            "second",
            OperationRequest {
                request_id: "plan-second-after-revoke".into(),
                operation: "hyperliquid.orders.place".into(),
                input: input("plan-order-2", "spot", "ETH/USDC", "1", "10"),
                run_id: None,
            },
        )
        .await;
    assert!(after_revoke.is_err());
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 1);
}
