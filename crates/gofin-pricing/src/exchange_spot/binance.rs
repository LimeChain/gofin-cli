//! Public Binance spot snapshots only; this client has no authentication material.
use super::*;

pub struct BinanceSpot {
    client: reqwest::Client,
    markets: resolution::BinanceMarkets,
    cooldown: Mutex<Option<DateTime<Utc>>>,
    #[cfg(test)]
    pub(super) book_url: Option<String>,
}
impl BinanceSpot {
    #[cfg(test)]
    pub(crate) fn test_urls(mut self, metadata: String, books: String) -> Self {
        self.markets.test_url = Some(metadata);
        self.book_url = Some(books);
        self
    }
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("gofin-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .https_only(!cfg!(test))
            .no_proxy()
            .build()
            .map_err(|_| {
                spot_error(
                    "PRICE_VENUE_UNAVAILABLE",
                    "Public spot pricing transport could not initialize.",
                )
            })?;
        Ok(Self {
            markets: resolution::BinanceMarkets::new(client.clone()),
            client,
            cooldown: Mutex::new(None),
            #[cfg(test)]
            book_url: None,
        })
    }
}
#[async_trait]
impl SpotMarketSource for BinanceSpot {
    async fn markets(&self, exchange: &str) -> Result<Vec<ExchangeMarket>> {
        self.markets.markets(exchange).await
    }
    async fn books(&self, exchange: &str, markets: &[String]) -> Result<Vec<SpotBook>> {
        if exchange != "binance" || markets.is_empty() || markets.len() > 16 {
            return Err(spot_error(
                "PRICE_EXCHANGE_UNSUPPORTED",
                "Unsupported spot venue or bounded market request.",
            ));
        }
        if self
            .cooldown
            .lock()
            .await
            .is_some_and(|until| Utc::now() < until)
        {
            return Err(spot_error(
                "PRICE_VENUE_RATE_LIMITED",
                "The spot-price leg is in its venue Retry-After cooldown.",
            ));
        }
        let base = "https://api.binance.com/api/v3/ticker/bookTicker";
        #[cfg(test)]
        let base = self.book_url.as_deref().unwrap_or(base);
        let mut url = reqwest::Url::parse(base).map_err(|_| invalid_price())?;
        url.query_pairs_mut()
            .append_pair(
                "symbols",
                &serde_json::to_string(markets).map_err(|_| invalid_price())?,
            )
            .append_pair("symbolStatus", "TRADING");
        // REST bookTicker has no event timestamp. Bound freshness from the start
        // of the live snapshot request, never from cache retrieval/derivation.
        let observed = Utc::now();
        let mut response = self.client.get(url).send().await.map_err(|_| {
            spot_error(
                "PRICE_VENUE_UNAVAILABLE",
                "The Binance spot-price leg failed or timed out.",
            )
        })?;
        if matches!(response.status().as_u16(), 429 | 418) {
            let seconds = resolution::retry_after_seconds(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
                Utc::now(),
            );
            *self.cooldown.lock().await = Some(Utc::now() + chrono::Duration::seconds(seconds));
            return Err(spot_error(
                "PRICE_VENUE_RATE_LIMITED",
                "Binance rate limited the spot-price leg; Retry-After is honored.",
            ));
        }
        if response.status().as_u16() != 200 {
            return Err(spot_error(
                "PRICE_VENUE_UNAVAILABLE",
                "Binance rejected the selected spot-price request.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(spot_error(
                "PRICE_SPOT_DATA_INVALID",
                "Spot quote response exceeds its bounds.",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            spot_error(
                "PRICE_VENUE_UNAVAILABLE",
                "Spot snapshot response was interrupted.",
            )
        })? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(spot_error(
                    "PRICE_SPOT_DATA_INVALID",
                    "Spot quote response exceeds its bounds.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Book {
            symbol: String,
            bid_price: String,
            bid_qty: String,
            ask_price: String,
            ask_qty: String,
        }
        let rows: Vec<Book> = serde_json::from_slice(&bytes).map_err(|_| {
            spot_error(
                "PRICE_SPOT_DATA_INVALID",
                "Spot quote response lacks exact string prices, quantities or market identities.",
            )
        })?;
        if rows.len() > markets.len() {
            return Err(spot_error(
                "PRICE_SPOT_DATA_INVALID",
                "Unexpected spot snapshot coverage.",
            ));
        }
        Ok(rows
            .into_iter()
            .map(|row| SpotBook {
                market_id: row.symbol,
                bid: row.bid_price,
                ask: row.ask_price,
                bid_quantity: row.bid_qty,
                ask_quantity: row.ask_qty,
                observed_at: observed.to_rfc3339(),
            })
            .collect())
    }
}
