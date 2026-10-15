//! Exact, exchange-neutral order-book read contract and venue evidence checks.
use gofin_domain::{Definition, ExecutionActor, GoFinError, OperationKind};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrderBookInput {
    pub exchange: String,
    pub market_type: String,
    pub pair: String,
    pub depth: u16,
}

pub(crate) fn validate_order_book_input(input: &OrderBookInput) -> Result<(), GoFinError> {
    let supported = match input.exchange.as_str() {
        "binance" => matches!(input.market_type.as_str(), "spot" | "perpetual"),
        "hyperliquid" => input.market_type == "perpetual",
        "kraken" | "okx" | "coinbase" | "bybit" => input.market_type == "spot",
        _ => false,
    };
    let pair_valid = !input.pair.is_empty()
        && input.pair.len() <= 64
        && !input.pair.chars().any(char::is_control)
        && !input.pair.chars().any(char::is_whitespace);
    if !supported || !pair_valid || !(1..=100).contains(&input.depth) {
        return Err(GoFinError::validation(
            "Use an enabled exchange and its supported market type, an exact market identifier from exchange.markets, and depth from 1 to 100.",
        ));
    }
    Ok(())
}

pub(crate) fn order_book_definition() -> Definition {
    let venues = json!([
        "binance",
        "kraken",
        "hyperliquid",
        "okx",
        "coinbase",
        "bybit"
    ]);
    Definition {
        name: "exchange.order-book".into(),
        description: "Read exact public bid and ask levels for one verified exchange market.".into(),
        kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable,
        input_schema: json!({
            "type":"object",
            "properties":{
                "exchange":{"type":"string","enum":venues},
                "market_type":{"type":"string","enum":["spot","perpetual"],"description":"Must match the venue's supported market scope."},
                "pair":{"type":"string","minLength":1,"maxLength":64,"description":"Exact native market identifier from exchange.markets, binance.spot.markets, or binance.futures.markets as applicable."},
                "depth":{"type":"integer","minimum":1,"maximum":100,"description":"Maximum number of levels per side."}
            },
            "required":["exchange","market_type","pair","depth"],
            "additionalProperties":false,
            "oneOf":[
                {"properties":{"exchange":{"const":"binance"},"market_type":{"enum":["spot","perpetual"]}}},
                {"properties":{"exchange":{"const":"hyperliquid"},"market_type":{"const":"perpetual"}}},
                {"properties":{"exchange":{"enum":["kraken","okx","coinbase","bybit"]},"market_type":{"const":"spot"}}}
            ]
        }),
        output_schema: json!({
            "type":"object",
            "properties":{
                "exchange":{"type":"string"},"market_type":{"enum":["spot","perpetual"]},
                "market":{"type":"object","properties":{"id":{"type":"string"},"type":{"enum":["spot","perpetual"]},"base":{"type":["string","null"]},"quote":{"type":["string","null"]}},"required":["id","type","base","quote"],"additionalProperties":false},
                "bids":{"type":"array","items":{"$ref":"#/$defs/level"}},
                "asks":{"type":"array","items":{"$ref":"#/$defs/level"}},
                "observed_at":{"type":"string","format":"date-time"},"venue_timestamp":{"type":["string","null"],"format":"date-time"},"source":{"type":"string"}
            },
            "$defs":{"level":{"type":"object","properties":{"price":{"type":"string"},"quantity":{"type":"string"}},"required":["price","quantity"],"additionalProperties":false}},
            "required":["exchange","market_type","market","bids","asks","observed_at","venue_timestamp","source"],"additionalProperties":false
        }),
        error_schema: serde_json::to_value(schemars::schema_for!(GoFinError)).expect("error schema"),
        examples: vec![
            json!({"exchange":"binance","market_type":"spot","pair":"BTCUSDT","depth":20}),
            json!({"exchange":"hyperliquid","market_type":"perpetual","pair":"BTC","depth":20}),
            json!({"exchange":"okx","market_type":"spot","pair":"BTC-USDT","depth":20}),
        ],
        coverage: "Fixture-tested public order books: Binance spot and USDⓈ-M perpetual, Kraken spot, Hyperliquid primary perpetual, OKX spot, Coinbase Advanced spot, and Bybit spot. Each request first verifies the exact native identifier and market scope against venue metadata. No credentials or writes.".into(),
        completion: "Returns a timestamped public depth snapshot; it is not an execution guarantee or authoritative financial accounting.".into(),
        semantics: json!({"library":"ccxt-rust 4.5.85 implicit public endpoints","finImplemented":["exchange.order-book"],"writes":false,"credentials":false,"exactness":"Raw response decimals are emitted as strings. Observed time is request start. Venue timestamp is null unless the venue supplied a valid timestamp.","marketScopes":{"binance":["spot","perpetual"],"kraken":["spot"],"hyperliquid":["perpetual; primary dex"],"okx":["spot"],"coinbase":["spot"],"bybit":["spot"]}}),
    }
}

