//! Exchange-local USD prices; on-chain pricing delegates unchanged to its source.
use super::*;
use fin_domain::Quantity;
use std::collections::{BTreeSet, HashSet};

mod binance;
pub use binance::BinanceSpot;
#[cfg(test)]
mod tests;

const MARKET_SECONDS: i64 = 30;
const BOOK_CACHE_SECONDS: i64 = 10;
const MAX_BOOK_AGE_SECONDS: i64 = 30;

#[derive(Clone, Debug)]
pub struct SpotBook {
    pub market_id: String,
    pub bid: String,
    pub ask: String,
    pub bid_quantity: String,
    pub ask_quantity: String,
    /// Snapshot observation time, retained unchanged through cache hits.
    pub observed_at: String,
}

#[async_trait]
pub trait SpotMarketSource: Send + Sync {
    async fn markets(&self, exchange: &str) -> Result<Vec<ExchangeMarket>>;
    async fn books(&self, exchange: &str, markets: &[String]) -> Result<Vec<SpotBook>>;
}

#[derive(Clone, Debug)]
pub struct StablecoinRates {
    pub usdc: UsdQuote,
    pub usdt: UsdQuote,
}

#[async_trait]
pub trait StablecoinUsdSource: Send + Sync {
    /// One refresh obtains both reviewed currencies together; no parity fallback.
    async fn stablecoin_rates(&self) -> Result<StablecoinRates>;
}

struct CachedMarkets {
    observed: DateTime<Utc>,
    rows: Vec<ExchangeMarket>,
}
#[derive(Default)]
struct VenueCache {
    markets: BTreeMap<String, CachedMarkets>,
    books: BTreeMap<(String, String), SpotBook>,
}
struct CachedRates {
    fetched: DateTime<Utc>,
    rates: StablecoinRates,
}

pub struct ExchangeSpotPrices {
    onchain: Arc<dyn UsdPriceSource>,
    conversions: Arc<dyn StablecoinUsdSource>,
    venues: BTreeMap<String, Arc<dyn SpotMarketSource>>,
    cache: Mutex<VenueCache>,
    rates: Mutex<Option<CachedRates>>,
    #[cfg(test)]
    clock: Option<Arc<std::sync::atomic::AtomicI64>>,
}

