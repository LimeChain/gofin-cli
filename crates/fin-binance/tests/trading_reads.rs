use fin_binance::{
    AccountRole, BinanceAccount, BinanceConfig, BinanceEndpoint, BinanceRequest, BinanceTransport,
    CredentialHandle, TransportError,
    trading_reads::{trading_read, trading_read_definitions},
};
use fin_domain::{OperationKind, ResourceId};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};

struct Fixture {
    expected: Mutex<VecDeque<(BinanceEndpoint, Value)>>,
    seen: Mutex<Vec<(String, BinanceRequest)>>,
}
impl Fixture {
    fn new(expected: Vec<(BinanceEndpoint, Value)>) -> Self {
        Self {
            expected: Mutex::new(expected.into()),
            seen: Mutex::new(Vec::new()),
        }
    }
    fn finished(&self) {
        assert!(self.expected.lock().unwrap().is_empty());
    }
}
#[async_trait::async_trait]
impl BinanceTransport for Fixture {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        request
            .validate()
            .expect("fixture exercises a permitted finite request");
        let (endpoint, response) = self
            .expected
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected venue request");
        assert_eq!(request.endpoint, endpoint);
        let public = matches!(
            endpoint,
            BinanceEndpoint::SpotExchangeInfo | BinanceEndpoint::FuturesExchangeInfo
        );
        assert_eq!(
            credential.as_str(),
            if public {
                "public-market-data"
            } else {
                "synthetic-trading-credential"
            }
        );
        self.seen
            .lock()
            .unwrap()
            .push((credential.as_str().into(), request));
        Ok(response)
    }
}
fn config() -> BinanceConfig {
    BinanceConfig {
        accounts: vec![BinanceAccount {
            name: "main".into(),
            resource: ResourceId("binance:uid:123:SPOT".into()),
            credential: CredentialHandle::new("synthetic-trading-credential").unwrap(),
            role: AccountRole::Main,
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
        }],
        destinations: vec![],
    }
}
fn market(symbol: &str, contract: &str) -> Value {
    json!({"symbol":symbol,"status":"TRADING","baseAsset":"ETH","quoteAsset":"USDT","marginAsset":"USDT","contractType":contract,
        "filters":[{"filterType":"PRICE_FILTER","minPrice":"0.000000000000000001","maxPrice":"1000000","tickSize":"0.000000000000000001"},
        {"filterType":"LOT_SIZE","minQty":"0.001","maxQty":"10000","stepSize":"0.001"}],
        "arbitrarySecret":"must-not-be-returned"})
}
fn futures_markets() -> (BinanceEndpoint, Value) {
    (
        BinanceEndpoint::FuturesExchangeInfo,
        json!({"symbols":[market("ETHUSDT", "PERPETUAL"),market("ETHUSDT_261225", "CURRENT_QUARTER")]}),
    )
}

fn han_market() -> Value {
    let mut row = market("币安人生USDT", "PERPETUAL");
    row["baseAsset"] = json!("币安人生");
    row
}

#[tokio::test]
async fn native_han_markets_do_not_break_full_or_selected_market_discovery() {
    for futures in [false, true] {
        for selected in [false, true] {
            let endpoint = if futures {
                BinanceEndpoint::FuturesExchangeInfo
            } else {
                BinanceEndpoint::SpotExchangeInfo
            };
            let venue = Fixture::new(vec![(
                endpoint,
                json!({"symbols":[han_market(), market("ETHUSDT", "PERPETUAL")]}),
            )]);
            let operation = if futures {
                "binance.futures.markets"
            } else {
                "binance.spot.markets"
            };
            let args = if selected {
                json!({"symbol":"ETHUSDT"})
            } else {
                json!({})
            };
            let result = trading_read(operation, &args, &config(), &venue)
                .await
                .unwrap();
            assert_eq!(
                result["markets"].as_array().unwrap().len(),
                if selected { 1 } else { 2 }
            );
            if !selected {
                assert_eq!(result["markets"][0]["symbol"], "币安人生USDT");
                assert_eq!(result["markets"][0]["baseAsset"], "币安人生");
            }
            venue.finished();
        }
    }
}