fn invalid() -> GoFinError {
    GoFinError::new(
        "VENUE_RESPONSE_INVALID",
        "read",
        "Exchange order-book response did not match the requested market or depth.",
        "Inspect the exact market identifier and supported market type with exchange.markets.",
    )
}

/// Some venue APIs accept only a small set of depth values. Ask for the next
/// supported size, then trim the exact response to the user's requested bound.
pub(crate) fn venue_depth(exchange: &str, depth: u16) -> u16 {
    match exchange {
        "binance" => [5, 10, 20, 50, 100]
            .into_iter()
            .find(|n| *n >= depth)
            .unwrap_or(100),
        "bybit" if depth == 1 => 1,
        "bybit" if depth <= 50 => 50,
        "bybit" => 200,
        "hyperliquid" => 20,
        _ => depth,
    }
}

fn market_row<'a>(
    exchange: &str,
    market_type: &str,
    pair: &str,
    metadata: &'a Value,
) -> Result<&'a Value, GoFinError> {
    match exchange {
        "binance" => {
            let rows = metadata["symbols"].as_array().ok_or_else(invalid)?;
            let mut matches = rows.iter().filter(|r| r["symbol"] == pair);
            let row = matches.next().ok_or_else(invalid)?;
            if matches.next().is_some()
                || row["baseAsset"].as_str().is_none_or(str::is_empty)
                || row["quoteAsset"].as_str().is_none_or(str::is_empty)
            {
                return Err(invalid());
            }
            if row["status"] != "TRADING"
                || (market_type == "perpetual" && row["contractType"] != "PERPETUAL")
                || (market_type == "spot" && row["contractType"] == "PERPETUAL")
            {
                return Err(invalid());
            }
            Ok(row)
        }
        "kraken" => metadata["result"]
            .as_object()
            .and_then(|m| m.get(pair))
            .ok_or_else(invalid),
        "hyperliquid" => {
            let rows = metadata
                .as_array()
                .filter(|v| v.len() == 2)
                .and_then(|v| v[0]["universe"].as_array())
                .ok_or_else(invalid)?;
            let row = rows
                .iter()
                .find(|r| r["name"] == pair)
                .ok_or_else(invalid)?;
            if row["isDelisted"] == true {
                return Err(invalid());
            }
            Ok(row)
        }
        "okx" => {
            if metadata["code"] != "0" {
                return Err(invalid());
            }
            let rows = metadata["data"].as_array().ok_or_else(invalid)?;
            let row = rows
                .iter()
                .find(|r| r["instId"] == pair && r["instType"] == "SPOT" && r["state"] == "live")
                .ok_or_else(invalid)?;
            Ok(row)
        }
        "coinbase" => {
            if metadata["product_id"] == pair
                && metadata["product_type"] == "SPOT"
                && metadata["status"] == "online"
            {
                return Ok(metadata);
            }
            if metadata.get("product").is_some() {
                let row = &metadata["product"];
                if row["product_id"] == pair
                    && row["product_type"] == "SPOT"
                    && row["status"] == "online"
                {
                    return Ok(row);
                }
                return Err(invalid());
            }
            let rows = metadata["products"].as_array().ok_or_else(invalid)?;
            let row = rows
                .iter()
                .find(|r| {
                    r["product_id"] == pair
                        && r["product_type"] == "SPOT"
                        && r["status"] == "online"
                })
                .ok_or_else(invalid)?;
            Ok(row)
        }
        "bybit" => {
            if metadata["retCode"] != 0 || metadata["result"]["category"] != "spot" {
                return Err(invalid());
            }
            let rows = metadata["result"]["list"].as_array().ok_or_else(invalid)?;
            let row = rows
                .iter()
                .find(|r| r["symbol"] == pair && r["status"] == "Trading")
                .ok_or_else(invalid)?;
            Ok(row)
        }
        _ => Err(invalid()),
    }
}

