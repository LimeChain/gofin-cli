//! Coinbase spot buy-and-reverse named-plan rehearsal through Engine, Store,
//! the protocol socket, and optionally the public fin-dev CLI binary.
use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, NetworkConfig, RuntimeDependencies, SharedUsdValuator,
    compose_runtime_with_clock,
};
use gofin_coinbase::{CoinbaseAccount, CoinbaseProduct, CoinbaseProfile};
use gofin_domain::{clock::Clock, *};
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

fn decimal_micros(value: &str) -> i128 {
    Quantity::from_decimal(value, 6)
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
}
fn format_micros(value: i128) -> String {
    Quantity::new(value.to_string()).unwrap().to_decimal(6)
}

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
        if challenge.binding.get("plan").is_some() {
            assert!(challenge.binding["plan"]["steps"].is_array());
        } else {
            assert!(challenge.binding.is_object());
        }
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
                "USD" | "USDC" | "22222222-2222-4222-8222-222222222222" | "u" => "1",
                "EUR" => "1.10",
                "BTC" | "11111111-1111-4111-8111-111111111111" | "b" => "100",
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
    lose_first_order: std::sync::atomic::AtomicBool,
    progressive_settlement: std::sync::atomic::AtomicBool,
    progressive_order_reads: AtomicUsize,
    progressive_fill_reads: AtomicUsize,
    btc_eur_fok_path: std::sync::atomic::AtomicBool,
    btc_eur_preview_calls: AtomicUsize,
    btc_eur_preview_ids: Mutex<Vec<String>>,
    international_lifecycle: std::sync::atomic::AtomicBool,
    exchange_lifecycle: std::sync::atomic::AtomicBool,
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
        if self.international_lifecycle.load(Ordering::SeqCst) {
            match &endpoint {
                CoinbaseRoute::NativeRead { .. } => {
                    let path = endpoint.path().unwrap();
                    if path.contains("instruments") {
                        return Self::response(
                            json!({"instrument_id":"i","instrument_uuid":"i","symbol":"BTC-USDC","type":"SPOT","mode":"STANDARD","base_asset_id":"b","base_asset_uuid":"11111111-1111-4111-8111-111111111111","base_asset_name":"BTC","quote_asset_id":"u","quote_asset_uuid":"22222222-2222-4222-8222-222222222222","quote_asset_name":"USDC","base_increment":"0.001","quote_increment":"0.01","min_quantity":"0.001","trading_state":"TRADING"}),
                        );
                    }
                    if path.contains("fee-rates") {
                        return Self::response(
                            json!([{"instrument_type":"SPOT","maker_fee_rate":"0.001","taker_fee_rate":"0.002"}]),
                        );
                    }
                    if path.contains("balances") {
                        let balances = self.exchange_balances.lock().unwrap();
                        let holding = self
                            .exchange_orders
                            .lock()
                            .unwrap()
                            .values()
                            .any(|o| o["order_status"] == "WORKING");
                        return Self::response(
                            json!([{"asset_id":"b","asset_uuid":"11111111-1111-4111-8111-111111111111","asset_name":"BTC","quantity":balances["BTC"],"hold":if holding {"0.05"} else {"0"},"transfer_hold":"0","pledged_collateral_quantity":"0","loan":"0"},{"asset_id":"u","asset_uuid":"22222222-2222-4222-8222-222222222222","asset_name":"USDC","quantity":balances["USDC"],"hold":"0","transfer_hold":"0","pledged_collateral_quantity":"0","loan":"0"}]),
                        );
                    }
                    panic!("unexpected International read {path}");
                }
                CoinbaseRoute::InternationalOrders => {
                    let body: Value =
                        serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
                    let size = body["size"].as_str().unwrap();
                    let price = body["price"].as_str().unwrap();
                    let side = body["side"].as_str().unwrap();
                    let qty = decimal_micros(size);
                    let value = qty * decimal_micros(price) / 1_000_000;
                    let fee = value * 2 / 1000;
                    let resting = price == "120";
                    let id = format!("order-{}", self.writes.load(Ordering::SeqCst));
                    let order = json!({"order_id":id,"client_order_id":body["client_order_id"],"portfolio_uuid":"p","instrument_uuid":"i","symbol":"BTC-USDC","side":side,"type":"LIMIT","tif":body["tif"],"price":price,"size":size,"order_status":if resting {"WORKING"} else {"DONE"},"exec_qty":if resting {"0"} else {size},"fee":if resting {"0".to_owned()} else {format_micros(fee)},"leaves_qty":if resting {size} else {"0"}});
                    self.exchange_orders
                        .lock()
                        .unwrap()
                        .insert(id, order.clone());
                    if !resting {
                        let mut balances = self.exchange_balances.lock().unwrap();
                        let btc = decimal_micros(&balances["BTC"]);
                        let cash = decimal_micros(&balances["USDC"]);
                        let (btc, cash) = if side == "BUY" {
                            (btc + qty, cash - value - fee)
                        } else {
                            (btc - qty, cash + value - fee)
                        };
                        assert!(btc >= 0 && cash >= 0);
                        balances.insert("BTC".into(), format_micros(btc));
                        balances.insert("USDC".into(), format_micros(cash));
                    }
                    return Self::response(order);
                }
                CoinbaseRoute::InternationalOrdersFiltered { query } => {
                    let orders = self.exchange_orders.lock().unwrap();
                    let rows: Vec<_> = orders
                        .values()
                        .filter(|o| {
                            query
                                .client_order_id
                                .as_deref()
                                .is_none_or(|id| o["client_order_id"] == id)
                        })
                        .cloned()
                        .collect();
                    return Self::response(json!({"results":rows}));
                }
                CoinbaseRoute::InternationalOrderScoped { order_id, .. } => {
                    return Self::response(self.exchange_orders.lock().unwrap()[order_id].clone());
                }
                CoinbaseRoute::InternationalCancelOrderScoped { order_id, .. } => {
                    let mut orders = self.exchange_orders.lock().unwrap();
                    let order = orders.get_mut(order_id).unwrap();
                    assert_eq!(order["order_status"], "WORKING");
                    order["order_status"] = json!("DONE");
                    order["leaves_qty"] = json!("0");
                    return Self::response(order.clone());
                }
                CoinbaseRoute::InternationalPortfolioFillsFiltered { query, .. } => {
                    let orders = self.exchange_orders.lock().unwrap();
                    let rows:Vec<_> = orders.values().filter(|o| o["exec_qty"] != "0" && query.order_id.as_deref().is_none_or(|id| o["order_id"] == id) && query.client_order_id.as_deref().is_none_or(|id| o["client_order_id"] == id)).map(|o| json!({"portfolio_uuid":"p","fill_id":format!("fill-{}",o["order_id"].as_str().unwrap()),"order_id":o["order_id"],"instrument_uuid":"i","symbol":"BTC-USDC","client_order_id":o["client_order_id"],"fill_price":o["price"],"fill_qty":o["exec_qty"],"side":o["side"],"fee":o["fee"],"fee_asset":"USDC"})).collect();
                    return Self::response(json!({"results":rows}));
                }
                _ => panic!("unexpected International endpoint {endpoint:?}"),
            }
        }
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
                if self.lose_first_order.swap(false, Ordering::SeqCst) {
                    return Err(CoinbaseError::Transport);
                }
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
                        let total = balances.get(currency).cloned().unwrap_or_else(|| "0".into());
                        let held = if currency == "BTC" && self.exchange_lifecycle.load(Ordering::SeqCst) && self.exchange_orders.lock().unwrap().values().any(|o| o["status"] == "open") { "0.05" } else { "0" };
                        let available = if held == "0.05" { "0".to_owned() } else { total.clone() };
                        json!({"id":format!("{currency}-account"),"profile_id":"synthetic-exchange-profile","currency":currency,"available":available,"hold":held,"balance":total})
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
                if self.exchange_lifecycle.load(Ordering::SeqCst) {
                    assert_eq!(product_id, "BTC-USDC");
                    let qty = decimal_micros(size);
                    let price_micros = decimal_micros(price);
                    let value = qty * price_micros / 1_000_000;
                    let fee = value * 2 / 1000;
                    let resting = price_micros == 120_000_000;
                    let id = format!("exchange-order-{}", self.writes.load(Ordering::SeqCst));
                    let order = json!({"id":id,"profile_id":"synthetic-exchange-profile","client_oid":body["client_oid"],"product_id":product_id,"side":side,"type":"limit","time_in_force":body["time_in_force"],"price":price,"size":size,"status":if resting {"open"} else {"done"},"filled_size":if resting {"0".to_owned()} else {size.to_owned()},"executed_value":if resting {"0".to_owned()} else {format_micros(value)},"fill_fees":if resting {"0".to_owned()} else {format_micros(fee)},"settled":!resting});
                    self.exchange_orders
                        .lock()
                        .unwrap()
                        .insert(id, order.clone());
                    if !resting {
                        let mut balances = self.exchange_balances.lock().unwrap();
                        let btc = decimal_micros(&balances["BTC"]);
                        let cash = decimal_micros(&balances["USDC"]);
                        let (btc, cash) = if side == "buy" {
                            (btc + qty, cash - value - fee)
                        } else {
                            (btc - qty, cash + value - fee)
                        };
                        assert!(btc >= 0 && cash >= 0);
                        balances.insert("BTC".into(), format_micros(btc));
                        balances.insert("USDC".into(), format_micros(cash));
                    }
                    return Self::response(order);
                }
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
            CoinbaseRoute::ExchangeCancelOrderScoped { order_id, .. }
                if self.exchange_lifecycle.load(Ordering::SeqCst) =>
            {
                let mut orders = self.exchange_orders.lock().unwrap();
                let order = orders.get_mut(&order_id).unwrap();
                assert_eq!(order["status"], "open");
                order["status"] = json!("canceled");
                order["done_reason"] = json!("canceled");
                order["settled"] = json!(true);
                Self::response(json!(order_id))
            }
            CoinbaseRoute::ExchangeFillsFiltered { query } => {
                let orders = self.exchange_orders.lock().unwrap();
                let order = orders
                    .values()
                    .find(|order| order["id"] == query.order_id.clone().unwrap_or_default())
                    .cloned()
                    .ok_or(CoinbaseError::Transport)?;
                if self.exchange_lifecycle.load(Ordering::SeqCst) {
                    if order["filled_size"] == "0" {
                        return Self::response(json!([]));
                    }
                    return Self::response(
                        json!([{"trade_id":format!("fill-{}", order["id"].as_str().unwrap()),"order_id":order["id"],"profile_id":"synthetic-exchange-profile","product_id":order["product_id"],"side":order["side"],"size":order["filled_size"],"price":order["price"],"fee":order["fill_fees"],"settled":true}]),
                    );
                }
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
fn international_adapter(venue: Arc<Venue>) -> Arc<ExchangeAdapter<CoinbaseProfile>> {
    let api: Arc<dyn CoinbaseApi> = venue;
    Arc::new(ExchangeAdapter::new(
        CoinbaseProfile::new(
            vec![CoinbaseAccount {
                name: "x".into(),
                resource: ResourceId("coinbaseinternational:portfolio:p".into()),
                credential_ref: "key".into(),
                physical_identity: "client:portfolio:p".into(),
                identity_verified: true,
                portfolio_id: Some("p".into()),
                product: CoinbaseProduct::International,
            }],
            [("key".into(), api)].into(),
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    ))
}
fn international_runtime(
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
            adapters: vec![international_adapter(venue)],
            authorizer: approval,
            valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
            bridge: None,
        },
        Arc::new(TestClock),
    )
    .unwrap()
}
include!("support/coinbase_plan_client.rs");

