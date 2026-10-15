//! Public exact Kraken market metadata and depth; no credential access.
use super::*;
use serde_json::Value;

const MAX_METADATA_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

fn response_limit(endpoint: &str) -> Result<usize> {
    match endpoint {
        "AssetPairs" => Ok(MAX_METADATA_RESPONSE_BYTES),
        "Depth" => Ok(MAX_RESPONSE_BYTES),
        _ => Err(invalid_price()),
    }
}
fn oversized() -> GoFinError {
    spot_error(
        "PRICE_DATA_OVERSIZED",
        "The Kraken public pricing response exceeds its endpoint-specific byte bound.",
    )
}
fn decode_response(endpoint: &str, bytes: &[u8]) -> Result<Value> {
    if bytes.len() > response_limit(endpoint)? {
        return Err(oversized());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid_price())?;
    if value
        .get("error")
        .and_then(Value::as_array)
        .is_none_or(|errors| !errors.is_empty())
    {
        return Err(unavailable());
    }
    value.get("result").cloned().ok_or_else(invalid_price)
}

pub struct KrakenSpot {
    client: reqwest::Client,
}
impl KrakenSpot {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .https_only(true)
                .no_proxy()
                .build()
                .map_err(|_| invalid_price())?,
        })
    }
    async fn get(&self, endpoint: &str, parameters: &[(&str, String)]) -> Result<Value> {
        let limit = response_limit(endpoint)?;
        let mut response = self
            .client
            .get(format!("https://api.kraken.com/0/public/{endpoint}"))
            .query(parameters)
            .send()
            .await
            .map_err(|_| unavailable())?;
        if !response.status().is_success() {
            return Err(unavailable());
        }
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err(oversized());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > limit - bytes.len() {
                return Err(oversized());
            }
            bytes.extend_from_slice(&chunk);
        }
        decode_response(endpoint, &bytes)
    }
}
fn unavailable() -> GoFinError {
    spot_error(
        "PRICE_VENUE_UNAVAILABLE",
        "The Kraken public pricing leg failed; no substitute price was used.",
    )
}