fn identity(exchange: &str, market_type: &str, pair: &str, row: &Value) -> Value {
    let (base, quote) = match exchange {
        "binance" => (row["baseAsset"].as_str(), row["quoteAsset"].as_str()),
        "kraken" => (row["base"].as_str(), row["quote"].as_str()),
        "hyperliquid" => (row["name"].as_str(), Some("USDC")),
        "okx" => (row["baseCcy"].as_str(), row["quoteCcy"].as_str()),
        "coinbase" => (
            row["base_currency_id"].as_str(),
            row["quote_currency_id"].as_str(),
        ),
        "bybit" => (row["baseCoin"].as_str(), row["quoteCoin"].as_str()),
        _ => (None, None),
    };
    json!({"id":pair,"type":market_type,"base":base,"quote":quote})
}

fn decimal(value: &Value) -> Option<String> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_number().map(ToString::to_string))?;
    if text.is_empty() || text.len() > 128 {
        return None;
    }
    let (mantissa, exponent) = text
        .split_once(['e', 'E'])
        .map_or((text.as_str(), None), |(m, e)| (m, Some(e)));
    if mantissa.starts_with('-') || mantissa.is_empty() {
        return None;
    }
    let mut dot = false;
    let mut digits = 0;
    let mut nonzero = false;
    for byte in mantissa.bytes() {
        if byte == b'.' && !dot {
            dot = true;
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        digits += 1;
        nonzero |= byte != b'0';
    }
    if digits == 0 || !nonzero {
        return None;
    }
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if exponent.is_empty() || !exponent.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(text)
}

fn venue_level(row: &Value) -> (Option<String>, Option<String>) {
    let values = row.as_array();
    let price = values
        .and_then(|v| v.first())
        .or_else(|| row.get("price"))
        .or_else(|| row.get("px"));
    let quantity = values
        .and_then(|v| v.get(1))
        .or_else(|| row.get("size"))
        .or_else(|| row.get("sz"));
    (price.and_then(decimal), quantity.and_then(decimal))
}

fn decimal_cmp(left: &str, right: &str) -> Result<std::cmp::Ordering, GoFinError> {
    fn parts(value: &str) -> Result<(String, i32), GoFinError> {
        let (mantissa, exponent) = match value.split_once(['e', 'E']) {
            Some((m, e)) => (m, e.parse::<i32>().map_err(|_| invalid())?),
            None => (value, 0),
        };
        if exponent.unsigned_abs() > 10_000 {
            return Err(invalid());
        }
        let fraction = mantissa.split_once('.').map_or(0, |(_, f)| f.len()) as i32;
        let digits = mantissa
            .bytes()
            .filter(u8::is_ascii_digit)
            .map(char::from)
            .collect::<String>();
        let significant = digits.trim_start_matches('0').to_owned();
        if significant.is_empty() {
            return Err(invalid());
        }
        Ok((significant, exponent - fraction))
    }
    let (left_digits, left_exp) = parts(left)?;
    let (right_digits, right_exp) = parts(right)?;
    let left_magnitude = left_digits.len() as i32 + left_exp;
    let right_magnitude = right_digits.len() as i32 + right_exp;
    if left_magnitude != right_magnitude {
        return Ok(left_magnitude.cmp(&right_magnitude));
    }
    let width = left_digits.len().max(right_digits.len());
    for index in 0..width {
        let a = left_digits.as_bytes().get(index).copied().unwrap_or(b'0');
        let b = right_digits.as_bytes().get(index).copied().unwrap_or(b'0');
        if a != b {
            return Ok(a.cmp(&b));
        }
    }
    Ok(std::cmp::Ordering::Equal)
}

