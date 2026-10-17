//! Coinbase spot buy-and-reverse named-plan rehearsal through Engine, Store,
//! the protocol socket, and optionally the public fin-dev CLI binary.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile, institutional};
use gofin_domain::{clock::Clock, financial_plans::*, *};
use gofin_exchange::{ExchangeAdapter, coinbase::*};
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

const ACCOUNT: &str = "coinbase:portfolio:synthetic-portfolio";
const CLIENT_IDENTITY: &str = "synthetic-coinbase-plan";
const EXCHANGE_ACCOUNT: &str = "coinbaseexchange:portfolio:synthetic-exchange-profile";

struct TestClock;
impl Clock for TestClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}
struct Approval(AtomicUsize);
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        assert!(challenge.binding["plan"]["steps"].is_array());
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            mechanism: "synthetic Coinbase named plan".into(),
            source: None,
            yolo_provenance: None,
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Prices;
#[async_trait]
impl gofin_pricing::UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<gofin_pricing::UsdQuote> {
        Ok(gofin_pricing::UsdQuote {
            usd_per_unit: match asset.reference.as_str() {
                "USD" | "USDC" => "1",
                "EUR" => "1.10",
                "BTC" => "100",
                "HBAR" => "0.10",
                _ => panic!("unexpected Coinbase asset {:?}", asset),
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic independent USD quote".into(),
        })
    }
}
#[derive(Default)]
struct Venue {
    calls: Mutex<Vec<(CoinbaseEndpoint, Option<String>)>>,
    orders: Mutex<BTreeMap<String, Value>>,
    writes: AtomicUsize,
    bought: std::sync::atomic::AtomicBool,
    sold: std::sync::atomic::AtomicBool,
    sell_then_hbar: std::sync::atomic::AtomicBool,
    hbar_path_step: AtomicUsize,
    native_funding_errors: std::sync::atomic::AtomicBool,
    native_create_rejection: std::sync::atomic::AtomicBool,
    progressive_settlement: std::sync::atomic::AtomicBool,
    progressive_order_reads: AtomicUsize,
    progressive_fill_reads: AtomicUsize,
    btc_eur_fok_path: std::sync::atomic::AtomicBool,
    btc_eur_preview_calls: AtomicUsize,
    btc_eur_preview_ids: Mutex<Vec<String>>,
    exchange_balances: Mutex<BTreeMap<String, String>>,
    exchange_orders: Mutex<BTreeMap<String, Value>>,
}
impl Venue {
    fn response(value: Value) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        Ok(CoinbaseResponse {
            status: 200,
            body: serde_json::to_vec(&value).unwrap(),

            pagination: Default::default(),
        })
    }
}
#[async_trait]
impl CoinbaseApi for Venue {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> std::result::Result<CoinbaseResponse, CoinbaseError> {
        if endpoint.mutating() {
            if !guard.as_ref().is_some_and(|check| check()) {
                return Err(CoinbaseError::GuardRejected);
            }
            self.writes.fetch_add(1, Ordering::SeqCst);
        } else {
            assert!(
                guard.is_none(),
                "read requests do not carry dispatch authority"
            );
        }
        self.calls
            .lock()
            .unwrap()
            .push((endpoint.clone(), request.body.clone()));
        match endpoint {
            CoinbaseRoute::KeyPermissions => Self::response(
                json!({"portfolio_uuid":"synthetic-portfolio-id","can_view":true,"can_trade":true,"can_transfer":true}),
            ),
            CoinbaseRoute::Product { product_id } => {
                let mut currencies = product_id.split('-');
                let base = currencies.next().unwrap();
                let quote = currencies.next().unwrap();
                Self::response(
                    json!({"product":{"product_type":"SPOT","trading_disabled":false,"base_currency_id":base,"quote_currency_id":quote,"base_increment":"0.00000001","quote_increment":"0.01","price_increment":"0.01","base_min_size":"0.00000001","quote_min_size":"0.01"}}),
                )
            }
            CoinbaseRoute::OrderPreview => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                if self.btc_eur_fok_path.load(Ordering::SeqCst) && body["product_id"] == "BTC-EUR" {
                    let call = self.btc_eur_preview_calls.fetch_add(1, Ordering::SeqCst);
                    let preview_id = format!("synthetic-btc-eur-preview-{call}");
                    self.btc_eur_preview_ids
                        .lock()
                        .unwrap()
                        .push(preview_id.clone());
                    let total = match (body["side"].as_str().unwrap(), call % 4) {
                        ("BUY", 0) => "1.98",
                        ("BUY", _) => "1.99",
                        ("SELL", 1) => "2.02",
                        ("SELL", _) => "2.01",
                        _ => panic!("unexpected BTC-EUR preview side"),
                    };
                    let commission = if call < 2 { "0.008" } else { "0.009" };
                    return Self::response(
                        json!({"preview_id":preview_id,"order_total":total,"commission_total":commission,"errs":[]}),
                    );
                }
                let total = match (
                    body["product_id"].as_str().unwrap(),
                    body["side"].as_str().unwrap(),
                ) {
                    ("HBAR-USDC", "SELL") => "2.50",
                    ("BTC-USDC", "BUY") => "2.30",
                    ("BTC-USDC", "SELL") => "2.30",
                    ("HBAR-USDC", "BUY") => "2.10",
                    (_, "BUY") => "100.00",
                    _ => "110.00",
                };
                if self.native_funding_errors.load(Ordering::SeqCst) {
                    let step = self.hbar_path_step.load(Ordering::SeqCst);
                    let (required_step, base, quote, native_total) = match (
                        body["product_id"].as_str().unwrap(),
                        body["side"].as_str().unwrap(),
                    ) {
                        ("HBAR-USDC", "SELL") => (0, "25", "2.50", "2.40"),
                        ("BTC-USDC", "BUY") => (1, "0.023", "2.30", "2.40"),
                        ("BTC-USDC", "SELL") => (2, "0.023", "2.30", "2.20"),
                        ("HBAR-USDC", "BUY") => (3, "21", "2.10", "2.20"),
                        _ => panic!("unexpected future-funded order"),
                    };
                    return Self::response(
                        json!({"base_size":base,"quote_size":quote,"order_total":native_total,"commission_total":"0.10","errs":if step < required_step {vec!["PREVIEW_INSUFFICIENT_FUND"]} else {vec![]}}),
                    );
                }
                Self::response(json!({"order_total":total,"commission_total":"0.10","errs":[]}))
            }
            CoinbaseRoute::Orders => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                if self.native_create_rejection.load(Ordering::SeqCst) {
                    return Self::response(json!({
                        "success":false,
                        "error_response":{"error":"INSUFFICIENT_FUND","message":"Insufficient funds"}
                    }));
                }
                let id = format!("venue-order-{}", self.writes.load(Ordering::SeqCst));
                let product_id = body["product_id"].as_str().unwrap();
                let order = if self.btc_eur_fok_path.load(Ordering::SeqCst)
                    && product_id == "BTC-EUR"
                {
                    let (price, size, value) = if body["side"] == "BUY" {
                        ("1990", "0.001", "1.99")
                    } else {
                        ("2010", "0.001", "2.01")
                    };
                    json!({"order_id":id,"client_order_id":body["client_order_id"],"product_id":product_id,"side":body["side"],"status":"FILLED","size_in_quote":false,"filled_size":size,"filled_value":value,"total_fees":"0.01","settled":true,"price":price})
                } else {
                    let (price, size, value) = if self.sell_then_hbar.load(Ordering::SeqCst) {
                        match (product_id, body["side"].as_str().unwrap()) {
                            ("HBAR-USDC", "SELL") => ("0.10", "25", "2.50"),
                            ("BTC-USDC", "BUY") => ("100", "0.023", "2.30"),
                            ("BTC-USDC", "SELL") => ("100", "0.023", "2.30"),
                            ("HBAR-USDC", "BUY") => ("0.10", "21", "2.10"),
                            _ => panic!(
                                "unexpected future-funded order {product_id} {}",
                                body["side"]
                            ),
                        }
                    } else if body["side"] == "BUY" {
                        ("100", "1", "100")
                    } else {
                        ("110", "1", "110")
                    };
                    json!({"order_id":id,"client_order_id":body["client_order_id"],"product_id":product_id,"side":body["side"],"status":"FILLED","size_in_quote":false,"filled_size":size,"filled_value":value,"total_fees":"0.10","settled":true,"price":price})
                };
                self.orders.lock().unwrap().insert(id.clone(), order);
                if body["side"] == "BUY" {
                    self.bought.store(true, Ordering::SeqCst);
                } else {
                    self.sold.store(true, Ordering::SeqCst);
                }
                if self.sell_then_hbar.load(Ordering::SeqCst) {
                    self.hbar_path_step.fetch_add(1, Ordering::SeqCst);
                }
                Self::response(json!({"success":true,"success_response":{"order_id":id}}))
            }
            CoinbaseRoute::Order { order_id } => {
                let mut order = self
                    .orders
                    .lock()
                    .unwrap()
                    .get(&order_id)
                    .cloned()
                    .ok_or(CoinbaseError::Transport)?;
                if self.progressive_settlement.load(Ordering::SeqCst) {
                    let stage = self.progressive_order_reads.fetch_add(1, Ordering::SeqCst) / 2;
                    order["status"] = json!(if stage == 0 {
                        "PARTIALLY_FILLED"
                    } else {
                        "FILLED"
                    });
                    order["size_in_quote"] = json!(false);
                    order["filled_size"] = json!(if stage == 0 { "0.25" } else { "0.5" });
                    order["filled_value"] = json!(if stage == 0 { "25" } else { "50" });
                    order["total_fees"] = json!(if stage == 0 { "0.10" } else { "0.20" });
                    order["settled"] = json!(stage != 0);
                }
                Self::response(json!({"order":order}))
            }
            CoinbaseRoute::Fills { query } => {
                let id = query
                    .order_ids
                    .first()
                    .cloned()
                    .or(query.order_id)
                    .unwrap_or_default();
                let order = self
                    .orders
                    .lock()
                    .unwrap()
                    .get(&id)
                    .cloned()
                    .ok_or(CoinbaseError::Transport)?;
                if self.progressive_settlement.load(Ordering::SeqCst) {
                    let stage = self.progressive_fill_reads.fetch_add(1, Ordering::SeqCst);
                    let mut fills = vec![
                        json!({"order_id":id,"entry_id":"progressive-fill-1","product_id":"BTC-USD","side":"BUY","price":"100","size":"0.25","size_in_quote":false,"commission":"0.10"}),
                    ];
                    if stage > 0 {
                        fills.push(json!({"order_id":id,"entry_id":"progressive-fill-2","product_id":"BTC-USD","side":"BUY","price":"100","size":"0.25","size_in_quote":false,"commission":"0.10"}));
                    }
                    return Self::response(
                        json!({"fills":fills,"cursor":"","proof_token_required":false}),
                    );
                }
                let side = order["side"].as_str().unwrap();
                let product_id = order["product_id"].as_str().unwrap();
                let (price, size) =
                    if self.btc_eur_fok_path.load(Ordering::SeqCst) && product_id == "BTC-EUR" {
                        if side == "BUY" {
                            ("1990", "0.001")
                        } else {
                            ("2010", "0.001")
                        }
                    } else if self.sell_then_hbar.load(Ordering::SeqCst) {
                        match (product_id, side) {
                            ("HBAR-USDC", "SELL") => ("0.10", "25"),
                            ("BTC-USDC", "BUY") => ("100", "0.023"),
                            ("BTC-USDC", "SELL") => ("100", "0.023"),
                            ("HBAR-USDC", "BUY") => ("0.10", "21"),
                            _ => panic!("unexpected synthetic product/side {product_id} {side}"),
                        }
                    } else if side == "BUY" {
                        ("100", "1")
                    } else {
                        ("110", "1")
                    };
                let commission =
                    if self.btc_eur_fok_path.load(Ordering::SeqCst) && product_id == "BTC-EUR" {
                        "0.01"
                    } else {
                        "0.10"
                    };
                Self::response(
                    json!({"fills":[{"order_id":id,"entry_id":format!("fill-{id}"),"product_id":product_id,"side":side,"price":price,"size":size,"size_in_quote":false,"commission":commission}],"has_next":false}),
                )
            }
            CoinbaseRoute::LegacyCurrencies => {
                if self.btc_eur_fok_path.load(Ordering::SeqCst) {
                    Self::response(json!({"data":[{"id":"USD"},{"id":"EUR"}]}))
                } else {
                    Self::response(json!({"data":[{"id":"USD"}]}))
                }
            }
            CoinbaseRoute::LegacyCryptoCurrencies => {
                Self::response(json!({"data":[{"id":"BTC"},{"id":"HBAR"},{"id":"USDC"}]}))
            }
            CoinbaseRoute::Accounts { .. } => {
                if self.btc_eur_fok_path.load(Ordering::SeqCst) {
                    let stage = self.writes.load(Ordering::SeqCst);
                    let (eur, btc) = match stage {
                        0 => ("2.00", "0"),
                        1 => ("0", "0.001"),
                        _ => ("2.00", "0"),
                    };
                    return Self::response(json!({"accounts":[
                        {"uuid":"eur-account","currency":"EUR","active":true,"ready":true,"available_balance":{"value":eur,"currency":"EUR"},"hold":{"value":"0","currency":"EUR"}},
                        {"uuid":"btc-account","currency":"BTC","active":true,"ready":true,"available_balance":{"value":btc,"currency":"BTC"},"hold":{"value":"0","currency":"BTC"}}
                    ],"has_next":false}));
                }
                if self.sell_then_hbar.load(Ordering::SeqCst) {
                    let (usdc, btc, hbar) = match self.hbar_path_step.load(Ordering::SeqCst) {
                        0 => ("0", "0", "25"),
                        1 => ("2.40", "0", "0"),
                        2 => ("0", "0.023", "0"),
                        3 => ("2.20", "0", "0"),
                        _ => ("0", "0", "21"),
                    };
                    return Self::response(json!({"accounts":[
                        {"uuid":"usdc-account","currency":"USDC","active":true,"ready":true,"available_balance":{"value":usdc,"currency":"USDC"},"hold":{"value":"0","currency":"USDC"}},
                        {"uuid":"btc-account","currency":"BTC","active":true,"ready":true,"available_balance":{"value":btc,"currency":"BTC"},"hold":{"value":"0","currency":"BTC"}},
                        {"uuid":"hbar-account","currency":"HBAR","active":true,"ready":true,"available_balance":{"value":hbar,"currency":"HBAR"},"hold":{"value":"0","currency":"HBAR"}}
                    ],"has_next":false}));
                }
                let (usd, btc) = if self.sold.load(Ordering::SeqCst) {
                    ("1010", "0")
                } else if self.bought.load(Ordering::SeqCst) {
                    ("900", "1")
                } else {
                    ("1000", "0")
                };
                Self::response(
                    json!({"accounts":[{"uuid":"usd-account","currency":"USD","active":true,"ready":true,"available_balance":{"value":usd,"currency":"USD"},"hold":{"value":"0","currency":"USD"}},{"uuid":"btc-account","currency":"BTC","active":true,"ready":true,"available_balance":{"value":btc,"currency":"BTC"},"hold":{"value":"0","currency":"BTC"}},{"uuid":"hbar-account","currency":"HBAR","active":true,"ready":true,"available_balance":{"value":"0","currency":"HBAR"},"hold":{"value":"0","currency":"HBAR"}}],"has_next":false}),
                )
            }
            CoinbaseRoute::HistoricalOrders { .. } => {
                Self::response(json!({"orders":[],"has_next":false}))
            }
            route @ CoinbaseRoute::NativeRead { .. } => {
                let path = route.path().unwrap();
                let (id, base) = match path.as_str() {
                    "/products/HBAR-USDC" => ("HBAR-USDC", "HBAR"),
                    "/products/BTC-USDC" => ("BTC-USDC", "BTC"),
                    _ => panic!("unexpected Exchange metadata path {path}"),
                };
                Self::response(
                    json!({"id":id,"base_currency":base,"quote_currency":"USDC","base_increment":"0.001","price_increment":"0.01","base_min_size":"0.001","status":"online","trading_disabled":false}),
                )
            }
            CoinbaseRoute::ExchangeFees => {
                Self::response(json!({"maker_fee_rate":"0.001","taker_fee_rate":"0.002"}))
            }
            CoinbaseRoute::ExchangeAccounts => {
                let balances = self.exchange_balances.lock().unwrap();
                Self::response(json!(
                    ["HBAR", "USDC", "BTC"].into_iter().map(|currency| {
                        let available = balances.get(currency).cloned().unwrap_or_else(|| "0".into());
                        json!({"id":format!("{currency}-account"),"profile_id":"synthetic-exchange-profile","currency":currency,"available":available,"hold":"0","balance":balances.get(currency).cloned().unwrap_or_else(|| "0".into())})
                    }).collect::<Vec<_>>()
                ))
            }
            CoinbaseRoute::ExchangeLoanLendingOverview => Self::response(
                json!({"overview":{"collateral_value":"0","withdrawal_restricted":false},"loans":[]}),
            ),
            CoinbaseRoute::ExchangeOrders => {
                let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                let product_id = body["product_id"].as_str().unwrap();
                let side = body["side"].as_str().unwrap();
                let size = body["size"].as_str().unwrap();
                let price = body["price"].as_str().unwrap();
                let (value, fee) = match (product_id, side) {
                    ("HBAR-USDC", "sell") => ("2.5", "0.005"),
                    ("BTC-USDC", "buy") => ("2.3", "0.0046"),
                    _ => panic!("unexpected institutional Exchange order {product_id} {side}"),
                };
                let id = format!("exchange-order-{}", self.writes.load(Ordering::SeqCst));
                let order = json!({"id":id,"profile_id":"synthetic-exchange-profile","client_oid":body["client_oid"],"product_id":product_id,"side":side,"type":"limit","time_in_force":body["time_in_force"],"price":price,"size":size,"status":"done","filled_size":size,"executed_value":value,"fill_fees":fee,"settled":true});
                {
                    let mut orders = self.exchange_orders.lock().unwrap();
                    orders.insert(id.clone(), order.clone());
                }
                {
                    let mut balances = self.exchange_balances.lock().unwrap();
                    match (product_id, side) {
                        ("HBAR-USDC", "sell") => {
                            balances.insert("HBAR".into(), "0".into());
                            balances.insert("USDC".into(), "2.495".into());
                        }
                        ("BTC-USDC", "buy") => {
                            balances.insert("USDC".into(), "0.1904".into());
                            balances.insert("BTC".into(), "0.023".into());
                        }
                        _ => unreachable!(),
                    }
                }
                Self::response(order)
            }
            CoinbaseRoute::ExchangeOrderByClientId { client_oid } => {
                let order = self
                    .exchange_orders
                    .lock()
                    .unwrap()
                    .values()
                    .find(|order| order["client_oid"] == client_oid)
                    .cloned()
                    .ok_or(CoinbaseError::Transport)?;
                Self::response(order)
            }
            CoinbaseRoute::ExchangeOrder { order_id } => {
                let order = self
                    .exchange_orders
                    .lock()
                    .unwrap()
                    .get(&order_id)
                    .cloned()
                    .ok_or(CoinbaseError::Transport)?;
                Self::response(order)
            }
            CoinbaseRoute::ExchangeFillsFiltered { query } => {
                let orders = self.exchange_orders.lock().unwrap();
                let order = orders
                    .values()
                    .find(|order| order["id"] == query.order_id.clone().unwrap_or_default())
                    .cloned()
                    .ok_or(CoinbaseError::Transport)?;
                let (size, fee, price, trade_id) = if order["product_id"] == "HBAR-USDC" {
                    ("25", "0.005", "0.1", 81)
                } else {
                    ("0.023", "0.0046", "100", 82)
                };
                Self::response(
                    json!([{"trade_id":trade_id,"order_id":order["id"],"profile_id":"synthetic-exchange-profile","product_id":order["product_id"],"side":order["side"],"size":size,"price":price,"fee":fee,"settled":true}]),
                )
            }
            _ => panic!("unexpected Coinbase endpoint {:?}", endpoint),
        }
    }
}

