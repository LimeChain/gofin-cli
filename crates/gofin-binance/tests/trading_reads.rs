use gofin_binance::{
    AccountRole, BinanceAccount, BinanceConfig, BinanceEndpoint, BinanceRequest, BinanceTransport,
    CredentialHandle, TransportError, trading_reads::trading_read,
};
use gofin_domain::ResourceId;
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