impl ExchangeSpotPrices {
    pub fn new(provider: CoinGeckoPrices) -> Result<Self> {
        let provider = Arc::new(provider);
        Ok(Self::with_sources(
            provider.clone(),
            provider,
            Arc::new(BinanceSpot::new()?),
        ))
    }
    /// Injectable sources share the same production selection/derivation/cache path.
    pub fn with_sources(
        onchain: Arc<dyn UsdPriceSource>,
        conversions: Arc<dyn StablecoinUsdSource>,
        binance: Arc<dyn SpotMarketSource>,
    ) -> Self {
        Self {
            onchain,
            conversions,
            venues: BTreeMap::from([("binance".into(), binance)]),
            cache: Mutex::new(VenueCache::default()),
            rates: Mutex::new(None),
            #[cfg(test)]
            clock: None,
        }
    }
    pub fn with_exchange_source(
        mut self,
        exchange: &str,
        source: Arc<dyn SpotMarketSource>,
    ) -> Result<Self> {
        resolution::validate_identifier(exchange)?;
        self.venues.insert(exchange.into(), source);
        Ok(self)
    }
    fn now(&self) -> DateTime<Utc> {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return DateTime::from_timestamp(clock.load(std::sync::atomic::Ordering::SeqCst), 0)
                .expect("synthetic clock");
        }
        Utc::now()
    }
    async fn rates(&self) -> Result<StablecoinRates> {
        let mut cache = self.rates.lock().await;
        let now = self.now();
        if let Some(cached) = cache.as_ref()
            && fresh(cached.fetched, now, CACHE_SECONDS)
            && validate_rates(&cached.rates, now).is_ok()
        {
            return Ok(cached.rates.clone());
        }
        *cache = None;
        let rates = self
            .conversions
            .stablecoin_rates()
            .await
            .map_err(|mut error| {
                error.message = format!("Stablecoin/USD conversion leg: {}", error.message);
                error
            })?;
        let fetched = self.now();
        validate_rates(&rates, fetched)?;
        *cache = Some(CachedRates {
            fetched,
            rates: rates.clone(),
        });
        Ok(rates)
    }
    async fn exchange_quotes(&self, assets: &[AssetId]) -> Result<Vec<UsdQuote>> {
        // Coalesce market and book requests across concurrent actions.
        let mut cache = self.cache.lock().await;
        let mut chosen = Vec::with_capacity(assets.len());
        for asset in assets {
            resolution::validate_identifier(&asset.reference)?;
            if asset.decimals > 36 {
                return Err(spot_error(
                    "PRICE_IDENTITY_INVALID",
                    "Exchange asset scale exceeds supported precision.",
                ));
            }
            let exchange = resolution::exchange_name(&asset.network).ok_or_else(|| {
                spot_error(
                    "PRICE_EXCHANGE_UNSUPPORTED",
                    "No spot-price source is registered for this exchange.",
                )
            })?;
            let source = self.venues.get(exchange).ok_or_else(|| {
                spot_error(
                    "PRICE_EXCHANGE_UNSUPPORTED",
                    "No spot-price source is registered for this exchange.",
                )
            })?;
            let now = self.now();
            if !cache
                .markets
                .get(exchange)
                .is_some_and(|entry| fresh(entry.observed, now, MARKET_SECONDS))
            {
                cache.markets.remove(exchange);
                cache.books.retain(|(venue, _), _| venue != exchange);
                let rows = source.markets(exchange).await.map_err(|mut error| {
                    error.message = format!("Spot metadata leg: {}", error.message);
                    error
                })?;
                validate_markets(&rows)?;
                cache.markets.insert(
                    exchange.into(),
                    CachedMarkets {
                        observed: self.now(),
                        rows,
                    },
                );
            }
            let rows = &cache.markets.get(exchange).expect("inserted metadata").rows;
            if matches!(asset.reference.as_str(), "USDC" | "USDT") {
                if !rows.iter().any(|row| {
                    row.active
                        && (row.base_asset == asset.reference || row.quote_asset == asset.reference)
                }) {
                    return Err(spot_error(
                        "PRICE_SPOT_PAIR_UNSUPPORTED",
                        "The exact stablecoin identity is absent from active venue metadata.",
                    ));
                }
                chosen.push((exchange.to_owned(), None, asset.reference.clone()));
                continue;
            }
            let market = select_pair(rows, &asset.reference)?;
            chosen.push((
                exchange.to_owned(),
                Some(market.market_id.clone()),
                market.quote_asset.clone(),
            ));
        }
        let mut missing = BTreeMap::<String, BTreeSet<String>>::new();
        for (exchange, market, _) in &chosen {
            if let Some(market) = market {
                let key = (exchange.clone(), market.clone());
                let reusable = cache.books.get(&key).is_some_and(|book| {
                    time(&book.observed_at)
                        .is_ok_and(|at| fresh(at, self.now(), BOOK_CACHE_SECONDS))
                        && validate_book(book, self.now()).is_ok()
                });
                if !reusable {
                    cache.books.remove(&key);
                    missing
                        .entry(exchange.clone())
                        .or_default()
                        .insert(market.clone());
                }
            }
        }
        for (exchange, markets) in missing {
            let markets = markets.into_iter().collect::<Vec<_>>();
            let source = self.venues.get(&exchange).expect("registered source");
            for batch in markets.chunks(16) {
                let books = source.books(&exchange, batch).await?;
                let mut seen = HashSet::new();
                for book in &books {
                    if !batch.contains(&book.market_id) || !seen.insert(&book.market_id) {
                        return Err(spot_error(
                            "PRICE_SPOT_DATA_INVALID",
                            "Spot snapshots contain an unexpected or duplicate exact market identity.",
                        ));
                    }
                    validate_book(book, self.now())?;
                }
                if books.len() != batch.len() {
                    return Err(spot_error(
                        "PRICE_SPOT_QUOTE_MISSING",
                        "The selected spot-price leg is missing; no alternate pair was substituted.",
                    ));
                }
                for book in books {
                    cache
                        .books
                        .insert((exchange.clone(), book.market_id.clone()), book);
                }
            }
        }
        let rates = self.rates().await?;
        let now = self.now();
        validate_rates(&rates, now)?;
        assets.iter().zip(chosen).map(|(asset, (exchange, market, currency))| {
            let rate = if currency == "USDC" { &rates.usdc } else { &rates.usdt };
            let fx_time = time(&rate.updated_at)?;
            if let Some(market) = market {
                let book = cache.books.get(&(exchange.clone(), market.clone())).expect("validated book");
                validate_book(book, now)?;
                let spot_time = time(&book.observed_at)?;
                Ok(UsdQuote {
                    usd_per_unit: multiply(&book.ask, &rate.usd_per_unit)?,
                    updated_at: spot_time.min(fx_time).to_rfc3339(),
                    observed_at: book.observed_at.clone(),
                    source: format!("Spot best ask; exchange={exchange}; exact_asset={}; venue_market={market}; quote_asset={currency}; updated:{}; USD conversion: {}; updated:{}; conversion retrieval; updated:{}", asset.reference, book.observed_at, rate.source, rate.updated_at, rate.observed_at),
                })
            } else {
                let mut quote = rate.clone();
                quote.source = format!("Stablecoin/USD conversion; exchange={exchange}; exact_asset={}; {}; updated:{}; conversion retrieval; updated:{}", asset.reference, rate.source, rate.updated_at, rate.observed_at);
                Ok(quote)
            }
        }).collect()
    }
}