#[test]
fn coinbase_standalone_buy_then_reverse_with_two_approvals_and_restart_identity() {
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
fn retired_movement_cap_does_not_block_native_bounded_action() {
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
fn coinbase_exchange_sell_funds_a_later_standalone_order() {
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
    let sell = json!({"account":"synthetic-exchange","requestId":"exchange-sell-hbar","productId":"HBAR-USDC","side":"SELL","baseSize":"25","limitPrice":"0.10","timeInForce":"FOK","maximumSpend":"25","minimumReceive":"2.49","maximumFee":"0.01"});
    let buy = json!({"account":"synthetic-exchange","requestId":"exchange-buy-btc","productId":"BTC-USDC","side":"BUY","baseSize":"0.023","limitPrice":"100","timeInForce":"FOK","maximumSpend":"2.3","minimumReceive":"0.022","maximumFee":"0.01"});
    let client = Client::new(
        exchange_runtime(&path, venue.clone(), approval.clone()),
        temp.path(),
    )
    .await;
    assert_eq!(
        venue.exchange_balances.lock().unwrap().get("USDC").unwrap(),
        "0"
    );
    for (_step, operation_input) in [("sell-hbar", sell), ("buy-btc", buy)] {
        let result = client
            .call(
                args(&[
                    "execute",
                    "coinbaseexchange.create-order",
                    "--input",
                    &operation_input.to_string(),
                    "--id",
                    operation_input["requestId"].as_str().unwrap(),
                    "--json",
                ]),
                BrokerRequest::Execute {
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
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
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
    let buy = json!({"account":"synthetic-portfolio","requestId":"btc-eur-fok-buy","productId":"BTC-EUR","side":"BUY","orderConfiguration":{"limit_limit_fok":{"base_size":"0.001","limit_price":"1990.00"}},"maximumSpend":"1.99","minimumReceive":"0.00099","maximumFee":"0.01"});
    let sell = json!({"account":"synthetic-portfolio","requestId":"btc-eur-fok-sell","productId":"BTC-EUR","side":"SELL","orderConfiguration":{"limit_limit_fok":{"base_size":"0.001","limit_price":"2010.00"}},"maximumSpend":"0.001","minimumReceive":"2.00","maximumFee":"0.01"});
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let mut saved = Vec::new();
    for (_step, request_id, input) in [
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
            "--json",
        ]);
        let call = BrokerRequest::Execute { request };
        let result = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(result["status"], "completed", "{result}");
        assert!(result["submission"].is_object());
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
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
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
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
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
    let sell_hbar = json!({"account":"synthetic-portfolio","requestId":"sell-hbar","productId":"HBAR-USDC","side":"SELL","orderConfiguration":{"market_market_ioc":{"base_size":"25"}},"maximumSpend":"25","minimumReceive":"2.40","maximumFee":"0.10"});
    let buy_btc = json!({"account":"synthetic-portfolio","requestId":"buy-btc","productId":"BTC-USDC","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"2.30"}},"maximumSpend":"2.30","minimumReceive":"0.022","maximumFee":"0.10"});
    let sell_btc = json!({"account":"synthetic-portfolio","requestId":"sell-btc","productId":"BTC-USDC","side":"SELL","orderConfiguration":{"market_market_ioc":{"base_size":"0.023"}},"maximumSpend":"0.023","minimumReceive":"2.20","maximumFee":"0.10"});
    let buy_hbar = json!({"account":"synthetic-portfolio","requestId":"buy-hbar","productId":"HBAR-USDC","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"2.10"}},"maximumSpend":"2.10","minimumReceive":"20","maximumFee":"0.10"});
    let inputs = vec![sell_hbar, buy_btc, sell_btc, buy_hbar];
    venue.native_funding_errors.store(true, Ordering::SeqCst);
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    for (_step_id, input) in ["sell-hbar", "buy-btc", "sell-btc", "buy-hbar"]
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
                    "--json",
                ]),
                BrokerRequest::Execute {
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
    assert_eq!(approval.0.load(Ordering::SeqCst), 4);
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
    let input = json!({"account":"synthetic-portfolio","requestId":"progressive-order","productId":"BTC-USD","side":"BUY","orderConfiguration":{"market_market_ioc":{"quote_size":"50"}},"maximumSpend":"50","minimumReceive":"0.49","maximumFee":"0.20"});
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    let run_step = || BrokerRequest::Execute {
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
                "--json",
            ]),
            run_step(),
            true,
        )
        .await;
    assert_eq!(first["status"], "pending", "{first}");
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        first["effects"]["evidence"]["settlement_progress"]["sequence"],
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
    let verified: gofin_domain::VerifiedEffects =
        serde_json::from_value(second["effects"].clone()).unwrap();
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
        .get_record(second["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        stored["effects"]["evidence"]["settlement_progress"]["sequence"],
        2
    );
    assert_eq!(
        stored["submission"]["reference"],
        first["submission"]["reference"]
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
    assert!(
        prepared
            .intent
            .maximum_costs
            .iter()
            .any(|cost| !cost.amount.atoms.is_zero())
    );
    let runtime = runtime_with_policy(
        &temp.path().join("default-cap.sqlite"),
        venue.clone(),
        approval.clone(),
        BrokerPolicy::beta(),
    );
    let executed = runtime
        .handle(BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "default-cap-buy".into(),
                operation: "coinbase.create-order".into(),
                input,
                run_id: None,
            },
        })
        .await;
    assert!(
        executed.ok,
        "standalone action returns a durable record: {:?}",
        executed.error
    );
    let record = executed.result.unwrap();
    assert_ne!(record["error"]["code"], "MOVEMENT_LIMIT", "{record}");
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[ignore = "requires the explicitly built synthetic Coinbase development CLI"]
async fn missing_coinbase_order_stays_uncertain_across_restart() {
    assert!(std::env::var_os("GOFIN_COINBASE_PLAN_CLI").is_some());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing-order.sqlite");
    let venue = Arc::new(Venue::default());
    venue.lose_first_order.store(true, Ordering::SeqCst);
    venue.exchange_lifecycle.store(true, Ordering::SeqCst);
    venue
        .exchange_balances
        .lock()
        .unwrap()
        .extend([("USDC".into(), "100".into()), ("BTC".into(), "0".into())]);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let make_runtime = || {
        let transport: Arc<dyn CoinbaseApi> = venue.clone();
        let profile = CoinbaseProfile::new(
            vec![
                CoinbaseAccount {
                    name: "synthetic-portfolio".into(),
                    resource: ResourceId(ACCOUNT.into()),
                    credential_ref: "synthetic-credential".into(),
                    physical_identity: "synthetic-portfolio-id".into(),
                    identity_verified: true,
                    portfolio_id: Some("synthetic-portfolio-id".into()),
                    product: CoinbaseProduct::Advanced,
                },
                CoinbaseAccount {
                    name: "synthetic-exchange".into(),
                    resource: ResourceId(EXCHANGE_ACCOUNT.into()),
                    credential_ref: "synthetic-exchange-credential".into(),
                    physical_identity: "synthetic-exchange-profile".into(),
                    identity_verified: true,
                    portfolio_id: None,
                    product: CoinbaseProduct::Exchange,
                },
            ],
            BTreeMap::from([
                ("synthetic-credential".into(), transport.clone()),
                ("synthetic-exchange-credential".into(), transport),
            ]),
        )
        .unwrap()
        .with_prices(Arc::new(Prices));
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
                store: Store::open(&path).unwrap(),
                adapters: vec![Arc::new(ExchangeAdapter::new(profile))],
                authorizer: approval.clone(),
                valuator: Arc::new(SharedUsdValuator::with_source(Arc::new(Prices))),
                bridge: None,
            },
            Arc::new(TestClock),
        )
        .unwrap()
    };
    let client = Client::new(make_runtime(), temp.path()).await;
    let id = "missing-coinbase";
    let input = json!({"account":"synthetic-portfolio","requestId":id,"productId":"BTC-USD","side":"BUY","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"100.00","post_only":false}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"});
    let argv = args(&[
        "execute",
        "coinbase.create-order",
        "--input",
        &input.to_string(),
        "--id",
        id,
        "--json",
    ]);
    let request = BrokerRequest::Execute {
        request: OperationRequest {
            request_id: id.into(),
            operation: "coinbase.create-order".into(),
            input: input.clone(),
            run_id: None,
        },
    };
    let original = client.call(argv.clone(), request.clone(), true).await;
    assert!(original["submission"].is_object(), "{original}");
    assert_eq!(original["outcome"]["replay_safe"], false);
    let mut probe_input = input;
    probe_input["requestId"] = json!("coinbase-probe");
    let probe = client
        .call(
            args(&[
                "execute",
                "coinbase.create-order",
                "--input",
                &probe_input.to_string(),
                "--id",
                "coinbase-probe",
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: "coinbase-probe".into(),
                    operation: "coinbase.create-order".into(),
                    input: probe_input,
                    run_id: None,
                },
            },
            true,
        )
        .await;
    assert!(probe["submission"].is_object(), "{probe}");
    let other = json!({"account":"synthetic-exchange","requestId":"coinbase-other","productId":"BTC-USDC","side":"BUY","baseSize":"0.1","limitPrice":"100","timeInForce":"FOK","maximumSpend":"10","minimumReceive":"0.1","maximumFee":"0.03"});
    let different = client
        .call(
            args(&[
                "execute",
                "coinbaseexchange.create-order",
                "--input",
                &other.to_string(),
                "--id",
                "coinbase-other",
                "--json",
            ]),
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: "coinbase-other".into(),
                    operation: "coinbaseexchange.create-order".into(),
                    input: other,
                    run_id: None,
                },
            },
            true,
        )
        .await;
    assert!(different["submission"].is_object(), "{different}");
    drop(client);
    let client = Client::new(make_runtime(), temp.path()).await;
    let repeated = client.call(argv, request, true).await;
    assert_eq!(repeated["id"], original["id"]);
    assert_eq!(repeated["submission"], original["submission"]);
    assert_eq!(repeated["outcome"]["replay_safe"], false);
    assert_eq!(venue.writes.load(Ordering::SeqCst), 3);
    assert_eq!(approval.0.load(Ordering::SeqCst), 3);
    if let Ok(path) = std::env::var("GOFIN_OUTCOME_EVIDENCE") {
        std::fs::write(path,serde_json::to_vec_pretty(&json!({"scenario":"outcome.a13.coinbase","approval_count":1,"external_submission_count":1,"probe_approval_count":2,"probe_submission_count":2,"intervention_count":0,"remaining_external_uncertainty":true,"blocked_unrelated_activity":false,"different_account_probed":true})).unwrap()).unwrap();
    }
}
async fn rehearse() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-plan.sqlite");
    let venue = Arc::new(Venue::default());
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let buy = json!({"account":"synthetic-portfolio","requestId":"plan-buy","productId":"BTC-USD","side":"BUY","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"100.00","post_only":false}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"});
    let sell = json!({"account":"synthetic-portfolio","requestId":"plan-sell","productId":"BTC-USD","side":"SELL","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"110.00","post_only":false}},"maximumSpend":"1.00","minimumReceive":"100.00","maximumFee":"0.10"});
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
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
            "--json",
        ]);
        let call = BrokerRequest::Execute { request };
        let result = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(result["operation"], "coinbase.create-order");
        assert!(result["submission"].is_object());
        saved.push((argv, call, result));
    }
    assert_eq!(venue.writes.load(Ordering::SeqCst), 2);
    assert_eq!(approval.0.load(Ordering::SeqCst), 2);
    drop(client);
    let client = Client::new(runtime(&path, venue.clone(), approval.clone()), temp.path()).await;
    for (argv, call, prior) in &saved {
        let replay = client.call(argv.clone(), call.clone(), true).await;
        assert_eq!(replay["id"], prior["id"]);
        assert_eq!(replay["submission"], prior["submission"]);
    }
    let (original_argv, original_call, _) = &saved[0];
    let mut conflicting_call = original_call.clone();
    if let BrokerRequest::Execute { request } = &mut conflicting_call {
        request.input["maximumFee"] = json!("0.09");
    } else {
        unreachable!("saved request is a standalone operation")
    }
    let mut conflicting_argv = original_argv.clone();
    conflicting_argv[3] = match &conflicting_call {
        BrokerRequest::Execute { request } => request.input.to_string(),
        _ => unreachable!("saved request is a standalone operation"),
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
        2,
        "idempotent replay does not prompt again"
    );
}