fn markets(result: &Value) -> Result<Vec<ExchangeMarket>> {
    let rows = result.as_object().ok_or_else(invalid_price)?;
    if rows.len() > 20_000 {
        return Err(invalid_price());
    }
    rows.iter()
        .map(|(id, row)| {
            Ok(ExchangeMarket {
                market_id: id.clone(),
                base_asset: row["base"].as_str().ok_or_else(invalid_price)?.into(),
                quote_asset: row["quote"].as_str().ok_or_else(invalid_price)?.into(),
                active: row["status"].as_str() == Some("online"),
            })
        })
        .collect()
}
fn book(result: &Value, market: &str, observed: &str) -> Result<SpotBook> {
    let rows = result.as_object().ok_or_else(invalid_price)?;
    // The selected request uses the exact native market key, never an alias.
    if rows.len() != 1 {
        return Err(invalid_price());
    }
    let row = rows.get(market).ok_or_else(invalid_price)?;
    let best = |side: &str| -> Result<(String, String)> {
        let level = row[side]
            .as_array()
            .and_then(|levels| levels.first())
            .and_then(Value::as_array)
            .ok_or_else(invalid_price)?;
        if level.len() != 3 {
            return Err(invalid_price());
        }
        let price = level[0].as_str().ok_or_else(invalid_price)?;
        let quantity = level[1].as_str().ok_or_else(invalid_price)?;
        exact_decimal(price)?;
        exact_decimal(quantity)?;
        Ok((price.into(), quantity.into()))
    };
    let (bid, bid_quantity) = best("bids")?;
    let (ask, ask_quantity) = best("asks")?;
    let book = SpotBook {
        market_id: market.into(),
        bid,
        ask,
        bid_quantity,
        ask_quantity,
        observed_at: observed.into(),
    };
    validate_book(&book, Utc::now())?;
    Ok(book)
}
#[async_trait]
impl SpotMarketSource for KrakenSpot {
    async fn markets(&self, exchange: &str) -> Result<Vec<ExchangeMarket>> {
        if exchange != "kraken" {
            return Err(invalid_price());
        }
        markets(&self.get("AssetPairs", &[]).await?)
    }
    async fn books(&self, exchange: &str, selected: &[String]) -> Result<Vec<SpotBook>> {
        if exchange != "kraken" || selected.is_empty() || selected.len() > 16 {
            return Err(invalid_price());
        }
        let mut books = Vec::with_capacity(selected.len());
        for market in selected {
            resolution::validate_identifier(market)?;
            let observed = Utc::now().to_rfc3339();
            let result = self
                .get("Depth", &[("pair", market.clone()), ("count", "1".into())])
                .await?;
            books.push(book(&result, market, &observed)?);
        }
        Ok(books)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_catalog_has_its_own_bound_while_depth_and_business_failures_stay_closed() {
        let mut rows = serde_json::Map::new();
        for index in 0..2000 {
            rows.insert(
                format!("SYNTHETIC{index}"),
                serde_json::json!({
                    "base":"XXBT","quote":"ZUSD","status":"online","ordermin":"0.0001",
                    "fees":[[0,"0.4"],[10000,"0.35"],[50000,"0.3"],[100000,"0.25"],[250000,"0.2"]],
                    "fees_maker":[[0,"0.25"],[10000,"0.2"],[50000,"0.15"],[100000,"0.12"]]
                }),
            );
        }
        let bytes = serde_json::to_vec(&serde_json::json!({"error":[],"result":rows})).unwrap();
        assert!(bytes.len() > MAX_RESPONSE_BYTES);
        assert_eq!(
            markets(&decode_response("AssetPairs", &bytes).unwrap())
                .unwrap()
                .len(),
            2000
        );
        assert_eq!(
            decode_response("Depth", &bytes).unwrap_err().code,
            "PRICE_DATA_OVERSIZED"
        );
        assert_eq!(
            decode_response("AssetPairs", &vec![b' '; MAX_METADATA_RESPONSE_BYTES + 1])
                .unwrap_err()
                .code,
            "PRICE_DATA_OVERSIZED"
        );
        assert!(decode_response("UnreviewedPath", b"{}").is_err());
        assert_eq!(
            decode_response(
                "AssetPairs",
                br#"{"error":["EService:Unavailable"],"result":{}}"#
            )
            .unwrap_err()
            .code,
            "PRICE_VENUE_UNAVAILABLE"
        );
    }
    #[test]
    fn native_ids_and_exact_books_are_preserved_without_symbol_parity() {
        let rows=markets(&serde_json::json!({"XXBTZUSD":{"base":"XXBT","quote":"ZUSD","status":"online","altname":"XBTUSD"},"XXBTUSDC":{"base":"XXBT","quote":"USDC","status":"cancel_only"}})).unwrap();
        let usdc = rows.iter().find(|row| row.market_id == "XXBTUSDC").unwrap();
        let usd = rows.iter().find(|row| row.market_id == "XXBTZUSD").unwrap();
        assert_eq!(usdc.base_asset, "XXBT");
        assert!(!usdc.active);
        assert_eq!(usd.quote_asset, "ZUSD");
        assert!(usd.active);
        let result = serde_json::json!({"XXBTZUSD":{"bids":[["123.123456789012345678901234567890123456","2.1",1]],"asks":[["124","2",1]]}});
        let quote = book(&result, "XXBTZUSD", &Utc::now().to_rfc3339()).unwrap();
        assert_eq!(quote.bid, "123.123456789012345678901234567890123456");
        assert!(book(&result, "XBTUSD", &Utc::now().to_rfc3339()).is_err());
        assert!(
            markets(&serde_json::json!({"SYNTHETIC":{"base":"XBT","status":"online"}})).is_err()
        );
    }
}