#[async_trait]
impl UsdPriceSource for ExchangeSpotPrices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        self.quotes(std::slice::from_ref(asset))
            .await?
            .into_iter()
            .next()
            .ok_or_else(invalid_price)
    }
    async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<UsdQuote>> {
        if assets.len() > 64 {
            return Err(spot_error(
                "PRICE_REQUEST_TOO_LARGE",
                "At most 64 exact assets may be priced in one action.",
            ));
        }
        let mut exchanges = Vec::new();
        let mut chains = Vec::new();
        for asset in assets {
            if resolution::exchange_name(&asset.network).is_some() {
                exchanges.push(asset.clone());
            } else {
                chains.push(asset.clone());
            }
        }
        let exchange_quotes = if exchanges.is_empty() {
            vec![]
        } else {
            self.exchange_quotes(&exchanges).await?
        };
        let chain_quotes = if chains.is_empty() {
            vec![]
        } else {
            self.onchain.quotes(&chains).await?
        };
        if chain_quotes.len() != chains.len() {
            return Err(invalid_price());
        }
        let mut exchanges = exchange_quotes.into_iter();
        let mut chains = chain_quotes.into_iter();
        assets
            .iter()
            .map(|asset| {
                if resolution::exchange_name(&asset.network).is_some() {
                    exchanges.next()
                } else {
                    chains.next()
                }
                .ok_or_else(invalid_price)
            })
            .collect()
    }
}

