//! Public-only synthetic venue envelopes. No credentials, network or compiled
//! CCXT feature is needed to exercise the common profile's response contract.
use async_trait::async_trait;
use gofin_domain::{Adapter, GoFinError};
use gofin_exchange::{DefaultExchangeProfile, ExchangeAdapter, PublicExchangeData};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PRICE: &str = "123456.123456789123456789";
const SIZE: &str = "0.000000000000000001";
const VENUES: [(&str, &str); 5] = [
    ("kraken", "XBTUSD"),
    ("hyperliquid", "BTC"),
    ("okx", "BTC-USDT"),
    ("coinbase", "BTC-USD"),
    ("bybit", "BTCUSDT"),
];

fn hyperliquid() -> Value {
    // BTC is deliberately second. Selecting contexts[0] would give ETH's price.
    json!([
        {"universe":[{"name":"ETH","szDecimals":4,"maxLeverage":25},{"name":"BTC","szDecimals":5,"maxLeverage":40}]},
        [
            {"markPx":"4321.25","oraclePx":"4321.20","funding":"0.0000125","openInterest":"10"},
            {"markPx":PRICE,"oraclePx":PRICE,"funding":"-0.000000000000123456","openInterest":SIZE}
        ]
    ])
}
fn markets(exchange: &str) -> Value {
    match exchange {
        "kraken" => serde_json::from_str(r#"{"error":[],"result":{"XXBTZUSD":{"altname":"XBTUSD","wsname":"XBT/USD","pair_decimals":1,"lot_decimals":8,"ordermin":"0.00005000","fees":[[0,0.260000000000000000000001]]}}}"#).unwrap(),
        "hyperliquid" => hyperliquid(),
        "okx" => json!({"code":"0","msg":"","data":[{"instType":"SPOT","instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","tickSz":"0.10000000","lotSz":SIZE,"minSz":"0.00001000","state":"live"}]}),
        "coinbase" => json!({"products":[{"product_id":"BTC-USD","price":PRICE,"base_increment":SIZE,"quote_increment":"0.01","base_min_size":"0.00000001","product_type":"SPOT","status":"online"}],"num_products":1,"pagination":{"has_next":true,"next_cursor":"synthetic-next-page"}}),
        "bybit" => json!({"retCode":0,"retMsg":"OK","result":{"category":"spot","list":[{"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","status":"Trading","lotSizeFilter":{"basePrecision":SIZE,"minOrderAmt":"1"},"priceFilter":{"tickSize":"0.01"}}],"nextPageCursor":"synthetic-next-page"},"time":1770000000000_u64}),
        _ => panic!("unexpected synthetic venue"),
    }
}
fn ticker(exchange: &str) -> Value {
    match exchange {
        "kraken" => {
            json!({"error":[],"result":{"XXBTZUSD":{"a":[PRICE,"1",SIZE],"b":[PRICE,"1",SIZE],"c":[PRICE,SIZE],"v":[SIZE,SIZE]}}})
        }
        "hyperliquid" => hyperliquid(),
        "okx" => {
            json!({"code":"0","msg":"","data":[{"instType":"SPOT","instId":"BTC-USDT","last":PRICE,"lastSz":SIZE,"askPx":PRICE,"bidPx":PRICE,"ts":"1770000000000"}]})
        }
        "coinbase" => {
            json!({"best_bid":PRICE,"best_ask":PRICE,"trades":[{"trade_id":"9007199254740993","product_id":"BTC-USD","price":PRICE,"size":SIZE,"side":"BUY","time":"2026-01-01T00:00:00Z"}]})
        }
        "bybit" => {
            json!({"retCode":0,"retMsg":"OK","result":{"category":"spot","list":[{"symbol":"BTCUSDT","lastPrice":PRICE,"bid1Price":PRICE,"ask1Price":PRICE,"bid1Size":SIZE,"ask1Size":SIZE}]},"time":9007199254740993_u64})
        }
        _ => panic!("unexpected synthetic venue"),
    }
}

struct Fixture {
    exchange: &'static str,
    market_response: Value,
    ticker_response: Value,
    calls: Mutex<Vec<(String, String)>>,
}
#[async_trait]
impl PublicExchangeData for Fixture {
    async fn markets(&self, exchange: &str) -> Result<Value, GoFinError> {
        assert_eq!(exchange, self.exchange);
        self.calls
            .lock()
            .unwrap()
            .push(("markets".into(), exchange.into()));
        Ok(self.market_response.clone())
    }
    async fn ticker(&self, exchange: &str, pair: &str) -> Result<Value, GoFinError> {
        assert_eq!(exchange, self.exchange);
        self.calls
            .lock()
            .unwrap()
            .push(("ticker".into(), pair.into()));
        Ok(self.ticker_response.clone())
    }
    async fn order_book(
        &self,
        exchange: &str,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<(Value, Value, String), GoFinError> {
        assert_eq!(exchange, self.exchange);
        Err(GoFinError::validation("order book not used by this test"))
    }
}
fn fixture(
    exchange: &'static str,
    market_response: Value,
    ticker_response: Value,
) -> (ExchangeAdapter<DefaultExchangeProfile>, Arc<Fixture>) {
    let data = Arc::new(Fixture {
        exchange,
        market_response,
        ticker_response,
        calls: Mutex::new(vec![]),
    });
    (
        ExchangeAdapter::new(DefaultExchangeProfile::new(data.clone())),
        data,
    )
}

#[tokio::test]
async fn hyperliquid_misaligned_missing_or_invalid_contexts_are_rejected() {
    let mut short = hyperliquid();
    short[1].as_array_mut().unwrap().pop();
    let mut long = hyperliquid();
    long[1].as_array_mut().unwrap().push(json!({"markPx":"1"}));
    let mut numeric_price = hyperliquid();
    numeric_price[1][1]["markPx"] = json!(123);
    let mut missing_price = hyperliquid();
    missing_price[1][1]
        .as_object_mut()
        .unwrap()
        .remove("markPx");
    for malformed in [
        json!({"universe":[]}),
        json!([{"universe":[]},[]]),
        json!([{}, []]),
        short,
        long,
        numeric_price,
        missing_price,
    ] {
        let (adapter, data) = fixture("hyperliquid", malformed.clone(), malformed);
        assert_eq!(
            adapter
                .read(
                    "exchange.ticker",
                    &json!({"exchange":"hyperliquid","pair":"BTC"})
                )
                .await
                .unwrap_err()
                .code,
            "VENUE_RESPONSE_INVALID"
        );
        assert_eq!(data.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn ticker_wrong_product_category_or_venue_error_never_returns_a_quote() {
    let mut okx_pair = ticker("okx");
    okx_pair["data"][0]["instId"] = json!("ETH-USDT");
    let mut okx_multiple = ticker("okx");
    okx_multiple["data"]
        .as_array_mut()
        .unwrap()
        .push(json!({"instId":"ETH-USDT","last":"1"}));
    let mut bybit_pair = ticker("bybit");
    bybit_pair["result"]["list"][0]["symbol"] = json!("ETHUSDT");
    let mut bybit_category = ticker("bybit");
    bybit_category["result"]["category"] = json!("linear");
    let mut coinbase_pair = ticker("coinbase");
    coinbase_pair["trades"][0]["product_id"] = json!("ETH-USD");
    for (venue, pair, response) in [
        ("okx", "BTC-USDT", okx_pair),
        ("okx", "BTC-USDT", okx_multiple),
        (
            "okx",
            "BTC-USDT",
            json!({"code":"51001","msg":"instrument does not exist","data":[]}),
        ),
        ("bybit", "BTCUSDT", bybit_pair),
        ("bybit", "BTCUSDT", bybit_category),
        (
            "bybit",
            "BTCUSDT",
            json!({"retCode":10001,"result":{"category":"spot","list":[]}}),
        ),
        ("coinbase", "BTC-USD", coinbase_pair),
        ("coinbase", "BTC-USD", json!({"error":"NOT_FOUND"})),
        (
            "kraken",
            "XBTUSD",
            json!({"error":["EQuery:Unknown asset pair"],"result":{}}),
        ),
        ("kraken", "XBTUSD", json!({"error":[],"result":{}})),
    ] {
        let (adapter, data) = fixture(venue, markets(venue), response);
        assert_eq!(
            adapter
                .read("exchange.ticker", &json!({"exchange":venue,"pair":pair}))
                .await
                .unwrap_err()
                .code,
            "VENUE_RESPONSE_INVALID"
        );
        assert_eq!(data.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn malformed_market_envelopes_are_rejected_for_every_venue() {
    for (venue, _) in VENUES {
        for response in [
            Value::Null,
            json!({}),
            json!([]),
            json!({"error":"unavailable"}),
        ] {
            let (adapter, data) = fixture(venue, response, ticker(venue));
            assert_eq!(
                adapter
                    .read("exchange.markets", &json!({"exchange":venue}))
                    .await
                    .unwrap_err()
                    .code,
                "VENUE_RESPONSE_INVALID"
            );
            assert_eq!(data.calls.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn invalid_symbols_extra_parameters_and_write_paths_make_no_data_calls() {
    for (venue, pair) in VENUES {
        let (adapter, data) = fixture(venue, markets(venue), ticker(venue));
        for invalid in [
            "",
            "BTC/USDT",
            "BTC&method=withdraw",
            "BTC\nUSD",
            "BTC USD",
            "€BTC",
            "../BTC",
        ] {
            assert!(
                adapter
                    .read("exchange.ticker", &json!({"exchange":venue,"pair":invalid}))
                    .await
                    .is_err(),
                "{venue}: {invalid:?}"
            );
        }
        assert!(
            adapter
                .read(
                    "exchange.ticker",
                    &json!({"exchange":venue,"pair":"A".repeat(65)})
                )
                .await
                .is_err()
        );
        for extra in [
            json!({"exchange":venue,"pair":pair,"params":{"category":"linear"}}),
            json!({"exchange":venue,"pair":pair,"method":"withdraw"}),
            json!({"exchange":venue,"pair":pair,"api_key":"synthetic-not-a-key"}),
        ] {
            assert!(adapter.read("exchange.ticker", &extra).await.is_err());
        }
        assert!(
            adapter
                .read("exchange.markets", &json!({"exchange":venue,"pair":pair}))
                .await
                .is_err()
        );
        assert!(
            adapter
                .read("exchange.markets", &json!({"exchange":"unsupported"}))
                .await
                .is_err()
        );
        assert!(
            adapter
                .read("exchange.withdraw", &json!({"exchange":venue}))
                .await
                .is_err()
        );
        assert!(
            adapter
                .prepare("exchange.markets", &json!({"exchange":venue}))
                .await
                .is_err()
        );
        assert!(
            adapter
                .prepare("exchange.ticker", &json!({"exchange":venue,"pair":pair}))
                .await
                .is_err()
        );
        assert!(data.calls.lock().unwrap().is_empty());
    }
}