#[tokio::test]
async fn futures_positions_for_a_subaccount_accept_unrelated_native_han_markets() {
    let venue = Fixture::new(vec![
        (
            BinanceEndpoint::FuturesExchangeInfo,
            json!({"symbols":[han_market(), market("ETHUSDT", "PERPETUAL")]}),
        ),
        (
            BinanceEndpoint::FuturesPositions,
            json!([{"symbol":"ETHUSDT", "positionSide":"SHORT", "marginAsset":"USDT", "positionAmt":"-0.001", "entryPrice":"1234.123456789012345678", "markPrice":"1235", "unRealizedProfit":"-0.001", "notional":"-1.235"}]),
        ),
    ]);
    let mut accounts = config();
    let mut child = accounts.accounts[0].clone();
    child.name = "synthetic-child".into();
    child.role = AccountRole::Sub;
    child.parent = Some("main".into());
    child.email = Some("synthetic-child@example.invalid".into());
    child.resource = ResourceId("binance:uid:124:SPOT".into());
    accounts.accounts.push(child);
    let result = trading_read(
        "binance.futures.positions",
        &json!({"account":"synthetic-child"}),
        &accounts,
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["account"], "synthetic-child");
    assert_eq!(result["positions"][0]["positionAmt"], "-0.001");
    assert_eq!(
        result["positions"][0]["entryPrice"],
        "1234.123456789012345678"
    );
    venue.finished();
}