async fn rehearse_native_rejection_releases_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("coinbase-native-rejection.sqlite");
    let venue = Arc::new(Venue::default());
    venue.native_create_rejection.store(true, Ordering::SeqCst);
    let approval = Arc::new(Approval(AtomicUsize::new(0)));
    let input = json!({"account":"synthetic-portfolio","requestId":"native-rejection-buy","productId":"BTC-USD","side":"BUY","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"100.00","post_only":false}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"});
    let runtime = runtime(&path, venue.clone(), approval);
    let request = BrokerRequest::Execute {
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
fn native_create_order_rejection_is_proven_without_replay() {
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

#[cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;

#[test]
#[ignore = "requires GOFIN_COINBASE_PLAN_CLI built with synthetic-coinbase-build"]
fn positive_coinbase_exchange_order_lifecycle_via_cli() {
    let binary = std::env::var_os("GOFIN_COINBASE_PLAN_CLI").expect("public CLI required");
    assert!(
        std::path::Path::new(&binary).is_file(),
        "public CLI missing"
    );
    std::thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_stack_size(8 * 1024 * 1024).enable_all().build().unwrap().block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let venue = Arc::new(Venue::default());
            venue.exchange_lifecycle.store(true, Ordering::SeqCst);
            venue.exchange_balances.lock().unwrap().extend([("USDC".into(),"100".into()), ("BTC".into(),"0".into())]);
            let approval = Arc::new(Approval(AtomicUsize::new(0)));
            let adapter = exchange_adapter(venue.clone());
            let client = Client::new(exchange_runtime(&temp.path().join("exchange.sqlite"), venue.clone(), approval.clone()), temp.path()).await;
            for (id, side, size, price, tif, expected) in [
                ("buy", "BUY", "0.1", "100", "FOK", "completed"),
                ("sell-part", "SELL", "0.05", "100", "FOK", "completed"),
                ("rest", "SELL", "0.05", "120", "GTC", "pending"),
                ("sell-rest", "SELL", "0.05", "100", "FOK", "completed"),
            ] {
                let input = json!({"account":"synthetic-exchange", "requestId":id,"productId":"BTC-USDC", "side":side,"baseSize":size,"limitPrice":price,"timeInForce":tif,"maximumSpend":if side == "BUY" {"10"} else {size},"minimumReceive":if side == "BUY" {size} else {"4.9"},"maximumFee":"0.03"});
                let prepared = adapter.prepare("coinbaseexchange.create-order", &input).await.unwrap();
                #[cfg(target_os = "macos")]
                native_approval::assert_prepared_renders(&prepared);
                let request = OperationRequest {request_id:id.into(), operation:"coinbaseexchange.create-order".into(), input:input.clone(), run_id:None};
                let result = client.call(args(&["execute", "coinbaseexchange.create-order", "--input", &input.to_string(), "--id", id, "--json"]), BrokerRequest::Execute {request}, true).await;
                assert_eq!(result["status"], expected, "{result}");
                if id == "rest" {
                    let order = venue.exchange_orders.lock().unwrap().values().find(|o| o["status"] == "open").unwrap().clone();
                    assert_eq!(venue.exchange_balances.lock().unwrap()["BTC"], "0.05");
                    let accounts: Value = serde_json::from_slice(&venue.request(CoinbaseRoute::ExchangeAccounts, CoinbaseRequest::get(), None).await.unwrap().body).unwrap();
                    let btc = accounts.as_array().unwrap().iter().find(|a| a["currency"] == "BTC").unwrap();
                    assert_eq!(btc["hold"], "0.05");
                    assert_eq!(btc["available"], "0");
                    let cancel = json!({"account":"synthetic-exchange","requestId":"cancel","orderId":order["id"],"clientOrderId":order["client_oid"],"productId":"BTC-USDC"});
                    let cancel_preview = client.call(args(&["preview", "coinbaseexchange.cancel-order", "--input", &cancel.to_string(), "--json"]), BrokerRequest::Preview { operation:"coinbaseexchange.cancel-order".into(), input:cancel.clone() }, true).await;
                    let cancel_prepared: PreparedOperation = serde_json::from_value(cancel_preview).unwrap();
                    #[cfg(target_os = "macos")]
                    native_approval::assert_prepared_renders(&cancel_prepared);
                    let request = OperationRequest {request_id:"cancel".into(), operation:"coinbaseexchange.cancel-order".into(), input:cancel.clone(), run_id:None};
                    let canceled = client.call(args(&["execute", "coinbaseexchange.cancel-order", "--input", &cancel.to_string(), "--id", "cancel", "--json"]), BrokerRequest::Execute {request}, true).await;
                    assert_eq!(canceled["status"], "completed", "{canceled}");
                    let parent_id = result["id"].as_str().unwrap();
                    let reconciled = client.call(args(&["status", parent_id, "--json"]), BrokerRequest::Status {id:parent_id.into()}, true).await;
                    assert!(reconciled["status"] == "failed" || reconciled["status"] == "completed", "{reconciled}");
                    assert_ne!(venue.exchange_orders.lock().unwrap()[order["id"].as_str().unwrap()]["status"], "open");
                    let accounts: Value = serde_json::from_slice(&venue.request(CoinbaseRoute::ExchangeAccounts, CoinbaseRequest::get(), None).await.unwrap().body).unwrap();
                    let btc = accounts.as_array().unwrap().iter().find(|a| a["currency"] == "BTC").unwrap();
                    assert_eq!(btc["hold"], "0");
                    assert_eq!(btc["available"], "0.05");
                }
            }
            let balances = venue.exchange_balances.lock().unwrap();
            assert_eq!(balances["BTC"], "0");
            assert_eq!(balances["USDC"], "99.96");
            assert_eq!(venue.writes.load(Ordering::SeqCst), 5);
            assert_eq!(approval.0.load(Ordering::SeqCst), 5);
        });
    }).unwrap().join().unwrap();
}

