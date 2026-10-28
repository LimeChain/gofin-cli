use super::*;
use serde_json::Value;

const ADVANCED: &str = "https://api.coinbase.com/api/v3/brokerage/market";
const EXCHANGE: &str = "https://api.exchange.coinbase.com";
const INTERNATIONAL: &str = "https://api.international.coinbase.com/api/v1";
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_PAGES: usize = 200;

/// Public Advanced Trade spot metadata and books. Exact numeric JSON lexemes
/// are retained by serde_json's arbitrary_precision feature; no float path is
/// used to produce the price strings consumed by the shared valuation layer.
pub struct CoinbaseSpot {
    client: reqwest::Client,
}
impl CoinbaseSpot {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| {
                spot_error(
                    "PRICE_UNAVAILABLE",
                    "Coinbase public market transport is unavailable.",
                )
            })?;
        Ok(Self { client })
    }
    async fn get(&self, base: &str, path: &str) -> Result<Value> {
        let url = format!("{base}{path}");
        let mut response = self.client.get(url).send().await.map_err(|_| {
            spot_error(
                "PRICE_UNAVAILABLE",
                "Coinbase public market data is unavailable.",
            )
        })?;
        if !response.status().is_success() {
            return Err(spot_error(
                "PRICE_UNAVAILABLE",
                "Coinbase public market data rejected the request.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return Err(spot_error(
                "PRICE_DATA_INVALID",
                "Coinbase market response exceeded its bound.",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            spot_error(
                "PRICE_UNAVAILABLE",
                "Coinbase public market response was incomplete.",
            )
        })? {
            if chunk.len() > MAX_BYTES.saturating_sub(bytes.len()) {
                return Err(spot_error(
                    "PRICE_DATA_INVALID",
                    "Coinbase market response exceeded its bound.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            spot_error(
                "PRICE_DATA_INVALID",
                "Coinbase public market response was malformed.",
            )
        })
    }
}
#[async_trait]
impl SpotMarketSource for CoinbaseSpot {
    async fn markets(&self, exchange: &str) -> Result<Vec<ExchangeMarket>> {
        if exchange == "coinbaseinternational" {
            return international_markets(&self.get(INTERNATIONAL, "/instruments").await?);
        }
        if !matches!(exchange, "coinbase" | "coinbaseexchange") {
            return Err(spot_error(
                "PRICE_EXCHANGE_UNSUPPORTED",
                "Coinbase spot source received a different venue identity.",
            ));
        }
        let mut cursor: Option<String> = None;
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        for _ in 0..MAX_PAGES {
            let path = cursor
                .as_ref()
                .map(|c| {
                    format!(
                        "/products?limit=250&cursor={}",
                        percent_encoding::utf8_percent_encode(
                            c,
                            percent_encoding::NON_ALPHANUMERIC
                        )
                    )
                })
                .unwrap_or_else(|| "/products?limit=250".into());
            let body = if exchange == "coinbase" {
                self.get(ADVANCED, &path).await?
            } else {
                self.get(EXCHANGE, "/products").await?
            };
            let products = if exchange == "coinbase" {
                body.get("products").and_then(Value::as_array)
            } else {
                body.as_array()
            }
            .ok_or_else(|| {
                spot_error(
                    "PRICE_DATA_INVALID",
                    "Coinbase product metadata is incomplete.",
                )
            })?;
            for item in products {
                let market_id = item
                    .get(if exchange == "coinbase" {
                        "product_id"
                    } else {
                        "id"
                    })
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        spot_error(
                            "PRICE_DATA_INVALID",
                            "Coinbase product identity is missing.",
                        )
                    })?
                    .to_owned();
                let base = item
                    .get(if exchange == "coinbase" {
                        "base_currency_id"
                    } else {
                        "base_currency"
                    })
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        spot_error(
                            "PRICE_DATA_INVALID",
                            "Coinbase base currency identity is missing.",
                        )
                    })?
                    .to_owned();
                let quote = item
                    .get(if exchange == "coinbase" {
                        "quote_currency_id"
                    } else {
                        "quote_currency"
                    })
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        spot_error(
                            "PRICE_DATA_INVALID",
                            "Coinbase quote currency identity is missing.",
                        )
                    })?
                    .to_owned();
                if !seen.insert(market_id.clone()) {
                    return Err(spot_error(
                        "PRICE_DATA_INVALID",
                        "Coinbase product pagination repeated a market.",
                    ));
                }
                let kind = item
                    .get("product_type")
                    .and_then(Value::as_str)
                    .unwrap_or("SPOT");
                let status = item.get("status").and_then(Value::as_str).unwrap_or("");
                let online = if exchange == "coinbase" {
                    status.is_empty() || status == "online"
                } else {
                    status == "online"
                };
                rows.push(ExchangeMarket {
                    market_id,
                    base_asset: base,
                    quote_asset: quote,
                    active: (exchange == "coinbaseexchange" || kind == "SPOT")
                        && online
                        && item.get("is_disabled").and_then(Value::as_bool) != Some(true),
                });
            }
            cursor = if exchange == "coinbase" {
                body.get("cursor")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned)
            } else {
                None
            };
            if cursor.is_none() {
                return Ok(rows);
            }
        }
        Err(spot_error(
            "PRICE_DATA_INVALID",
            "Coinbase product pagination exceeded its page bound.",
        ))
    }
    async fn books(&self, exchange: &str, markets: &[String]) -> Result<Vec<SpotBook>> {
        if exchange == "coinbaseinternational" {
            let mut books = Vec::with_capacity(markets.len());
            for market in markets {
                resolution::validate_identifier(market)?;
                let path = format!(
                    "/instruments/{}/quote",
                    percent_encoding::utf8_percent_encode(
                        market,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                );
                let quote = self.get(INTERNATIONAL, &path).await?;
                books.push(international_book(market, &quote)?);
            }
            return Ok(books);
        }
        if !matches!(exchange, "coinbase" | "coinbaseexchange") {
            return Err(spot_error(
                "PRICE_EXCHANGE_UNSUPPORTED",
                "Coinbase spot source received a different venue identity.",
            ));
        }
        let mut output = Vec::with_capacity(markets.len());
        for market in markets {
            if market.is_empty() || market.len() > 128 || market.chars().any(char::is_control) {
                return Err(spot_error(
                    "PRICE_IDENTITY_INVALID",
                    "Coinbase market identifier is invalid.",
                ));
            }
            let (bid, ask) = if exchange == "coinbase" {
                let path = format!(
                    "/product_book?product_id={}",
                    percent_encoding::utf8_percent_encode(
                        market,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                );
                let body = self.get(ADVANCED, &path).await?;
                let book = body.get("pricebook").ok_or_else(|| {
                    spot_error("PRICE_DATA_INVALID", "Coinbase order book is incomplete.")
                })?;
                (
                    level(
                        book.get("bids")
                            .and_then(Value::as_array)
                            .and_then(|a| a.first()),
                    )?,
                    level(
                        book.get("asks")
                            .and_then(Value::as_array)
                            .and_then(|a| a.first()),
                    )?,
                )
            } else {
                let path = format!(
                    "/products/{}/book?level=2",
                    percent_encoding::utf8_percent_encode(
                        market,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                );
                let body = self.get(EXCHANGE, &path).await?;
                (
                    legacy_level(
                        body.get("bids")
                            .and_then(Value::as_array)
                            .and_then(|a| a.first()),
                    )?,
                    legacy_level(
                        body.get("asks")
                            .and_then(Value::as_array)
                            .and_then(|a| a.first()),
                    )?,
                )
            };
            let bid_price = Quantity::from_decimal(&bid.0, 36).map_err(|_| {
                spot_error(
                    "PRICE_DATA_INVALID",
                    "Coinbase bid is not an exact decimal.",
                )
            })?;
            let ask_price = Quantity::from_decimal(&ask.0, 36).map_err(|_| {
                spot_error(
                    "PRICE_DATA_INVALID",
                    "Coinbase ask is not an exact decimal.",
                )
            })?;
            if bid_price.is_zero() || ask_price.is_zero() || bid_price > ask_price {
                return Err(spot_error(
                    "PRICE_DATA_INVALID",
                    "Coinbase top of book is crossed or non-positive.",
                ));
            }
            output.push(SpotBook {
                market_id: market.clone(),
                bid: bid.0,
                ask: ask.0,
                bid_quantity: bid.1,
                ask_quantity: ask.1,
                observed_at: Utc::now().to_rfc3339(),
            });
        }
        Ok(output)
    }
}
fn international_markets(body: &Value) -> Result<Vec<ExchangeMarket>> {
    let rows = body.as_array().ok_or_else(|| {
        spot_error(
            "PRICE_DATA_INVALID",
            "International instrument metadata is incomplete.",
        )
    })?;
    rows.iter()
        .map(|row| {
            let text = |key: &str| {
                row[key]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        spot_error(
                            "PRICE_DATA_INVALID",
                            "International instrument identity is missing.",
                        )
                    })
            };
            Ok(ExchangeMarket {
                market_id: text("symbol")?,
                base_asset: text("base_asset_name")?,
                quote_asset: text("quote_asset_name")?,
                active: text("type")? == "SPOT" && text("trading_state")? == "TRADING",
            })
        })
        .collect()
}
fn international_book(market: &str, quote: &Value) -> Result<SpotBook> {
    let decimal = |key: &str| match &quote[key] {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(spot_error(
            "PRICE_DATA_INVALID",
            "International quote omitted an exact price or quantity.",
        )),
    };
    let observed_at = quote["timestamp"].as_str().ok_or_else(|| {
        spot_error(
            "PRICE_DATA_INVALID",
            "International quote omitted its native timestamp.",
        )
    })?;
    let book = SpotBook {
        market_id: market.into(),
        bid: decimal("best_bid_price")?,
        ask: decimal("best_ask_price")?,
        bid_quantity: decimal("best_bid_size")?,
        ask_quantity: decimal("best_ask_size")?,
        observed_at: observed_at.into(),
    };
    validate_book(&book, Utc::now())?;
    Ok(book)
}
fn level(row: Option<&Value>) -> Result<(String, String)> {
    let row = row.ok_or_else(|| {
        spot_error(
            "PRICE_DATA_INVALID",
            "Coinbase order book has no top level.",
        )
    })?;
    let decimal = |key: &str| -> Result<String> {
        match row.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(Value::Number(n)) => Ok(n.to_string()),
            _ => Err(spot_error(
                "PRICE_DATA_INVALID",
                "Coinbase order book level is missing an exact decimal.",
            )),
        }
    };
    let price = decimal("price")?;
    let quantity = decimal("size")?;
    if Quantity::from_decimal(&price, 36).map_or(true, |q| q.is_zero())
        || Quantity::from_decimal(&quantity, 36).map_or(true, |q| q.is_zero())
    {
        return Err(spot_error(
            "PRICE_DATA_INVALID",
            "Coinbase order book level is non-positive.",
        ));
    }
    Ok((price, quantity))
}
fn legacy_level(row: Option<&Value>) -> Result<(String, String)> {
    let row = row.and_then(Value::as_array).ok_or_else(|| {
        spot_error(
            "PRICE_DATA_INVALID",
            "Coinbase Exchange order book has no top level.",
        )
    })?;
    let price = row
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| {
            spot_error(
                "PRICE_DATA_INVALID",
                "Coinbase Exchange bid or ask price is malformed.",
            )
        })?
        .to_owned();
    let quantity = row
        .get(1)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            spot_error(
                "PRICE_DATA_INVALID",
                "Coinbase Exchange bid or ask size is malformed.",
            )
        })?
        .to_owned();
    if Quantity::from_decimal(&price, 36).map_or(true, |q| q.is_zero())
        || Quantity::from_decimal(&quantity, 36).map_or(true, |q| q.is_zero())
    {
        return Err(spot_error(
            "PRICE_DATA_INVALID",
            "Coinbase Exchange top level is non-positive.",
        ));
    }
    Ok((price, quantity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn international_metadata_separates_spot_from_perpetual_and_halted_markets() {
        let rows = international_markets(&json!([
            {"symbol":"BTC-USDC","type":"SPOT","base_asset_name":"BTC","quote_asset_name":"USDC","trading_state":"TRADING"},
            {"symbol":"BTC-PERP","type":"PERP","base_asset_name":"BTC","quote_asset_name":"USDC","trading_state":"TRADING"},
            {"symbol":"ETH-USDC","type":"SPOT","base_asset_name":"ETH","quote_asset_name":"USDC","trading_state":"HALT"}
        ])).unwrap();
        assert_eq!(
            rows.iter()
                .filter(|row| row.active)
                .map(|row| row.market_id.as_str())
                .collect::<Vec<_>>(),
            vec!["BTC-USDC"]
        );
    }
}