#[tokio::test]
async fn native_han_symbol_selectors_are_preserved_in_reviewed_read_requests() {
    let venue = Fixture::new(vec![
        (
            BinanceEndpoint::FuturesExchangeInfo,
            json!({"symbols":[han_market()]}),
        ),
        (BinanceEndpoint::FuturesOpenOrders, json!([])),
    ]);
    let result = trading_read(
        "binance.futures.orders",
        &json!({"account":"main", "symbol":"币安人生USDT"}),
        &config(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["orders"], json!([]));
    assert_eq!(
        venue.seen.lock().unwrap()[1].1.get("symbol"),
        Some("币安人生USDT")
    );
    venue.finished();
}

#[tokio::test]
async fn control_characters_and_punctuation_remain_invalid_native_symbol_selectors() {
    for symbol in [
        "币安人生USDT&x=y",
        "币安\n人生USDT",
        "币安\u{202e}人生USDT",
        "ethusdt",
        "😀USDT",
    ] {
        let venue = Fixture::new(vec![]);
        let failure = trading_read(
            "binance.futures.orders",
            &json!({"account":"main", "symbol":symbol}),
            &config(),
            &venue,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code, "INVALID_SYMBOL");
        assert!(venue.seen.lock().unwrap().is_empty());
    }
}
fn order() -> Value {
    json!({"symbol":"ETHUSDT","orderId":9007199254740993u64,"clientOrderId":"fixture-order","status":"PARTIALLY_FILLED","side":"BUY","type":"LIMIT",
        "price":"1234.000000000000000001","origQty":"0.123456789012345678","executedQty":"0.000000000000000001",
        "cummulativeQuoteQty":"0.000000000000001234","positionSide":"BOTH","reduceOnly":false,
        "arbitrarySecret":"must-not-be-returned"})
}
fn trade() -> Value {
    json!({"symbol":"ETHUSDT","id":9007199254740997u64,"orderId":9007199254740993u64,"price":"1234.000000000000000001","qty":"0.000000000000000001",
        "quoteQty":"0.000000000000001234","commission":"0.000000000000000001","commissionAsset":"USDT","time":1790000000000u64,
        "realizedPnl":"-0.000000000000000007","side":"SELL","positionSide":"SHORT","isBuyer":false,"maker":true})
}

#[tokio::test]
async fn public_markets_need_no_accounts_and_keep_exact_filters_excluding_delivery() {
    let venue = Fixture::new(vec![futures_markets()]);
    let result = trading_read(
        "binance.futures.markets",
        &json!({}),
        &BinanceConfig::default(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["markets"].as_array().unwrap().len(), 1);
    assert_eq!(result["markets"][0]["contractType"], "PERPETUAL");
    assert_eq!(
        result["markets"][0]["filters"][0]["tickSize"],
        "0.000000000000000001"
    );
    assert!(!result.to_string().contains("must-not-be-returned"));
    assert!(venue.seen.lock().unwrap()[0].1.params.is_empty());
    venue.finished();
}

#[tokio::test]
async fn spot_markets_use_fixed_public_symbol_request() {
    let venue = Fixture::new(vec![(
        BinanceEndpoint::SpotExchangeInfo,
        json!({"symbols":[market("ETHUSDT", "")] }),
    )]);
    let result = trading_read(
        "binance.spot.markets",
        &json!({"symbol":"ETHUSDT"}),
        &BinanceConfig::default(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["marketType"], "spot");
    assert!(result["markets"][0].get("contractType").is_none());
    assert_eq!(
        venue.seen.lock().unwrap()[0].1.get("symbol"),
        Some("ETHUSDT")
    );
    venue.finished();
}

#[tokio::test]
async fn spot_order_preserves_exact_quantities_and_large_identifier_without_unknown_fields() {
    let venue = Fixture::new(vec![(BinanceEndpoint::SpotOrder, order())]);
    let result = trading_read(
        "binance.spot.order",
        &json!({"account":"main","symbol":"ETHUSDT","order_id":"9007199254740993"}),
        &config(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["order"]["orderId"], "9007199254740993");
    assert_eq!(result["order"]["price"], "1234.000000000000000001");
    assert_eq!(result["order"]["executedQty"], "0.000000000000000001");
    assert!(!result.to_string().contains("must-not-be-returned"));
    assert!(result["order"].get("positionSide").is_none());
    venue.finished();
}

#[tokio::test]
async fn open_orders_bound_output_and_report_truncation_without_sending_unsupported_limit() {
    let venue = Fixture::new(vec![(
        BinanceEndpoint::SpotOpenOrders,
        json!([order(), order()]),
    )]);
    let result = trading_read(
        "binance.spot.orders",
        &json!({"account":"main","symbol":"ETHUSDT","limit":1}),
        &config(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["orders"].as_array().unwrap().len(), 1);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["coverage"], "current_open_orders");
    assert_eq!(venue.seen.lock().unwrap()[0].1.get("limit"), None);
}

#[tokio::test]
async fn spot_and_futures_trades_use_own_routes_with_exact_fees_and_signed_pnl() {
    for (operation, endpoint, futures) in [
        ("binance.spot.trades", BinanceEndpoint::SpotTrades, false),
        (
            "binance.futures.trades",
            BinanceEndpoint::FuturesTrades,
            true,
        ),
    ] {
        let mut expected = Vec::new();
        if futures {
            expected.push(futures_markets());
        }
        expected.push((endpoint, json!([trade()])));
        let venue = Fixture::new(expected);
        let result = trading_read(
            operation,
            &json!({"account":"main","symbol":"ETHUSDT","limit":7}),
            &config(),
            &venue,
        )
        .await
        .unwrap();
        assert_eq!(result["trades"][0]["id"], "9007199254740997");
        assert_eq!(result["trades"][0]["commission"], "0.000000000000000001");
        if futures {
            assert_eq!(result["trades"][0]["realizedPnl"], "-0.000000000000000007");
        } else {
            assert!(result["trades"][0].get("realizedPnl").is_none());
        }
        assert_eq!(result["coverage"], "bounded_recent_history");
        assert_eq!(
            venue.seen.lock().unwrap().last().unwrap().1.get("limit"),
            Some("7")
        );
        venue.finished();
    }
}

#[tokio::test]
async fn futures_balances_distinguish_collateral_available_and_unrealized_pnl() {
    let venue = Fixture::new(vec![(
        BinanceEndpoint::FuturesAccount,
        json!({"assets":[{
            "asset":"USDT","walletBalance":"100.000000000000000001","unrealizedProfit":"-0.000000000000000002",
            "marginBalance":"99.999999999999999999","availableBalance":"90.0","initialMargin":"9.999999999999999999","maintMargin":"1.0"
        }]}),
    )]);
    let result = trading_read(
        "binance.futures.balances",
        &json!({"account":"main"}),
        &config(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(
        result["balances"][0]["walletBalance"],
        "100.000000000000000001"
    );
    assert_eq!(result["balances"][0]["availableBalance"], "90.0");
    assert_eq!(
        result["balances"][0]["unrealizedProfit"],
        "-0.000000000000000002"
    );
    assert!(result["balances"][0].get("free").is_none());
    assert!(result["balances"][0].get("locked").is_none());
    assert!(result["multiAssetsMargin"].is_null());
    venue.finished();
}

#[tokio::test]
async fn futures_positions_preserve_short_sign_hedge_side_and_filter_delivery() {
    let position = json!({"symbol":"ETHUSDT","positionSide":"SHORT","marginAsset":"USDT","positionAmt":"-0.001","entryPrice":"1234.123456789012345678",
        "markPrice":"1235.0","unRealizedProfit":"-0.000876543210987655","notional":"-1.2350","isolatedMargin":"0.2"});
    let mut delivery = position.clone();
    delivery["symbol"] = json!("ETHUSDT_261225");
    let venue = Fixture::new(vec![
        futures_markets(),
        (
            BinanceEndpoint::FuturesPositions,
            json!([position, delivery]),
        ),
    ]);
    let result = trading_read(
        "binance.futures.positions",
        &json!({"account":"main"}),
        &config(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["positions"].as_array().unwrap().len(), 1);
    assert_eq!(result["positions"][0]["positionAmt"], "-0.001");
    assert_eq!(result["positions"][0]["positionSide"], "SHORT");
    venue.finished();
}

#[tokio::test]
async fn futures_order_and_open_orders_validate_perpetual_market_and_requested_identity() {
    for (operation, endpoint, args, body, key) in [
        (
            "binance.futures.order",
            BinanceEndpoint::FuturesOrder,
            json!({"account":"main","symbol":"ETHUSDT","client_order_id":"fixture-order"}),
            order(),
            "order",
        ),
        (
            "binance.futures.orders",
            BinanceEndpoint::FuturesOpenOrders,
            json!({"account":"main","symbol":"ETHUSDT"}),
            json!([order()]),
            "orders",
        ),
    ] {
        let venue = Fixture::new(vec![futures_markets(), (endpoint, body)]);
        let result = trading_read(operation, &args, &config(), &venue)
            .await
            .unwrap();
        let selected = if key == "orders" {
            &result[key][0]
        } else {
            &result[key]
        };
        assert_eq!(selected["positionSide"], "BOTH");
        assert_eq!(selected["reduceOnly"], false);
        venue.finished();
    }
}

#[tokio::test]
async fn futures_income_keeps_negative_funding_and_integer_identity() {
    let venue = Fixture::new(vec![
        futures_markets(),
        (
            BinanceEndpoint::FuturesIncome,
            json!([{
                "symbol":"ETHUSDT","incomeType":"FUNDING_FEE","asset":"USDT","income":"-0.000000000000000001","tranId":9007199254740999u64,"time":1790000000000u64,"info":"not-disclosed"
            }]),
        ),
    ]);
    let result = trading_read(
        "binance.futures.income",
        &json!({"account":"main","symbol":"ETHUSDT"}),
        &config(),
        &venue,
    )
    .await
    .unwrap();
    assert_eq!(result["income"][0]["income"], "-0.000000000000000001");
    assert_eq!(result["income"][0]["tranId"], "9007199254740999");
    assert!(result["income"][0].get("info").is_none());
    venue.finished();
}

#[tokio::test]
async fn malformed_unbounded_and_unknown_inputs_never_call_transport() {
    for (operation, args) in [
        (
            "binance.spot.orders",
            json!({"account":"main","symbol":"ETHUSDT","limit":0}),
        ),
        (
            "binance.spot.trades",
            json!({"account":"main","symbol":"ETHUSDT","limit":1001}),
        ),
        (
            "binance.spot.orders",
            json!({"account":"main","symbol":"ETHUSDT&secret=x"}),
        ),
        (
            "binance.spot.orders",
            json!({"account":"main","symbol":"ethusdt"}),
        ),
        (
            "binance.spot.orders",
            json!({"account":"main","symbol":"ETHUSDT","params":{}}),
        ),
        (
            "binance.spot.order",
            json!({"account":"main","symbol":"ETHUSDT"}),
        ),
        (
            "binance.spot.order",
            json!({"account":"main","symbol":"ETHUSDT","order_id":"1","client_order_id":"two"}),
        ),
        (
            "binance.spot.order",
            json!({"account":"main","symbol":"ETHUSDT","order_id":9007199254740993u64}),
        ),
        (
            "binance.spot.order",
            json!({"account":"main","symbol":"ETHUSDT","client_order_id":"bad&identity"}),
        ),
        ("binance.futures.balances", json!({"account":"missing"})),
        (
            "binance.futures.place",
            json!({"account":"main","symbol":"ETHUSDT"}),
        ),
    ] {
        let venue = Fixture::new(vec![]);
        assert!(
            trading_read(operation, &args, &config(), &venue)
                .await
                .is_err(),
            "accepted {operation}: {args}"
        );
        assert!(venue.seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn delivery_symbol_is_rejected_before_authenticated_order_read() {
    let venue = Fixture::new(vec![futures_markets()]);
    let error = trading_read(
        "binance.futures.order",
        &json!({"account":"main","symbol":"ETHUSDT_261225","order_id":"1"}),
        &config(),
        &venue,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "UNSUPPORTED_MARKET");
    venue.finished();
}

#[tokio::test]
async fn numeric_amounts_and_mismatched_order_evidence_are_rejected() {
    for changed in [
        {
            let mut row = order();
            row["price"] = json!(1234.0);
            row
        },
        {
            let mut row = order();
            row["origQty"] = json!("1e-9");
            row
        },
        {
            let mut row = order();
            row["orderId"] = json!(9007199254740994u64);
            row
        },
        {
            let mut row = order();
            row["symbol"] = json!("BTCUSDT");
            row
        },
    ] {
        let venue = Fixture::new(vec![(BinanceEndpoint::SpotOrder, changed)]);
        assert_eq!(
            trading_read(
                "binance.spot.order",
                &json!({"account":"main","symbol":"ETHUSDT","order_id":"9007199254740993"}),
                &config(),
                &venue
            )
            .await
            .unwrap_err()
            .code,
            "VENUE_RESPONSE_INVALID"
        );
    }
}

#[test]
fn discovery_names_only_implemented_reads_with_bounded_inputs_and_explicit_coverage() {
    let defs = trading_read_definitions();
    assert_eq!(defs.len(), 11);
    for def in defs {
        assert_eq!(def.kind, OperationKind::Read);
        assert_eq!(def.input_schema["additionalProperties"], false);
        assert!(
            def.coverage
                .contains("Production account and live acceptance pending")
        );
        assert!(!def.examples.is_empty());
        if def.name.ends_with(".trades")
            || def.name.ends_with(".orders")
            || def.name.ends_with(".income")
        {
            assert_eq!(def.input_schema["properties"]["limit"]["minimum"], 1);
            assert_eq!(def.input_schema["properties"]["limit"]["maximum"], 1000);
        }
        if def.name.ends_with(".order") {
            assert_eq!(def.input_schema["oneOf"].as_array().unwrap().len(), 2);
        }
    }
}