#[test]
#[ignore = "requires GOFIN_COINBASE_PLAN_CLI built with synthetic-coinbase-build"]
fn positive_coinbase_international_order_lifecycle_via_cli() {
    let binary = std::env::var_os("GOFIN_COINBASE_PLAN_CLI").expect("public CLI required");
    assert!(
        std::path::Path::new(&binary).is_file(),
        "public CLI missing"
    );
    std::thread::Builder::new().stack_size(8 * 1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_stack_size(8 * 1024 * 1024).enable_all().build().unwrap().block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let venue = Arc::new(Venue::default());
            venue.international_lifecycle.store(true, Ordering::SeqCst);
            venue.exchange_balances.lock().unwrap().extend([("USDC".into(),"100".into()), ("BTC".into(),"0".into())]);
            let approval = Arc::new(Approval(AtomicUsize::new(0)));
            let adapter = international_adapter(venue.clone());
            let client = Client::new(international_runtime(&temp.path().join("international.sqlite"), venue.clone(), approval.clone()), temp.path()).await;
            for (id, side, size, price, tif, expected) in [
                ("buy", "BUY", "0.1", "100", "FOK", "completed"),
                ("sell-part", "SELL", "0.05", "100", "FOK", "completed"),
                ("rest", "SELL", "0.05", "120", "GTC", "pending"),
                ("sell-rest", "SELL", "0.05", "100", "FOK", "completed"),
            ] {
                let input = json!({"account":"x", "requestId":id,"instrument":"BTC-USDC", "side":side,"baseSize":size,"limitPrice":price,"timeInForce":tif,"maximumSpend":if side == "BUY" {"10"} else {size},"minimumReceive":if side == "BUY" {size} else {"4.9"},"maximumFee":"0.03"});
                let prepared = adapter.prepare("coinbaseinternational.create-order", &input).await.unwrap();
                #[cfg(target_os = "macos")]
                {
                    let reason = native_approval::assert_prepared_renders(&prepared);
                    for term in [side, price, size, "BTC", "USDC", tif] {
                        assert!(reason.contains(term), "review omitted {term}: {reason}");
                    }
                }
                let request = OperationRequest {request_id:id.into(), operation:"coinbaseinternational.create-order".into(), input:input.clone(), run_id:None};
                let result = client.call(args(&["execute", "coinbaseinternational.create-order", "--input", &input.to_string(), "--id", id, "--json"]), BrokerRequest::Execute {request}, true).await;
                assert_eq!(result["status"], expected, "{result}");
                if id == "rest" {
                    let order = venue.exchange_orders.lock().unwrap().values().find(|o| o["order_status"] == "WORKING").unwrap().clone();
                    assert_eq!(venue.exchange_balances.lock().unwrap()["BTC"], "0.05");
                    let cancel = json!({"account":"x","requestId":"cancel","orderId":order["order_id"],"clientOrderId":order["client_order_id"],"instrument":"BTC-USDC"});
                    let cancel_preview = client.call(args(&["preview", "coinbaseinternational.cancel-order", "--input", &cancel.to_string(), "--json"]), BrokerRequest::Preview { operation:"coinbaseinternational.cancel-order".into(), input:cancel.clone() }, true).await;
                    let cancel_prepared: PreparedOperation = serde_json::from_value(cancel_preview).unwrap();
                    #[cfg(target_os = "macos")]
                    native_approval::assert_prepared_renders(&cancel_prepared);
                    let request = OperationRequest {request_id:"cancel".into(), operation:"coinbaseinternational.cancel-order".into(), input:cancel.clone(), run_id:None};
                    let canceled = client.call(args(&["execute", "coinbaseinternational.cancel-order", "--input", &cancel.to_string(), "--id", "cancel", "--json"]), BrokerRequest::Execute {request}, true).await;
                    assert_eq!(canceled["status"], "completed", "{canceled}");
                    let parent_id = result["id"].as_str().unwrap();
                    let reconciled = client.call(args(&["status", parent_id, "--json"]), BrokerRequest::Status {id:parent_id.into()}, true).await;
                    assert!(reconciled["status"] == "failed" || reconciled["status"] == "completed", "{reconciled}");
                    assert_ne!(venue.exchange_orders.lock().unwrap()[order["order_id"].as_str().unwrap()]["order_status"], "WORKING");

                }
            }
            let balances = venue.exchange_balances.lock().unwrap();
            assert_eq!(balances["BTC"], "0");
            assert_eq!(balances["USDC"], "99.96");
            assert_eq!(venue.writes.load(Ordering::SeqCst), 5);
            assert_eq!(approval.0.load(Ordering::SeqCst), 5);
        });
    }).unwrap().join().unwrap();
}

#[tokio::test]
async fn coinbase_native_order_prepare_does_not_require_reporting_price() {
    let venue = Arc::new(Venue::default());
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
    .unwrap();
    let adapter = ExchangeAdapter::new(profile);
    let input = json!({"account":"synthetic-portfolio","requestId":"unpriced-native-order","productId":"BTC-USD","side":"BUY","orderConfiguration":{"limit_limit_gtc":{"base_size":"1.00","limit_price":"100.00","post_only":false}},"maximumSpend":"100.00","minimumReceive":"0.99","maximumFee":"0.10"});
    let prepared = adapter
        .prepare("coinbase.create-order", &input)
        .await
        .unwrap();
    assert!(prepared.valuation.is_none());
    assert!(!prepared.intent.debits.is_empty());
    assert!(!prepared.intent.maximum_costs.is_empty());
    let refreshed = adapter.revalidate(&prepared).await.unwrap();
    assert_eq!(refreshed.input, prepared.input);
    assert_eq!(refreshed.intent, prepared.intent);
    assert!(refreshed.valuation.is_none());
}