fn spot_error(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "valuation",
        message,
        "Use an active direct USDC/USDT spot pair and fresh prices for both legs; no fallback route or stablecoin parity is assumed.",
    )
}
fn time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|_| {
            spot_error(
                "PRICE_LEG_TIME_INVALID",
                "A pricing leg has an invalid observation time.",
            )
        })
}
fn fresh(at: DateTime<Utc>, now: DateTime<Utc>, seconds: i64) -> bool {
    let age = now - at;
    age >= chrono::Duration::zero() && age < chrono::Duration::seconds(seconds)
}
fn validate_rates(rates: &StablecoinRates, now: DateTime<Utc>) -> Result<()> {
    for (currency, rate) in [("USDC", &rates.usdc), ("USDT", &rates.usdt)] {
        let updated = time(&rate.updated_at)?;
        let observed = time(&rate.observed_at)?;
        if !fresh(
            updated,
            now,
            fin_domain::price_freshness::STABLECOIN_USD_MAX_AGE_SECONDS,
        ) || !fresh(
            observed,
            now,
            fin_domain::price_freshness::STABLECOIN_USD_MAX_AGE_SECONDS,
        ) || updated > observed
        {
            return Err(spot_error(
                "PRICE_CONVERSION_STALE",
                &format!(
                    "The {currency}/USD conversion leg is stale, future-dated or has inconsistent observation times."
                ),
            ));
        }
        exact_decimal(&rate.usd_per_unit).map_err(|_| {
            spot_error(
                "PRICE_CONVERSION_INVALID",
                &format!("The {currency}/USD conversion leg needs a positive exact price."),
            )
        })?;
        if rate.source.is_empty() {
            return Err(spot_error(
                "PRICE_CONVERSION_MISSING",
                "The stablecoin/USD conversion leg lacks source evidence.",
            ));
        }
    }
    Ok(())
}
fn validate_book(book: &SpotBook, now: DateTime<Utc>) -> Result<()> {
    if !fresh(time(&book.observed_at)?, now, MAX_BOOK_AGE_SECONDS) {
        return Err(spot_error(
            "PRICE_SPOT_STALE",
            "The selected spot-price leg is stale or future-dated.",
        ));
    }
    let invalid = || {
        spot_error(
            "PRICE_SPOT_DATA_INVALID",
            "The spot-price leg requires positive exact bid, ask and quantities.",
        )
    };
    let bid = Quantity::from_decimal(&exact_decimal(&book.bid).map_err(|_| invalid())?, 36)?;
    let ask = Quantity::from_decimal(&exact_decimal(&book.ask).map_err(|_| invalid())?, 36)?;
    exact_decimal(&book.bid_quantity).map_err(|_| invalid())?;
    exact_decimal(&book.ask_quantity).map_err(|_| invalid())?;
    if bid > ask {
        return Err(spot_error(
            "PRICE_SPOT_DATA_INVALID",
            "The spot book is crossed.",
        ));
    }
    Ok(())
}
fn validate_markets(rows: &[ExchangeMarket]) -> Result<()> {
    if rows.is_empty() || rows.len() > 20_000 {
        return Err(spot_error(
            "PRICE_SPOT_DATA_INVALID",
            "Spot market metadata is empty or exceeds its bounds.",
        ));
    }
    let mut ids = HashSet::new();
    for row in rows {
        for field in [&row.market_id, &row.base_asset, &row.quote_asset] {
            resolution::validate_identifier(field)?;
        }
        if !ids.insert(&row.market_id) || row.base_asset == row.quote_asset {
            return Err(spot_error(
                "PRICE_SPOT_IDENTITY_CONFLICT",
                "Spot metadata contains duplicate or inconsistent exact market identities.",
            ));
        }
    }
    Ok(())
}
fn select_pair<'a>(rows: &'a [ExchangeMarket], asset: &str) -> Result<&'a ExchangeMarket> {
    for currency in ["USDC", "USDT"] {
        let selected: Vec<_> = rows
            .iter()
            .filter(|row| row.active && row.base_asset == asset && row.quote_asset == currency)
            .collect();
        match selected.as_slice() {
            [row] => return Ok(row),
            [] => {}
            _ => {
                return Err(spot_error(
                    "PRICE_SPOT_IDENTITY_CONFLICT",
                    "Multiple active markets claim the same exact base/quote pair.",
                ));
            }
        }
    }
    Err(spot_error(
        "PRICE_SPOT_PAIR_UNSUPPORTED",
        "The exact exchange asset has no active direct USDC or USDT spot pair.",
    ))
}
fn multiply(a: &str, b: &str) -> Result<String> {
    let a = Quantity::from_decimal(&exact_decimal(a)?, 36)?;
    let b = Quantity::from_decimal(&exact_decimal(b)?, 36)?;
    let product = Quantity::new((a.integer() * b.integer()).to_string())?.to_decimal(72);
    exact_decimal(&product).map_err(|_| spot_error("PRICE_CONVERSION_PRECISION_UNSUPPORTED", "The exact price-leg product exceeds the supported decimal precision; it was not rounded."))
}