fn levels(
    rows: &Value,
    depth: u16,
    venue_limit: u16,
    bids: bool,
) -> Result<Vec<Value>, GoFinError> {
    let rows = rows.as_array().ok_or_else(invalid)?;
    if rows.len() > venue_limit as usize {
        return Err(invalid());
    }
    let result = rows
        .iter()
        .take(depth as usize)
        .map(|row| {
            let (price, quantity) = venue_level(row);
            Ok(json!({"price":price.ok_or_else(invalid)?,"quantity":quantity.ok_or_else(invalid)?}))
        })
        .collect::<Result<Vec<_>, GoFinError>>()?;
    for pair in result.windows(2) {
        let order = decimal_cmp(
            pair[0]["price"].as_str().ok_or_else(invalid)?,
            pair[1]["price"].as_str().ok_or_else(invalid)?,
        )?;
        if (bids && order.is_lt()) || (!bids && order.is_gt()) {
            return Err(invalid());
        }
    }
    Ok(result)
}

fn venue_timestamp(value: &Value) -> Result<Option<String>, GoFinError> {
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
            return Ok(Some(
                parsed
                    .with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ));
        }
        if let Ok(millis) = text.parse::<i64>() {
            return chrono::DateTime::from_timestamp_millis(millis)
                .map(|d| Some(d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
                .ok_or_else(invalid);
        }
        return Err(invalid());
    }
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_i64()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| Some(d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
        .ok_or_else(invalid)
}

pub(crate) fn normalize_order_book(
    exchange: &str,
    market_type: &str,
    pair: &str,
    depth: u16,
    metadata: Value,
    raw: Value,
    observed_at: String,
) -> Result<Value, GoFinError> {
    let market = market_row(exchange, market_type, pair, &metadata)?;
    let (bid_rows, ask_rows, ts) = match exchange {
        "binance" => {
            if raw["symbol"].is_string() && raw["symbol"] != pair {
                return Err(invalid());
            }
            (
                &raw["bids"],
                &raw["asks"],
                raw.get("T")
                    .or_else(|| raw.get("E"))
                    .cloned()
                    .unwrap_or(Value::Null),
            )
        }
        "kraken" => {
            if !raw["error"].as_array().is_some_and(Vec::is_empty) {
                return Err(invalid());
            }
            let rows = raw["result"].as_object().ok_or_else(invalid)?;
            if rows.len() != 1 {
                return Err(invalid());
            }
            let (id, book) = rows.iter().next().ok_or_else(invalid)?;
            if id != pair {
                return Err(invalid());
            }
            (&book["bids"], &book["asks"], Value::Null)
        }
        "hyperliquid" => {
            if raw["coin"] != pair {
                return Err(invalid());
            }
            (&raw["levels"][0], &raw["levels"][1], raw["time"].clone())
        }
        "okx" => {
            if raw["code"] != "0" {
                return Err(invalid());
            }
            let rows = raw["data"]
                .as_array()
                .filter(|rows| rows.len() == 1)
                .ok_or_else(invalid)?;
            if rows[0].get("instId").is_some_and(|id| id != pair) {
                return Err(invalid());
            }
            (&rows[0]["bids"], &rows[0]["asks"], rows[0]["ts"].clone())
        }
        "coinbase" => {
            let book = raw.get("pricebook").unwrap_or(&raw[""]);
            if book["product_id"] != pair {
                return Err(invalid());
            }
            (&book["bids"], &book["asks"], book["time"].clone())
        }
        "bybit" => {
            if raw["retCode"] != 0 || raw["result"]["s"] != pair {
                return Err(invalid());
            }
            (
                &raw["result"]["b"],
                &raw["result"]["a"],
                raw["time"].clone(),
            )
        }
        _ => return Err(invalid()),
    };
    let venue_limit = venue_depth(exchange, depth);
    let bids = levels(bid_rows, depth, venue_limit, true)?;
    let asks = levels(ask_rows, depth, venue_limit, false)?;
    let venue_timestamp = venue_timestamp(&ts)?;
    let source = match exchange {
        "binance" => "Binance public REST depth",
        "kraken" => "Kraken public REST depth",
        "hyperliquid" => "Hyperliquid public info l2Book",
        "okx" => "OKX public REST books",
        "coinbase" => "Coinbase Advanced public product book",
        "bybit" => "Bybit public v5 orderbook",
        _ => "public exchange order book",
    };
    Ok(
        json!({"exchange":exchange,"market_type":market_type,"market":identity(exchange,market_type,pair,market),"bids":bids,"asks":asks,"observed_at":observed_at,"venue_timestamp":venue_timestamp,"source":source}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultExchangeProfile, ExchangeAdapter, PublicExchangeData};
    use async_trait::async_trait;
    use gofin_domain::Adapter;
    use std::sync::Arc;

    struct Fixtures;
    #[async_trait]
    impl PublicExchangeData for Fixtures {
        async fn markets(&self, _: &str) -> Result<Value, GoFinError> {
            unreachable!()
        }
        async fn ticker(&self, _: &str, _: &str) -> Result<Value, GoFinError> {
            unreachable!()
        }
        async fn order_book(
            &self,
            exchange: &str,
            market_type: &str,
            pair: &str,
            depth: u16,
        ) -> Result<(Value, Value, String), GoFinError> {
            let (metadata, book) = match exchange {
                "binance" => (json!({"symbols":[{"symbol":"BTCUSDT","status":"TRADING","contractType":if market_type == "perpetual" {"PERPETUAL"} else {"SPOT"},"baseAsset":"BTC","quoteAsset":"USDT"}]}), json!({"symbol":pair,"lastUpdateId":9,"bids":[["12345.123456789012345678","0.000000000000000123"]],"asks":[]})),
                "kraken" => (json!({"error":[],"result":{"XXBTZUSD":{"base":"XXBT","quote":"ZUSD"}}}), serde_json::from_str(r#"{"error":[],"result":{"XXBTZUSD":{"bids":[[12345.123456789012345678,0.000000000000000123,1]],"asks":[]}}}"#).unwrap()),
                "hyperliquid" => (json!([{"universe":[{"name":"BTC"}]},[]]), json!({"coin":"BTC","time":1700000000000u64,"levels":[[{"px":"12345.123456789012345678","sz":"0.000000000000000123"}],[]]})),
                "okx" => (json!({"code":"0","data":[{"instId":"BTC-USDT","instType":"SPOT","state":"live","baseCcy":"BTC","quoteCcy":"USDT"}]}), json!({"code":"0","data":[{"ts":"1700000000000","bids":[["12345.123456789012345678","0.000000000000000123","2"]],"asks":[]}]})),
                "coinbase" => (json!({"product":{"product_id":"BTC-USD","product_type":"SPOT","status":"online","base_currency_id":"BTC","quote_currency_id":"USD"}}), json!({"pricebook":{"product_id":"BTC-USD","bids":[{"price":"12345.123456789012345678","size":"0.000000000000000123"}],"asks":[]}})),
                "bybit" => (json!({"retCode":0,"result":{"category":"spot","list":[{"symbol":"BTCUSDT","status":"Trading","baseCoin":"BTC","quoteCoin":"USDT"}]}}), json!({"retCode":0,"time":"1700000000000","result":{"s":"BTCUSDT","b":[["12345.123456789012345678","0.000000000000000123"]],"a":[]}})),
                _ => unreachable!(),
            };
            assert!((1..=100).contains(&depth));
            Ok((metadata, book, "2026-10-08T00:00:00Z".into()))
        }
    }

    #[tokio::test]
    async fn fixture_transport_covers_every_enabled_venue_and_preserves_exact_decimals() {
        let adapter = ExchangeAdapter::new(DefaultExchangeProfile::new(Arc::new(Fixtures)));
        for (exchange, market_type, pair) in [
            ("binance", "spot", "BTCUSDT"),
            ("binance", "perpetual", "BTCUSDT"),
            ("kraken", "spot", "XXBTZUSD"),
            ("hyperliquid", "perpetual", "BTC"),
            ("okx", "spot", "BTC-USDT"),
            ("coinbase", "spot", "BTC-USD"),
            ("bybit", "spot", "BTCUSDT"),
        ] {
            let value = adapter
                .read(
                    "exchange.order-book",
                    &json!({"exchange":exchange,"market_type":market_type,"pair":pair,"depth":5}),
                )
                .await
                .unwrap_or_else(|e| panic!("{exchange}: {e}"));
            assert_eq!(value["market"]["id"], pair);
            assert_eq!(value["market_type"], market_type);
            assert_eq!(value["bids"][0]["price"], "12345.123456789012345678");
            assert_eq!(value["bids"][0]["quantity"], "0.000000000000000123");
            assert_eq!(value["asks"], json!([]));
            assert!(value["source"].is_string());
        }
    }

    #[test]
    fn rejects_identity_conflicts_in_metadata_and_book_envelopes() {
        let metadata =
            json!({"code":"0","data":[{"instId":"BTC-USDT","instType":"SPOT","state":"live"}]});
        let conflicting = json!({"code":"0","data":[{"instId":"ETH-USDT","bids":[],"asks":[]}]});
        assert!(
            normalize_order_book(
                "okx",
                "spot",
                "BTC-USDT",
                10,
                metadata.clone(),
                conflicting,
                "2026-10-08T00:00:00Z".into()
            )
            .is_err()
        );
        let conflict_metadata =
            json!({"code":"0","data":[{"instId":"ETH-USDT","instType":"SPOT","state":"live"}]});
        let book = json!({"code":"0","data":[{"instId":"BTC-USDT","bids":[],"asks":[]}]});
        assert!(
            normalize_order_book(
                "okx",
                "spot",
                "BTC-USDT",
                10,
                conflict_metadata,
                book,
                "2026-10-08T00:00:00Z".into()
            )
            .is_err()
        );
    }

    #[test]
    fn validates_market_type_depth_level_counts_and_decimal_values() {
        for (exchange, market_type, pair, depth) in [
            ("hyperliquid", "spot", "BTC", 10),
            ("kraken", "perpetual", "XBTUSD", 10),
            ("okx", "spot", "BTC-USDT", 0),
            ("bybit", "spot", "BTCUSDT", 101),
            ("binance", "spot", "BTC USDT", 10),
        ] {
            assert!(
                validate_order_book_input(&OrderBookInput {
                    exchange: exchange.into(),
                    market_type: market_type.into(),
                    pair: pair.into(),
                    depth
                })
                .is_err()
            );
        }
        assert!(
            validate_order_book_input(&OrderBookInput {
                exchange: "binance".into(),
                market_type: "spot".into(),
                pair: "日本BTC".into(),
                depth: 1
            })
            .is_ok()
        );
        let metadata = json!({"symbols":[{"symbol":"BTCUSDT","status":"TRADING","contractType":"SPOT","baseAsset":"BTC","quoteAsset":"USDT"}]});
        let above_requested = json!({"bids":[["2","1"],["1","1"]],"asks":[]});
        let trimmed = normalize_order_book(
            "binance",
            "spot",
            "BTCUSDT",
            1,
            metadata.clone(),
            above_requested,
            "t".into(),
        )
        .unwrap();
        assert_eq!(trimmed["bids"].as_array().unwrap().len(), 1);
        assert_eq!(venue_depth("binance", 1), 5);
        assert_eq!(venue_depth("binance", 7), 10);
        assert_eq!(venue_depth("bybit", 2), 50);
        assert_eq!(venue_depth("bybit", 51), 200);
        let above_venue_limit =
            json!({"bids":[["6","1"],["5","1"],["4","1"],["3","1"],["2","1"],["1","1"]],"asks":[]});
        assert!(
            normalize_order_book(
                "binance",
                "spot",
                "BTCUSDT",
                1,
                metadata.clone(),
                above_venue_limit,
                "t".into()
            )
            .is_err()
        );
        let zero = json!({"bids":[["0","1"]],"asks":[]});
        assert!(
            normalize_order_book("binance", "spot", "BTCUSDT", 1, metadata, zero, "t".into())
                .is_err()
        );
    }

    #[test]
    fn missing_and_old_optional_venue_timestamps_are_reported_honestly() {
        let metadata = json!({"symbols":[{"symbol":"BTCUSDT","status":"TRADING","contractType":"SPOT","baseAsset":"BTC","quoteAsset":"USDT"}]});
        let duplicate = json!({"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT"},{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT"}]});
        assert!(
            normalize_order_book(
                "binance",
                "spot",
                "BTCUSDT",
                1,
                duplicate,
                json!({"bids":[],"asks":[]}),
                "t".into()
            )
            .is_err()
        );
        let no_time = json!({"bids":[],"asks":[]});
        let missing = normalize_order_book(
            "binance",
            "spot",
            "BTCUSDT",
            10,
            metadata.clone(),
            no_time,
            "2026-10-08T00:00:00Z".into(),
        )
        .unwrap();
        assert!(missing["venue_timestamp"].is_null());
        let old = json!({"T":1000,"bids":[],"asks":[]});
        let stale = normalize_order_book(
            "binance",
            "spot",
            "BTCUSDT",
            10,
            metadata,
            old,
            "2026-10-08T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(stale["venue_timestamp"], "1970-01-01T00:00:01.000Z");
        let malformed = json!({"T":"not-a-time","bids":[],"asks":[]});
        assert!(normalize_order_book("binance", "spot", "BTCUSDT", 10, json!({"symbols":[{"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT"}]}), malformed, "2026-10-08T00:00:00Z".into()).is_err());
    }

    #[test]
    fn hyperliquid_books_are_trimmed_to_requested_depth_and_exactly_sorted() {
        let metadata = json!([{"universe":[{"name":"BTC"}]},[]]);
        let bids = (0..20)
            .map(|i| json!({"px":(100-i).to_string(),"sz":"1.000"}))
            .collect::<Vec<_>>();
        let raw = json!({"coin":"BTC","levels":[bids,[]]});
        let book = normalize_order_book(
            "hyperliquid",
            "perpetual",
            "BTC",
            1,
            metadata,
            raw,
            "2026-10-08T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(book["bids"].as_array().unwrap().len(), 1);
        let wrong_order =
            json!({"coin":"BTC","levels":[[{"px":"99","sz":"1"},{"px":"100","sz":"1"}],[]]});
        let metadata = json!([{"universe":[{"name":"BTC"}]},[]]);
        assert!(
            normalize_order_book(
                "hyperliquid",
                "perpetual",
                "BTC",
                2,
                metadata,
                wrong_order,
                "t".into()
            )
            .is_err()
        );
    }
}
