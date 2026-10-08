//! A public-read profile shared across reviewed exchange entries. Exchange
//! routing is data; there is no per-exchange implementation of Adapter.
use crate::ExchangeProfile;
#[cfg(feature = "ccxt")]
use crate::ccxt::{CcxtClient, ExchangeId, Method};
use crate::order_book::{normalize_order_book, order_book_definition};
use async_trait::async_trait;
use fin_domain::{
    Definition, ExecutionActor, FinError, FinancialState, OperationKind, StateSnapshot, now,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// Injection point for offline fixtures. Production uses the CCXT wrapper.
#[async_trait]
pub trait PublicExchangeData: Send + Sync {
    async fn markets(&self, exchange: &str) -> Result<Value, FinError>;
    async fn ticker(&self, exchange: &str, pair: &str) -> Result<Value, FinError>;
    async fn order_book(
        &self,
        exchange: &str,
        market_type: &str,
        pair: &str,
        depth: u16,
    ) -> Result<(Value, Value, String), FinError>;
}

const VENUES: [(&str, &str, &str); 5] = [
    ("kraken", "XBTUSD", "spot"),
    ("hyperliquid", "BTC", "perpetuals; primary dex"),
    ("okx", "BTC-USDT", "spot"),
    ("coinbase", "BTC-USD", "Coinbase Advanced public products"),
    ("bybit", "BTCUSDT", "spot"),
];

#[cfg(feature = "ccxt")]
#[async_trait]
impl PublicExchangeData for CcxtClient {
    async fn markets(&self, exchange: &str) -> Result<Value, FinError> {
        let (venue, method, params) = match exchange {
            "kraken" => (ExchangeId::Kraken, Method::KrakenAssetPairs, vec![]),
            "hyperliquid" => (
                ExchangeId::Hyperliquid,
                Method::HyperliquidMetaAndAssetContexts,
                vec![("type", "metaAndAssetCtxs")],
            ),
            "okx" => (
                ExchangeId::Okx,
                Method::OkxInstruments,
                vec![("instType", "SPOT")],
            ),
            "coinbase" => (
                ExchangeId::Coinbase,
                Method::CoinbaseProducts,
                vec![("product_type", "SPOT"), ("limit", "1000")],
            ),
            "bybit" => (
                ExchangeId::Bybit,
                Method::BybitInstruments,
                vec![("category", "spot")],
            ),
            _ => return Err(unsupported()),
        };
        self.request(
            venue,
            method,
            None,
            params
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
        .await
        .map_err(unavailable)
    }
    async fn ticker(&self, exchange: &str, pair: &str) -> Result<Value, FinError> {
        let (venue, method, params) = match exchange {
            "kraken" => (
                ExchangeId::Kraken,
                Method::KrakenTicker,
                vec![("pair", pair)],
            ),
            "hyperliquid" => (
                ExchangeId::Hyperliquid,
                Method::HyperliquidMetaAndAssetContexts,
                vec![("type", "metaAndAssetCtxs")],
            ),
            "okx" => (ExchangeId::Okx, Method::OkxTicker, vec![("instId", pair)]),
            "coinbase" => (
                ExchangeId::Coinbase,
                Method::CoinbaseTicker,
                vec![("product_id", pair)],
            ),
            "bybit" => (
                ExchangeId::Bybit,
                Method::BybitTicker,
                vec![("category", "spot"), ("symbol", pair)],
            ),
            _ => return Err(unsupported()),
        };
        self.request(
            venue,
            method,
            None,
            params
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
        .await
        .map_err(unavailable)
    }
    async fn order_book(
        &self,
        exchange: &str,
        market_type: &str,
        pair: &str,
        depth: u16,
    ) -> Result<(Value, Value, String), FinError> {
        let (venue, metadata_method, metadata_params, book_method, book_params) = match (
            exchange,
            market_type,
        ) {
            ("binance", "spot") => (
                ExchangeId::Binance,
                Method::BinanceSpotExchangeInfo,
                vec![("symbol", pair)],
                Method::BinanceOrderBookSpot,
                vec![
                    ("symbol", pair.to_owned()),
                    (
                        "limit",
                        crate::order_book::venue_depth("binance", depth).to_string(),
                    ),
                ],
            ),
            ("binance", "perpetual") => (
                ExchangeId::Binance,
                Method::BinanceFuturesExchangeInfo,
                vec![("symbol", pair)],
                Method::BinanceOrderBookPerpetual,
                vec![
                    ("symbol", pair.to_owned()),
                    (
                        "limit",
                        crate::order_book::venue_depth("binance", depth).to_string(),
                    ),
                ],
            ),
            ("kraken", "spot") => (
                ExchangeId::Kraken,
                Method::KrakenAssetPairs,
                vec![("pair", pair)],
                Method::KrakenOrderBook,
                vec![("pair", pair.to_owned()), ("count", depth.to_string())],
            ),
            ("hyperliquid", "perpetual") => (
                ExchangeId::Hyperliquid,
                Method::HyperliquidMetaAndAssetContexts,
                vec![("type", "metaAndAssetCtxs")],
                Method::HyperliquidOrderBook,
                vec![("type", "l2Book".into()), ("coin", pair.to_owned())],
            ),
            ("okx", "spot") => (
                ExchangeId::Okx,
                Method::OkxInstruments,
                vec![("instType", "SPOT"), ("instId", pair)],
                Method::OkxOrderBook,
                vec![("instId", pair.to_owned()), ("sz", depth.to_string())],
            ),
            ("coinbase", "spot") => (
                ExchangeId::Coinbase,
                Method::CoinbaseProduct,
                vec![("product_id", pair)],
                Method::CoinbaseOrderBook,
                vec![
                    ("product_id", pair.to_owned()),
                    ("limit", depth.to_string()),
                ],
            ),
            ("bybit", "spot") => (
                ExchangeId::Bybit,
                Method::BybitInstruments,
                vec![("category", "spot"), ("symbol", pair)],
                Method::BybitOrderBook,
                vec![
                    ("category", "spot".into()),
                    ("symbol", pair.to_owned()),
                    (
                        "limit",
                        crate::order_book::venue_depth("bybit", depth).to_string(),
                    ),
                ],
            ),
            _ => {
                return Err(FinError::validation(
                    "This exchange does not support the requested market type for order-book reads.",
                ));
            }
        };
        let metadata = self
            .request(
                venue,
                metadata_method,
                None,
                metadata_params
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
            )
            .await
            .map_err(unavailable)?;
        let started = now();
        let book = self
            .request(
                venue,
                book_method,
                None,
                book_params
                    .into_iter()
                    .map(|(k, v)| (k.into(), v))
                    .collect(),
            )
            .await
            .map_err(unavailable)?;
        Ok((metadata, book, started))
    }
}
#[cfg(feature = "ccxt")]
fn unavailable(error: crate::ccxt::CcxtError) -> FinError {
    use crate::ccxt::CcxtErrorKind;
    let code = match error.kind {
        CcxtErrorKind::Unsupported => "EXCHANGE_NOT_COMPILED",
        CcxtErrorKind::RateLimited => "VENUE_RATE_LIMITED",
        CcxtErrorKind::PermissionDenied => "VENUE_ACCESS_DENIED",
        _ => "VENUE_UNAVAILABLE",
    };
    FinError::new(
        code,
        "read",
        "Exchange public data could not be read.",
        "Check venue availability and compiled capabilities; no financial action was submitted.",
    )
}
fn unsupported() -> FinError {
    FinError::new(
        "UNSUPPORTED_EXCHANGE",
        "validation",
        "This exchange is not supported for public reads.",
        "Use an exchange listed in the operation schema.",
    )
}
fn require_exchange(exchange: &str) -> Result<(), FinError> {
    if VENUES.iter().any(|(name, _, _)| *name == exchange) {
        Ok(())
    } else {
        Err(unsupported())
    }
}
fn pair_pattern(exchange: &str) -> &'static str {
    match exchange {
        "okx" | "coinbase" => "^[A-Z0-9]+-[A-Z0-9]+$",
        "hyperliquid" => "^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$",
        _ => "^[A-Z0-9]{2,24}$",
    }
}
fn valid_pair(exchange: &str, pair: &str) -> bool {
    match exchange {
        "okx" | "coinbase" => {
            let mut parts = pair.split('-');
            let valid = |part: &str| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            };
            pair.len() <= 64
                && parts.next().is_some_and(valid)
                && parts.next().is_some_and(valid)
                && parts.next().is_none()
        }
        "hyperliquid" => {
            !pair.is_empty()
                && pair.len() <= 32
                && pair.as_bytes()[0].is_ascii_alphanumeric()
                && pair
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        }
        _ => {
            (2..=24).contains(&pair.len())
                && pair
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        }
    }
}
fn invalid_response() -> FinError {
    FinError::new(
        "VENUE_RESPONSE_INVALID",
        "read",
        "Exchange public response was invalid or did not contain the requested market.",
        "Inspect the pair with exchange.markets or retry this read later.",
    )
}

// Preserve the venue envelope. Hyperliquid associates contexts by array index;
// select only after checking alignment and the exact market name.
fn public_response(exchange: &str, pair: Option<&str>, data: Value) -> Result<Value, FinError> {
    match exchange {
        "hyperliquid" => {
            let values = data
                .as_array()
                .filter(|a| a.len() == 2)
                .ok_or_else(invalid_response)?;
            let markets = values[0]["universe"]
                .as_array()
                .ok_or_else(invalid_response)?;
            let contexts = values[1].as_array().ok_or_else(invalid_response)?;
            if markets.is_empty() || markets.len() != contexts.len() {
                return Err(invalid_response());
            }
            if let Some(pair) = pair {
                let index = markets
                    .iter()
                    .position(|m| m["name"].as_str() == Some(pair))
                    .ok_or_else(invalid_response)?;
                if !contexts[index]["markPx"].is_string() {
                    return Err(invalid_response());
                }
                return Ok(json!({"market":markets[index],"context":contexts[index]}));
            }
        }
        "okx" => {
            if data["code"] != "0" {
                return Err(invalid_response());
            }
            let rows = data["data"]
                .as_array()
                .filter(|a| !a.is_empty())
                .ok_or_else(invalid_response)?;
            if let Some(pair) = pair
                && (rows.len() != 1 || rows[0]["instId"] != pair || !rows[0]["last"].is_string())
            {
                return Err(invalid_response());
            }
        }
        "bybit" => {
            if data["retCode"] != 0 || data["result"]["category"] != "spot" {
                return Err(invalid_response());
            }
            let rows = data["result"]["list"]
                .as_array()
                .filter(|a| !a.is_empty())
                .ok_or_else(invalid_response)?;
            if let Some(pair) = pair
                && (rows.len() != 1
                    || rows[0]["symbol"] != pair
                    || !rows[0]["lastPrice"].is_string())
            {
                return Err(invalid_response());
            }
        }
        "coinbase" => {
            if pair.is_some() {
                if !data["best_bid"].is_string()
                    || !data["best_ask"].is_string()
                    || !data["trades"].is_array()
                {
                    return Err(invalid_response());
                }
                if data["trades"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["product_id"].as_str() != pair)
                {
                    return Err(invalid_response());
                }
            } else if !data["products"].as_array().is_some_and(|a| !a.is_empty()) {
                return Err(invalid_response());
            }
        }
        "kraken" => {
            if !data["error"].as_array().is_some_and(Vec::is_empty)
                || !data["result"].as_object().is_some_and(|a| !a.is_empty())
            {
                return Err(invalid_response());
            }
        }
        _ => return Err(unsupported()),
    }
    Ok(data)
}

#[derive(Clone)]
pub struct DefaultExchangeProfile {
    data: Arc<dyn PublicExchangeData>,
}
impl DefaultExchangeProfile {
    pub fn new(data: Arc<dyn PublicExchangeData>) -> Self {
        Self { data }
    }
    pub fn definitions() -> Vec<Definition> {
        let mut values = definitions();
        values.push(order_book_definition());
        values
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketsInput {
    exchange: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TickerInput {
    exchange: String,
    pair: String,
}

fn definitions() -> Vec<Definition> {
    let venues: Vec<_> = VENUES.iter().map(|(name, _, _)| *name).collect();
    [("exchange.markets", "Read public market metadata (venue snapshot/page).", false), ("exchange.ticker", "Read public ticker data for a venue pair.", true)].into_iter().map(|(name, description, ticker)| {
        let mut schema = json!({"type":"object","properties":{"exchange":{"type":"string","enum":venues}},"required":["exchange"],"additionalProperties":false});
        if ticker {
            schema["properties"]["pair"] = json!({"type":"string","maxLength":64,"description":"Native venue identifier from exchange.markets: XBTUSD (Kraken), BTC (Hyperliquid perpetual), BTC-USDT (OKX), BTC-USD (Coinbase), BTCUSDT (Bybit)."});
            schema["required"] = json!(["exchange","pair"]);
            schema["oneOf"] = Value::Array(VENUES.iter().map(|(venue,_,_)|json!({"properties":{"exchange":{"const":venue},"pair":{"pattern":pair_pattern(venue)}}})).collect());
        }
        Definition {
            name:name.into(), description:description.into(), kind:OperationKind::Read,
            execution_actor:ExecutionActor::AgentReadable, input_schema:schema,
            output_schema:json!({"type":"object","properties":{"exchange":{"enum":venues},"data":{"type":["object","array"]}},"required":["exchange","data"],"additionalProperties":false}),
            error_schema:serde_json::to_value(schemars::schema_for!(FinError)).expect("error schema"),
            examples:VENUES.iter().map(|(venue,pair,_)|if ticker {json!({"exchange":venue,"pair":pair})} else {json!({"exchange":venue})}).collect(),
            coverage:"Public market/ticker reads through official CCXT Rust. No account enrollment or writes. Coinbase market discovery returns at most 1000 spot products; venue pagination metadata is retained, completeness is not asserted.".into(),
            completion:"Returns venue public data; does not imply any financial settlement.".into(),
            semantics:json!({"library":"ccxt-rust","finImplemented":["public_market_metadata","public_ticker"],"writes":false,"credentials":false,"marketScopes":VENUES.iter().map(|(name,_,scope)|(*name,*scope)).collect::<std::collections::BTreeMap<_,_>>(),"verification":"offline_synthetic; see verification report for public network checks","exactness":"Venue decimal strings and envelopes preserved; Hyperliquid ticker selects its matching market/context. Market observations are not authoritative accounting."}),
        }
    }).collect()
}

#[async_trait]
impl ExchangeProfile for DefaultExchangeProfile {
    type Request = ();
    fn definitions(&self) -> Vec<Definition> {
        let mut values = definitions();
        values.push(order_book_definition());
        values
    }
    fn submission_kind(&self) -> &str {
        "public-exchange"
    }
    async fn snapshot(&self) -> Result<StateSnapshot, FinError> {
        // This profile enrolls no accounts and owns no balances or positions.
        // Its empty scope is complete and must not block unrelated financial runs.
        Ok(StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![
                "public-exchange: public reads only; no enrolled financial accounts".into(),
            ],
            pending: vec![],
        })
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value, FinError> {
        let (exchange, pair, data) = match operation {
            "exchange.markets" => {
                let input: MarketsInput = serde_json::from_value(input.clone()).map_err(|_| {
                    FinError::validation("Expected an exchange name with no extra parameters.")
                })?;
                require_exchange(&input.exchange)?;
                let data = self.data.markets(&input.exchange).await?;
                (input.exchange, None, data)
            }
            "exchange.ticker" => {
                let input: TickerInput = serde_json::from_value(input.clone()).map_err(|_| {
                    FinError::validation("Expected exchange and pair with no extra parameters.")
                })?;
                require_exchange(&input.exchange)?;
                if !valid_pair(&input.exchange, &input.pair) {
                    return Err(FinError::validation(
                        "Pair does not match this venue's identifier format; inspect exchange.markets.",
                    ));
                }
                let data = self.data.ticker(&input.exchange, &input.pair).await?;
                (input.exchange, Some(input.pair), data)
            }
            "exchange.order-book" => {
                let input: crate::order_book::OrderBookInput =
                    serde_json::from_value(input.clone()).map_err(|_| {
                        FinError::validation(
                            "Expected exchange, market_type, exact pair, and bounded depth.",
                        )
                    })?;
                crate::order_book::validate_order_book_input(&input)?;
                let (metadata, book, started) = self
                    .data
                    .order_book(
                        &input.exchange,
                        &input.market_type,
                        &input.pair,
                        input.depth,
                    )
                    .await?;
                let data = normalize_order_book(
                    &input.exchange,
                    &input.market_type,
                    &input.pair,
                    input.depth,
                    metadata,
                    book,
                    started,
                )?;
                return Ok(data);
            }
            _ => {
                return Err(FinError::new(
                    "UNSUPPORTED_OPERATION",
                    "read",
                    "This public exchange operation is not implemented.",
                    "Inspect capabilities for supported named operations.",
                ));
            }
        };
        let data = public_response(&exchange, pair.as_deref(), data)?;
        Ok(json!({"exchange":exchange,"data":data}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExchangeAdapter;
    use fin_domain::Adapter;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fixture(AtomicUsize);
    #[async_trait]
    impl PublicExchangeData for Fixture {
        async fn markets(&self, exchange: &str) -> Result<Value, FinError> {
            assert_eq!(exchange, "kraken");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"error":[],"result":{"XXBTZUSD":{"pair_decimals":1,"ordermin":"0.00005000"}}}),
            )
        }
        async fn ticker(&self, exchange: &str, pair: &str) -> Result<Value, FinError> {
            assert_eq!((exchange, pair), ("kraken", "XBTUSD"));
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(
                json!({"error":[],"result":{"XXBTZUSD":{"c":["123456789.123456789123456789","0.000000000000000001"]}}}),
            )
        }
        async fn order_book(
            &self,
            _exchange: &str,
            _market_type: &str,
            _pair: &str,
            _depth: u16,
        ) -> Result<(Value, Value, String), FinError> {
            Err(FinError::validation("not used"))
        }
    }
    #[tokio::test]
    async fn second_exchange_uses_shared_adapter_and_retains_raw_decimal_strings() {
        let fixture = Arc::new(Fixture(AtomicUsize::new(0)));
        let adapter = ExchangeAdapter::new(DefaultExchangeProfile::new(fixture.clone()));
        let markets = adapter
            .read("exchange.markets", &json!({"exchange":"kraken"}))
            .await
            .unwrap();
        assert_eq!(
            markets["data"]["result"]["XXBTZUSD"]["ordermin"],
            "0.00005000"
        );
        let ticker = adapter
            .read(
                "exchange.ticker",
                &json!({"exchange":"kraken","pair":"XBTUSD"}),
            )
            .await
            .unwrap();
        assert_eq!(
            ticker["data"]["result"]["XXBTZUSD"]["c"][0],
            "123456789.123456789123456789"
        );
        assert_eq!(fixture.0.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn public_profile_has_no_financial_scope_to_block_composed_runs() {
        let fixture = Arc::new(Fixture(AtomicUsize::new(0)));
        let adapter = ExchangeAdapter::new(DefaultExchangeProfile::new(fixture.clone()));
        let snapshot = adapter.snapshot().await.unwrap();
        assert!(snapshot.complete);
        assert_eq!(snapshot.state, FinancialState::default());
        assert!(snapshot.pending.is_empty());
        assert!(adapter.connections().is_empty());
        assert_eq!(fixture.0.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn unsupported_exchange_params_methods_and_writes_fail_before_dispatch() {
        let fixture = Arc::new(Fixture(AtomicUsize::new(0)));
        let adapter = ExchangeAdapter::new(DefaultExchangeProfile::new(fixture.clone()));
        for input in [
            json!({"exchange":"bitstamp"}),
            json!({"exchange":"kraken","params":{"method":"withdraw"}}),
        ] {
            assert!(adapter.read("exchange.markets", &input).await.is_err());
        }
        assert!(
            adapter
                .read(
                    "exchange.ticker",
                    &json!({"exchange":"kraken","pair":"XBTUSD&method=withdraw"})
                )
                .await
                .is_err()
        );
        assert!(adapter.read("exchange.withdraw", &json!({})).await.is_err());
        assert!(
            adapter
                .prepare("exchange.markets", &json!({"exchange":"kraken"}))
                .await
                .is_err()
        );
        assert_eq!(fixture.0.load(Ordering::SeqCst), 0);
    }
}