fn adapter(venue: Arc<Venue>) -> Arc<ExchangeAdapter<CoinbaseProfile>> {
    let transport: Arc<dyn CoinbaseApi> = venue;
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "synthetic-portfolio".into(),
            resource: ResourceId(ACCOUNT.into()),
            credential_ref: "synthetic-credential".into(),
            physical_identity: "synthetic-portfolio-id".into(),
            identity_verified: true,
            portfolio_id: Some("synthetic-portfolio-id".into()),
            product: CoinbaseProduct::Advanced,
        }],
        BTreeMap::from([("synthetic-credential".into(), transport)]),
    )
    .unwrap()
    .with_prices(Arc::new(Prices));
    Arc::new(ExchangeAdapter::new(profile))
}
fn exchange_adapter(venue: Arc<Venue>) -> Arc<ExchangeAdapter<CoinbaseProfile>> {
    let transport: Arc<dyn CoinbaseApi> = venue;
    let profile = CoinbaseProfile::new(
        vec![CoinbaseAccount {
            name: "synthetic-exchange".into(),
            resource: ResourceId(EXCHANGE_ACCOUNT.into()),
            credential_ref: "synthetic-exchange-credential".into(),
            physical_identity: "synthetic-exchange-profile".into(),
            identity_verified: true,
            portfolio_id: None,
            product: CoinbaseProduct::Exchange,
        }],
        BTreeMap::from([("synthetic-exchange-credential".into(), transport)]),
    )
    .unwrap()
    .with_prices(Arc::new(Prices));
    Arc::new(ExchangeAdapter::new(profile))
}
fn runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    approval: Arc<Approval>,
) -> Arc<gofin_broker::BrokerRuntime> {
    let mut policy = BrokerPolicy::beta();
    // The approved rehearsal allows the two-leg ~$200 round trip while staying
    // a small finite synthetic limit. A separate test keeps the $100 default
    // fee-inclusive ceiling fail-closed.
    policy.max_movement_usd_micros = "300000000".into();
    policy.max_run_allocation_usd_micros = "500000000".into();
    runtime_with_policy(path, venue, approval, policy)
}
fn runtime_with_policy(
    path: &std::path::Path,
    venue: Arc<Venue>,
    approval: Arc<Approval>,
    policy: BrokerPolicy,
) -> Arc<gofin_broker::BrokerRuntime> {
    compose_runtime_with_clock(
        BrokerConfigV2 {
            version: 2,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![],
            policy,
            approver_uid: unsafe { libc::geteuid() },
            extensions: BTreeMap::new(),
        },
        RuntimeDependencies {
            store: Store::open(path).unwrap(),
            adapters: vec![adapter(venue)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap()
}
fn exchange_runtime(
    path: &std::path::Path,
    venue: Arc<Venue>,
    approval: Arc<Approval>,
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
            adapters: vec![exchange_adapter(venue)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap()
}
include!("support/coinbase_plan_client.rs");

fn exact_step(
    id: &str,
    prepared: &PreparedOperation,
    input: Value,
    credits: Value,
    dependencies: Vec<FinancialPlanFundingDependency>,
) -> FinancialPlanStepSpec {
    FinancialPlanStepSpec {
        step_id: id.into(),
        operation: prepared.operation.clone(),
        input,
        permitted_variations: json!({"mode":"exact","maximum_debits":prepared.intent.debits,"maximum_costs":prepared.intent.maximum_costs,"possible_credits":credits}),
        funding_dependencies: dependencies,
        action_deadline_seconds: None,
        completion: FinancialStepCompletion {
            operation_succeeded: true,
            principal_spend: None,
            minimum_net_receipt: None,
        },
    }
}

fn aggregate_cost_bounds(costs: impl IntoIterator<Item = CostBound>) -> Vec<CostBound> {
    let mut aggregate: Vec<CostBound> = Vec::new();
    for cost in costs {
        if let Some(existing) = aggregate.iter_mut().find(|existing| {
            existing.location == cost.location
                && existing.amount.asset == cost.amount.asset
                && existing.treatment == cost.treatment
        }) {
            existing.amount.atoms = existing
                .amount
                .atoms
                .checked_add(&cost.amount.atoms)
                .unwrap();
        } else {
            aggregate.push(cost);
        }
    }
    aggregate
}

fn aggregate_debit_bounds(
    debits: impl IntoIterator<Item = gofin_domain::ResourceAmount>,
) -> Vec<gofin_domain::ResourceAmount> {
    let mut aggregate: Vec<gofin_domain::ResourceAmount> = Vec::new();
    for debit in debits {
        if let Some(existing) = aggregate.iter_mut().find(|existing| {
            existing.resource == debit.resource && existing.amount.asset == debit.amount.asset
        }) {
            existing.amount.atoms = existing
                .amount
                .atoms
                .checked_add(&debit.amount.atoms)
                .unwrap();
        } else {
            aggregate.push(debit);
        }
    }
    aggregate
}

#[test]
fn coinbase_named_plan_executes_buy_then_reverse_with_one_approval_and_restart_identity() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse())
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn default_movement_cap_rejects_one_hundred_dollars_plus_fee_before_approval_or_dispatch() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_default_cap())
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn coinbase_sell_creates_verified_net_quote_for_following_hbar_buy_from_zero_quote_balance() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_zero_quote_hbar_funding())
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn coinbase_exchange_sell_funds_a_later_order_through_broker_plan() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_exchange_future_funding())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn rehearse_exchange_future_funding() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-exchange-future-funding.sqlite");
    let venue = Arc::new(Venue::default());
    venue.exchange_balances.lock().unwrap().extend([
        ("HBAR".into(), "25".into()),
        ("USDC".into(), "0".into()),
        ("BTC".into(), "0".into()),
    ]);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let adapter = exchange_adapter(venue.clone());
    let sell = json!({"account":"synthetic-exchange","requestId":"exchange-sell-hbar","productId":"HBAR-USDC","side":"SELL","baseSize":"25","limitPrice":"0.10","timeInForce":"FOK","maximumSpend":"25","minimumReceive":"2.49","maximumFee":"0.01"});
    let buy = json!({"account":"synthetic-exchange","requestId":"exchange-buy-btc","productId":"BTC-USDC","side":"BUY","baseSize":"0.023","limitPrice":"100","timeInForce":"FOK","maximumSpend":"2.3","minimumReceive":"0.022","maximumFee":"0.01"});
    let sell_prepared = adapter
        .prepare("coinbaseexchange.create-order", &sell)
        .await
        .unwrap();
    let buy_prepared = institutional::prepare_order_for_plan(
        adapter.profile(),
        "coinbaseexchange.create-order",
        &buy,
    )
    .await
    .unwrap()
    .unwrap();
    let usdc = AssetId {
        network: "coinbaseexchange".into(),
        reference: "USDC".into(),
        decimals: 36,
    };
    let btc = AssetId {
        network: "coinbaseexchange".into(),
        reference: "BTC".into(),
        decimals: 36,
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Exchange HBAR sale funds a BTC purchase".into(),
        summary: Some("Sell the enrolled HBAR balance, then use only verified fee-net USDC proceeds for the BTC order.".into()),
        steps: vec![
            exact_step("sell-hbar", &sell_prepared, sell.clone(), json!([{"resource":EXCHANGE_ACCOUNT,"asset":usdc}]), vec![]),
            exact_step("buy-btc", &buy_prepared, buy.clone(), json!([{"resource":EXCHANGE_ACCOUNT,"asset":btc}]), vec![FinancialPlanFundingDependency { producing_step_id:"sell-hbar".into(), resource:ResourceId(EXCHANGE_ACCOUNT.into()), asset:usdc, funding_scope:None, maximum_fraction_bps: None }]),
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: aggregate_debit_bounds(sell_prepared.intent.debits.iter().chain(&buy_prepared.intent.debits).cloned()),
            maximum_costs: aggregate_cost_bounds(sell_prepared.intent.maximum_costs.iter().chain(&buy_prepared.intent.maximum_costs).cloned()),
        },
    };
    let client = Client::new(
        exchange_runtime(&path, venue.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    let mut authorization = FinancialPlanAuthorizationRequest {
        request_id: "coinbase-exchange-future-funding-plan".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(900),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: authorization.clone(),
            },
            true,
        )
        .await;
    authorization.expected_review_hash = Some(
        preview["review_hash"]
            .as_str()
            .expect("plan preview succeeds with explicit future funding")
            .into(),
    );
    let approved = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--id",
                "coinbase-exchange-future-funding-plan",
                "--review-hash",
                authorization.expected_review_hash.as_deref().unwrap(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan {
                request: authorization,
            },
            true,
        )
        .await;
    let plan_id = approved["plan_id"]
        .as_str()
        .expect("plan authorization succeeds")
        .to_owned();
    for (step, operation_input) in [("sell-hbar", sell), ("buy-btc", buy)] {
        let result = client
            .call(
                args(&[
                    "execute",
                    "coinbaseexchange.create-order",
                    "--input",
                    &operation_input.to_string(),
                    "--id",
                    operation_input["requestId"].as_str().unwrap(),
                    "--plan",
                    &plan_id,
                    "--step",
                    step,
                    "--json",
                ]),
                BrokerRequest::ExecutePlanStep {
                    plan_id: plan_id.clone(),
                    step_id: step.into(),
                    request: OperationRequest {
                        request_id: operation_input["requestId"].as_str().unwrap().into(),
                        operation: "coinbaseexchange.create-order".into(),
                        input: operation_input,
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(result["status"], "completed", "{result}");
    }
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        venue.exchange_balances.lock().unwrap().get("HBAR").unwrap(),
        "0"
    );
    assert_eq!(
        venue.exchange_balances.lock().unwrap().get("USDC").unwrap(),
        "0.1904"
    );
    assert_eq!(
        venue.exchange_balances.lock().unwrap().get("BTC").unwrap(),
        "0.023"
    );
}

#[test]
fn coinbase_btc_eur_fok_buy_then_sell_rehearses_through_public_cli_and_store_restart() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_btc_eur_fok())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn rehearse_btc_eur_fok() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-btc-eur-fok.sqlite");
    let venue = Arc::new(Venue::default());
    venue.btc_eur_fok_path.store(true, Ordering::SeqCst);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let adapter = adapter(venue.clone());
    let buy = json!({"account":"synthetic-portfolio","requestId":"btc-eur-fok-buy","productId":"BTC-EUR","side":"BUY","orderConfiguration":{"limit_limit_fok":{"base_size":"0.001","limit_price":"1990.00"}},"maximumSpend":"1.99","minimumReceive":"0.00099","maximumFee":"0.01"});
    let sell = json!({"account":"synthetic-portfolio","requestId":"btc-eur-fok-sell","productId":"BTC-EUR","side":"SELL","orderConfiguration":{"limit_limit_fok":{"base_size":"0.001","limit_price":"2010.00"}},"maximumSpend":"0.001","minimumReceive":"2.00","maximumFee":"0.01"});
    let buy_prepared = adapter
        .prepare("coinbase.create-order", &buy)
        .await
        .unwrap();
    let sell_prepared = adapter
        .prepare("coinbase.create-order", &sell)
        .await
        .unwrap();
    let btc = AssetId {
        network: "coinbase".into(),
        reference: "BTC".into(),
        decimals: 36,
    };
    let eur = AssetId {
        network: "coinbase".into(),
        reference: "EUR".into(),
        decimals: 36,
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "BTC-EUR FOK buy then verified-proceeds sell".into(),
        summary: Some("Buy within a finite EUR principal and fee cap, then sell only the verified BTC credit.".into()),
        steps: vec![
            exact_step("buy", &buy_prepared, buy.clone(), json!([{"resource":ACCOUNT,"asset":btc}]), vec![]),
            exact_step("sell", &sell_prepared, sell.clone(), json!([{"resource":ACCOUNT,"asset":eur}]), vec![FinancialPlanFundingDependency { producing_step_id: "buy".into(), resource: ResourceId(ACCOUNT.into()), asset: btc, funding_scope: None, maximum_fraction_bps: None }]),
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: aggregate_debit_bounds(buy_prepared.intent.debits.iter().chain(&sell_prepared.intent.debits).cloned()),
            maximum_costs: aggregate_cost_bounds(buy_prepared.intent.maximum_costs.iter().chain(&sell_prepared.intent.maximum_costs).cloned()),
        },
    };
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let mut authorization = FinancialPlanAuthorizationRequest {
        request_id: "btc-eur-fok-plan".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(900),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: authorization.clone(),
            },
            true,
        )
        .await;
    authorization.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorized = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--id",
                "btc-eur-fok-plan",
                "--review-hash",
                authorization.expected_review_hash.as_deref().unwrap(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan {
                request: authorization,
            },
            true,
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    let mut saved = Vec::new();
    for (step, request_id, input) in [
        ("buy", "btc-eur-fok-buy", buy),
        ("sell", "btc-eur-fok-sell", sell),
    ] {
        let request = OperationRequest {
            request_id: request_id.into(),
            operation: "coinbase.create-order".into(),
            input: input.clone(),
            run_id: None,
        };
        let argv = args(&[
            "execute",
            "coinbase.create-order",
            "--input",
            &input.to_string(),
            "--id",
            request_id,
            "--plan",
            &plan_id,
            "--step",
            step,
            "--json",
        ]);
        let call = BrokerRequest::ExecutePlanStep {
            plan_id: plan_id.clone(),
            step_id: step.into(),
            request,
        };
        let result = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(result["status"], "completed", "{result}");
        let record: OperationRecord = serde_json::from_value(result.clone()).unwrap();
        assert!(record.submission.is_some());
        saved.push((argv, call, result));
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    {
        let preview_ids = venue.btc_eur_preview_ids.lock().unwrap();
        assert!(
            preview_ids.len() >= 4,
            "execution must refresh both approved previews: {preview_ids:?}"
        );
        assert!(
            preview_ids.windows(2).all(|ids| ids[0] != ids[1]),
            "each native preview must have a new ID: {preview_ids:?}"
        );
    }
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    drop(client);
    let resumed = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    for (argv, call, previous) in saved {
        let replay = resumed.call(argv, call, true).await;
        assert_eq!(replay["id"], previous["id"]);
        assert_eq!(replay["submission"], previous["submission"]);
    }
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        2,
        "restart reuses the original submission without replaying either order"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let requests = venue.calls.lock().unwrap();
    let order_bodies = requests
        .iter()
        .filter(|(route, _)| route == &CoinbaseRoute::Orders)
        .map(|(_, body)| serde_json::from_str::<Value>(body.as_deref().unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(order_bodies.len(), 2);
    assert!(
        order_bodies
            .iter()
            .all(|body| body["product_id"] == "BTC-EUR"
                && body["order_configuration"]["limit_limit_fok"].is_object())
    );
}

async fn rehearse_zero_quote_hbar_funding() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-zero-quote.sqlite");
    let venue = Arc::new(Venue::default());
    venue.sell_then_hbar.store(true, Ordering::SeqCst);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let adapter = adapter(venue.clone());
    let sell_hbar = json!({"account":"synthetic-portfolio","requestId":"sell-hbar","productId":"HBAR-USDC","side":"SELL","orderConfiguration":{"market_market_ioc":{"base_size":"25"}},"maximumSpend":"25","minimumReceive":"2.40","maximumFee":"0.10"});
    let buy_btc = json!({"account":"synthetic-portfolio","requestId":"buy-btc","productId":"BTC-USDC","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"2.30"}},"maximumSpend":"2.30","minimumReceive":"0.022","maximumFee":"0.10"});
    let sell_btc = json!({"account":"synthetic-portfolio","requestId":"sell-btc","productId":"BTC-USDC","side":"SELL","orderConfiguration":{"market_market_ioc":{"base_size":"0.023"}},"maximumSpend":"0.023","minimumReceive":"2.20","maximumFee":"0.10"});
    let buy_hbar = json!({"account":"synthetic-portfolio","requestId":"buy-hbar","productId":"HBAR-USDC","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"2.10"}},"maximumSpend":"2.10","minimumReceive":"20","maximumFee":"0.10"});
    let prepared = [
        adapter
            .prepare("coinbase.create-order", &sell_hbar)
            .await
            .unwrap(),
        adapter
            .prepare("coinbase.create-order", &buy_btc)
            .await
            .unwrap(),
        adapter
            .prepare("coinbase.create-order", &sell_btc)
            .await
            .unwrap(),
        adapter
            .prepare("coinbase.create-order", &buy_hbar)
            .await
            .unwrap(),
    ];
    let inputs = vec![sell_hbar, buy_btc, sell_btc, buy_hbar];
    let usdc = AssetId {
        network: "coinbase".into(),
        reference: "USDC".into(),
        decimals: 36,
    };
    let btc = AssetId {
        network: "coinbase".into(),
        reference: "BTC".into(),
        decimals: 36,
    };
    let hbar = AssetId {
        network: "coinbase".into(),
        reference: "HBAR".into(),
        decimals: 36,
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Coinbase HBAR-USDC-BTC-USDC-HBAR verified funding path".into(),
        summary: Some("Four sequential spot trades use only verified fee-net proceeds.".into()),
        steps: vec![
            exact_step(
                "sell-hbar",
                &prepared[0],
                inputs[0].clone(),
                json!([{"resource":ACCOUNT,"asset":usdc}]),
                vec![],
            ),
            exact_step(
                "buy-btc",
                &prepared[1],
                inputs[1].clone(),
                json!([{"resource":ACCOUNT,"asset":btc}]),
                vec![FinancialPlanFundingDependency {
                    producing_step_id: "sell-hbar".into(),
                    resource: ResourceId(ACCOUNT.into()),
                    asset: usdc.clone(),
                    funding_scope: None,
                    maximum_fraction_bps: None,
                }],
            ),
            exact_step(
                "sell-btc",
                &prepared[2],
                inputs[2].clone(),
                json!([{"resource":ACCOUNT,"asset":usdc}]),
                vec![FinancialPlanFundingDependency {
                    producing_step_id: "buy-btc".into(),
                    resource: ResourceId(ACCOUNT.into()),
                    asset: btc,
                    funding_scope: None,
                    maximum_fraction_bps: None,
                }],
            ),
            exact_step(
                "buy-hbar",
                &prepared[3],
                inputs[3].clone(),
                json!([{"resource":ACCOUNT,"asset":hbar}]),
                vec![FinancialPlanFundingDependency {
                    producing_step_id: "sell-btc".into(),
                    resource: ResourceId(ACCOUNT.into()),
                    asset: usdc,
                    funding_scope: None,
                    maximum_fraction_bps: None,
                }],
            ),
        ],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: aggregate_debit_bounds(
                prepared
                    .iter()
                    .flat_map(|item| item.intent.debits.iter())
                    .cloned(),
            ),
            maximum_costs: aggregate_cost_bounds(
                prepared
                    .iter()
                    .flat_map(|item| item.intent.maximum_costs.iter())
                    .cloned(),
            ),
        },
    };
    venue.native_funding_errors.store(true, Ordering::SeqCst);
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let request = FinancialPlanAuthorizationRequest {
        request_id: "hbar-usdc-btc-usdc-hbar-plan".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
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
    assert!(
        preview.to_string().contains("BTC") && preview.to_string().contains("USDC"),
        "{preview}"
    );
    let mut request = request;
    request.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let approved = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--id",
                "hbar-usdc-btc-usdc-hbar-plan",
                "--review-hash",
                request.expected_review_hash.as_deref().unwrap(),
                "--valid-for",
                "30m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan { request },
            true,
        )
        .await;
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let plan_id = approved["plan_id"].as_str().unwrap().to_owned();
    for (step_id, input) in ["sell-hbar", "buy-btc", "sell-btc", "buy-hbar"]
        .into_iter()
        .zip(inputs)
    {
        let response = client
            .call(
                args(&[
                    "execute",
                    "coinbase.create-order",
                    "--input",
                    &input.to_string(),
                    "--id",
                    input["requestId"].as_str().unwrap(),
                    "--plan",
                    &plan_id,
                    "--step",
                    step_id,
                    "--json",
                ]),
                BrokerRequest::ExecutePlanStep {
                    plan_id: plan_id.clone(),
                    step_id: step_id.into(),
                    request: OperationRequest {
                        request_id: input["requestId"].as_str().unwrap().into(),
                        operation: "coinbase.create-order".into(),
                        input,
                        run_id: None,
                    },
                },
                true,
            )
            .await;
        assert_eq!(response["status"], "completed", "{response}");
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 4);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let orders = venue.orders.lock().unwrap();
    assert_eq!(orders.len(), 4);
    for (side, product) in [
        ("SELL", "HBAR-USDC"),
        ("BUY", "BTC-USDC"),
        ("SELL", "BTC-USDC"),
        ("BUY", "HBAR-USDC"),
    ] {
        assert!(
            orders
                .values()
                .any(|order| order["side"] == side && order["product_id"] == product),
            "missing {side} {product}"
        );
    }
}

#[test]
fn coinbase_partial_then_complete_reconciliation_applies_cumulative_fills_once_across_store() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_progressive_settlement())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn rehearse_progressive_settlement() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-progressive.sqlite");
    let venue = Arc::new(Venue::default());
    venue.progressive_settlement.store(true, Ordering::SeqCst);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let adapter = adapter(venue.clone());
    let input = json!({"account":"synthetic-portfolio","requestId":"progressive-order","productId":"BTC-USD","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"50"}},"maximumSpend":"50","minimumReceive":"0.49","maximumFee":"0.20"});
    let prepared = adapter
        .prepare("coinbase.create-order", &input)
        .await
        .unwrap();
    let step = exact_step(
        "buy",
        &prepared,
        input.clone(),
        json!([{"resource":ACCOUNT,"asset":{"network":"coinbase","reference":"BTC","decimals":36}}]),
        vec![],
    );
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Progressively reconcile one Coinbase order".into(),
        summary: Some("Persist each fill once; resume the original order without replay.".into()),
        steps: vec![step],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: aggregate_debit_bounds(prepared.intent.debits.clone()),
            maximum_costs: aggregate_cost_bounds(prepared.intent.maximum_costs.clone()),
        },
    };
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let mut authorization = FinancialPlanAuthorizationRequest {
        request_id: "progressive-plan".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(900),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = client
        .call(
            args(&[
                "plans",
                "preview",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::PreviewPlan {
                request: authorization.clone(),
            },
            true,
        )
        .await;
    authorization.expected_review_hash = Some(preview["review_hash"].as_str().unwrap().into());
    let authorized = client
        .call(
            args(&[
                "plans",
                "authorize",
                "--input",
                &serde_json::to_string(&plan).unwrap(),
                "--id",
                "progressive-plan",
                "--review-hash",
                authorization.expected_review_hash.as_deref().unwrap(),
                "--valid-for",
                "15m",
                "--json",
            ]),
            BrokerRequest::AuthorizePlan {
                request: authorization,
            },
            true,
        )
        .await;
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    let run_step = || BrokerRequest::ExecutePlanStep {
        plan_id: plan_id.clone(),
        step_id: "buy".into(),
        request: OperationRequest {
            request_id: "progressive-order".into(),
            operation: "coinbase.create-order".into(),
            input: input.clone(),
            run_id: None,
        },
    };
    let first = client
        .call(
            args(&[
                "execute",
                "coinbase.create-order",
                "--input",
                &input.to_string(),
                "--id",
                "progressive-order",
                "--plan",
                &plan_id,
                "--step",
                "buy",
                "--json",
            ]),
            run_step(),
            true,
        )
        .await;
    assert_eq!(first["status"], "pending", "{first}");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    let first_record: OperationRecord = serde_json::from_value(first.clone()).unwrap();
    assert_eq!(
        first_record.effects.as_ref().unwrap().evidence["settlement_progress"]["sequence"],
        1
    );
    let second = client
        .call(
            args(&[
                "execute",
                "coinbase.create-order",
                "--input",
                &input.to_string(),
                "--id",
                "progressive-order",
                "--plan",
                &plan_id,
                "--step",
                "buy",
                "--json",
            ]),
            run_step(),
            true,
        )
        .await;
    assert_eq!(second["status"], "completed", "{second}");
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        1,
        "resume must reconcile, not POST again"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let final_record: OperationRecord = serde_json::from_value(second.clone()).unwrap();
    let verified = final_record.effects.unwrap();
    assert_eq!(verified.evidence["settlement_progress"]["sequence"], 2);
    assert_eq!(verified.evidence["fillCount"], 2);
    let effects = serde_json::to_value(verified.effects).unwrap();
    let conversions: Vec<_> = effects
        .as_array()
        .unwrap()
        .iter()
        .filter(|effect| effect["kind"] == "conversion")
        .collect();
    assert_eq!(conversions.len(), 1, "persist cumulative conversion once");
    assert_eq!(
        conversions[0]["spent"]["atoms"],
        format!("50{}", "0".repeat(36))
    );
    assert_eq!(
        conversions[0]["received"]["atoms"],
        format!("5{}", "0".repeat(35))
    );
    let fees: Vec<_> = effects
        .as_array()
        .unwrap()
        .iter()
        .filter(|effect| effect["kind"] == "cost")
        .collect();
    assert_eq!(fees.len(), 1, "aggregate the cumulative fee exactly once");
    assert_eq!(fees[0]["amount"]["atoms"], format!("2{}", "0".repeat(35)));
    let stored = Store::open(&path)
        .unwrap()
        .get_record(&final_record.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored["effects"]["evidence"]["settlement_progress"]["sequence"],
        2
    );
    assert_eq!(
        stored["submission"]["reference"],
        first_record.submission.unwrap().reference
    );
}

async fn rehearse_default_cap() {
    let temp = tempfile::tempdir().unwrap();
    let venue = Arc::new(Venue::default());
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let adapter = adapter(venue.clone());
    let input = json!({"account":"synthetic-portfolio","requestId":"default-cap-buy","productId":"BTC-USD","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"100.00"}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"});
    let prepared = adapter
        .prepare("coinbase.create-order", &input)
        .await
        .unwrap();
    let step = exact_step("buy", &prepared, input, json!([]), vec![]);
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Default cap fee-inclusive rejection".into(),
        summary: None,
        steps: vec![step],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: prepared.intent.debits.clone(),
            maximum_costs: prepared.intent.maximum_costs.clone(),
        },
    };
    let runtime = runtime_with_policy(
        &temp.path().join("default-cap.sqlite"),
        venue.clone(),
        approval.clone(),
        BrokerPolicy::beta(),
    );
    let mut request = FinancialPlanAuthorizationRequest {
        request_id: "default-cap-plan".into(),
        plan,
        valid_for_seconds: Some(900),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = runtime
        .handle(BrokerRequest::PreviewPlan {
            request: request.clone(),
        })
        .await;
    assert!(
        preview.ok,
        "preview should expose the reviewed policy check"
    );
    request.expected_review_hash = Some(
        preview.result.as_ref().unwrap()["review_hash"]
            .as_str()
            .unwrap()
            .into(),
    );
    let authorized = runtime
        .handle(BrokerRequest::AuthorizePlan {
            request: request.clone(),
        })
        .await;
    if authorized.ok {
        let plan_id = authorized.result.as_ref().unwrap()["plan_id"]
            .as_str()
            .unwrap();
        let execute = runtime
            .handle(BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.into(),
                step_id: "buy".into(),
                request: OperationRequest {
                    request_id: "default-cap-buy".into(),
                    operation: "coinbase.create-order".into(),
                    input: json!({"account":"synthetic-portfolio","requestId":"default-cap-buy","productId":"BTC-USD","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"100.00"}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"}),
                    run_id: None,
                },
            })
            .await;
        assert!(
            execute.ok,
            "operation failure is returned as a durable record"
        );
        let record = execute.result.unwrap();
        assert_eq!(record["status"], "failed", "{record}");
        assert_eq!(record["error"]["code"], "MOVEMENT_LIMIT", "{record}");
    } else {
        assert_eq!(authorized.error.unwrap().code, "MOVEMENT_LIMIT");
    }
    assert!(approval.0.load(Ordering::SeqCst) <= 1);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 0);
}
async fn rehearse() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-plan.sqlite");
    let venue = Arc::new(Venue::default());
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let a = adapter(venue.clone());
    let buy = json!({"account":"synthetic-portfolio","requestId":"plan-buy","productId":"BTC-USD","side":"BUY","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"100.00","post_only":false}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"});
    let sell = json!({"account":"synthetic-portfolio","requestId":"plan-sell","productId":"BTC-USD","side":"SELL","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"110.00","post_only":false}},"maximumSpend":"1.00","minimumReceive":"100.00","maximumFee":"0.10"});
    let b = a.prepare("coinbase.create-order", &buy).await.unwrap();
    let s = a.prepare("coinbase.create-order", &sell).await.unwrap();
    let usd = AssetId {
        network: "coinbase".into(),
        reference: "USD".into(),
        decimals: 36,
    };
    let btc = AssetId {
        network: "coinbase".into(),
        reference: "BTC".into(),
        decimals: 36,
    };
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Synthetic Coinbase buy and reverse".into(),
        summary: Some("Buy BTC with USD, then sell the same BTC back to USD.".into()),
        steps: vec![
            exact_step(
                "buy",
                &b,
                buy.clone(),
                json!([{"resource":ACCOUNT,"asset":btc}]),
                vec![],
            ),
            exact_step(
                "sell",
                &s,
                sell.clone(),
                json!([{"resource":ACCOUNT,"asset":usd}]),
                vec![FinancialPlanFundingDependency {
                    producing_step_id: "buy".into(),
                    resource: ResourceId(ACCOUNT.into()),
                    asset: btc,
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
                .chain(&s.intent.debits)
                .cloned()
                .collect(),
            maximum_costs: aggregate_cost_bounds(
                b.intent
                    .maximum_costs
                    .iter()
                    .chain(&s.intent.maximum_costs)
                    .cloned(),
            ),
        },
    };
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let request = FinancialPlanAuthorizationRequest {
        request_id: "synthetic-coinbase-plan".into(),
        plan: plan.clone(),
        valid_for_seconds: Some(1800),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
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
                "synthetic-coinbase-plan",
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
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    let plan_id = authorized["plan_id"].as_str().unwrap().to_owned();
    let mut saved = Vec::new();
    for (step, operation, input) in [
        ("buy", "coinbase.create-order", buy),
        ("sell", "coinbase.create-order", sell),
    ] {
        let id = format!("request-{step}");
        let request = OperationRequest {
            request_id: id.clone(),
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
            &id,
            "--plan",
            &plan_id,
            "--step",
            step,
            "--json",
        ]);
        let call = BrokerRequest::ExecutePlanStep {
            plan_id: plan_id.clone(),
            step_id: step.into(),
            request,
        };
        let result = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(result["status"], "completed", "{result}");
        let record: OperationRecord = serde_json::from_value(result.clone()).unwrap();
        assert_eq!(record.operation, "coinbase.create-order");
        assert!(record.submission.is_some());
        saved.push((argv, call, result));
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    drop(client);
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    for (argv, call, prior) in &saved {
        let replay = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(replay["id"], prior["id"]);
        assert_eq!(replay["submission"], prior["submission"]);
    }
    let (original_argv, original_call, _) = &saved[0];
    let mut conflicting_call = original_call.clone();
    if let BrokerRequest::ExecutePlanStep { request, .. } = &mut conflicting_call {
        request.input["maximumFee"] = json!("0.09");
    } else {
        unreachable!("saved request is a named-plan operation")
    }
    let mut conflicting_argv = original_argv.clone();
    conflicting_argv[3] = match &conflicting_call {
        BrokerRequest::ExecutePlanStep { request, .. } => request.input.to_string(),
        _ => unreachable!("saved request is a named-plan operation"),
    };
    let conflict = client.call(conflicting_argv, conflicting_call, false).await;
    assert_eq!(
        conflict.get("error").unwrap_or(&conflict)["code"],
        "REQUEST_ID_CONFLICT",
        "{conflict}"
    );
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        2,
        "restart replay must not send another Coinbase order"
    );
    assert_eq!(
        approval.0.load(Ordering::SeqCst),
        1,
        "the approved plan remains reusable for its named steps"
    );
}

async fn rehearse_native_rejection_releases_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-native-rejection.sqlite");
    let venue = Arc::new(Venue::default());
    venue.native_create_rejection.store(true, Ordering::SeqCst);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let adapter = adapter(venue.clone());
    let input = json!({"account":"synthetic-portfolio","requestId":"native-rejection-buy","productId":"BTC-USD","side":"BUY","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"100.00","post_only":false}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"});
    let prepared = adapter
        .prepare("coinbase.create-order", &input)
        .await
        .unwrap();
    let step = exact_step("buy", &prepared, input.clone(), json!([]), vec![]);
    let plan = FinancialPlanSpec {
        version: 1,
        name: "Coinbase native rejection proof".into(),
        summary: None,
        steps: vec![step],
        aggregate_bounds: FinancialPlanBounds {
            maximum_debits: prepared.intent.debits.clone(),
            maximum_costs: prepared.intent.maximum_costs.clone(),
        },
    };
    let runtime = runtime(&path, venue.clone(), approval);
    let mut auth = FinancialPlanAuthorizationRequest {
        request_id: "native-rejection-plan".into(),
        plan,
        valid_for_seconds: Some(900),
        replaces_revision_id: None,
        expected_review_hash: None,
    };
    let preview = runtime
        .handle(BrokerRequest::PreviewPlan {
            request: auth.clone(),
        })
        .await;
    assert!(preview.ok, "plan preview failed: {:?}", preview.error);
    auth.expected_review_hash = Some(
        preview.result.unwrap()["review_hash"]
            .as_str()
            .unwrap()
            .into(),
    );
    let authorized = runtime
        .handle(BrokerRequest::AuthorizePlan { request: auth })
        .await;
    assert!(
        authorized.ok,
        "plan authorization failed: {:?}",
        authorized.error
    );
    let plan_id = authorized.result.unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let request = BrokerRequest::ExecutePlanStep {
        plan_id: plan_id.clone(),
        step_id: "buy".into(),
        request: OperationRequest {
            request_id: "native-rejection-buy".into(),
            operation: "coinbase.create-order".into(),
            input,
            run_id: None,
        },
    };
    let first = runtime.handle(request.clone()).await;
    assert!(
        first.ok,
        "execution should return a durable failed record: {:?}",
        first.error
    );
    let record = first.result.unwrap();
    assert_eq!(record["status"], "failed", "{record}");
    let effects = record["effects"]["effects"]
        .as_array()
        .expect("verified effects");
    assert!(
        effects.is_empty(),
        "a native rejection proves no financial effects"
    );
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    let operation_id = record["id"].as_str().unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(store.reservation_count(operation_id).unwrap(), 0);
    let persisted: OperationRecord =
        serde_json::from_value(store.get_record(operation_id).unwrap().unwrap()).unwrap();
    assert_eq!(persisted.status, Status::Failed);
    assert!(persisted.effects.as_ref().unwrap().effects.is_empty());
    assert_eq!(
        venue
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(route, _)| matches!(route, CoinbaseRoute::HistoricalOrders { .. }))
            .count(),
        0,
        "the explicit rejection receipt resolves without history lookup"
    );
    let replay = runtime.handle(request).await;
    assert!(
        replay.ok,
        "idempotent status replay failed: {:?}",
        replay.error
    );
    assert_eq!(replay.result.unwrap()["id"], operation_id);
    assert_eq!(
        venue.writes.load(Ordering::SeqCst),
        1,
        "terminal replay cannot resend the native order"
    );
}

#[test]
fn native_create_order_rejection_releases_store_reservation_without_replay() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(rehearse_native_rejection_releases_reservation())
        })
        .unwrap()
        .join()
        .unwrap();
}
