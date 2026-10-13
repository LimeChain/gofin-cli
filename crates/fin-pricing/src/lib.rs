//! Shared exact public USD quotes. Anonymous CoinGecko access is best effort.
//! Protected callers may supply a Demo or Pro key in process; this crate loads
//! no key from environment/account configuration and never signs or trades.
//!
//! Provider numbers are captured as JSON RawValue lexemes before conversion.
//! No floating-point value or stablecoin parity assumption enters valuation.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fin_domain::{AssetId, FinError, Result};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize, de};
use serde_json::value::RawValue;
use std::{collections::BTreeMap, fmt, sync::Arc, time::Duration};

pub mod exchange_spot;
pub mod resolution;
pub use exchange_spot::{
    ExchangeSpotPrices, SpotBook, SpotMarketSource, StablecoinRates, StablecoinUsdSource,
    require_kraken_tokenized_quote_unit,
};
pub use resolution::{
    ExchangeAssetKey, ExchangeAssetResolver, ExchangeMarket, ExchangeMarketSource, IdentityStore,
    ResolvedExchangeAsset,
};
use resolution::{IdentityCache, RegisteredExchange};
use tokio::sync::Mutex;

const BASE_URL: &str = "https://api.coingecko.com/api/v3";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_CACHE_KEYS: usize = 128;
const CACHE_SECONDS: i64 = 30;
const MAX_PROVIDER_AGE_SECONDS: i64 = 120;
const MAX_FRACTION_DIGITS: i32 = 36;
const MAX_INTEGER_DIGITS: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiPlan {
    Demo,
    Pro,
}

impl ApiPlan {
    fn base_url(self) -> &'static str {
        match self {
            Self::Demo => BASE_URL,
            Self::Pro => "https://pro-api.coingecko.com/api/v3",
        }
    }
    fn header_name(self) -> &'static str {
        match self {
            Self::Demo => "x-cg-demo-api-key",
            Self::Pro => "x-cg-pro-api-key",
        }
    }
}

fn valid_api_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 256
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsdQuote {
    /// Exact positive USD per whole asset unit, canonical plain decimal text.
    pub usd_per_unit: String,
    /// Provider time, or the oldest source-leg time for a derived price.
    pub updated_at: String,
    /// Time this response was retrieved; cache hits retain the original value.
    pub observed_at: String,
    pub source: String,
}

#[async_trait]
impl StablecoinUsdSource for CoinGeckoPrices {
    async fn stablecoin_rates(&self) -> Result<StablecoinRates> {
        // Reviewed identities for the two supported USD-conversion currencies.
        // This is never a per-exchange symbol search or exchange-catalog lookup.
        let lookups = [
            Lookup::Coin("usd-coin".into()),
            Lookup::Coin("tether".into()),
        ];
        let bytes = self.bounded_request(&batch_path(&lookups)).await?;
        let observed = self.now();
        let leg_error = |currency: &str, mut error: FinError| {
            error.message = format!("{currency}/USD conversion leg: {}", error.message);
            error
        };
        Ok(StablecoinRates {
            usdc: parse_quote_with_age(
                &bytes,
                &lookups[0],
                observed,
                fin_domain::price_freshness::STABLECOIN_USD_MAX_AGE_SECONDS,
            )
            .map_err(|error| leg_error("USDC", error))?,
            usdt: parse_quote_with_age(
                &bytes,
                &lookups[1],
                observed,
                fin_domain::price_freshness::STABLECOIN_USD_MAX_AGE_SECONDS,
            )
            .map_err(|error| leg_error("USDT", error))?,
        })
    }
}

#[async_trait]
pub trait UsdPriceSource: Send + Sync {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote>;
    /// Return quotes in input order; providers may batch exact identities.
    async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<UsdQuote>> {
        let mut quotes = Vec::with_capacity(assets.len());
        for asset in assets {
            quotes.push(self.quote(asset).await?);
        }
        Ok(quotes)
    }
}

fn failure(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "valuation",
        message,
        "Retry a read with a supported asset and fresh CoinGecko data; no fallback price is assumed.",
    )
}

fn invalid_price() -> FinError {
    failure(
        "PRICE_DATA_INVALID",
        "A positive bounded exact USD number with at most 36 fractional places is required.",
    )
}

/// Normalize an unsigned JSON decimal or exponent using decimal digit shifts.
/// Bound all allocations before applying a shift, and never round a quote.
fn exact_decimal(raw: &str) -> Result<String> {
    if raw.is_empty() || raw.len() > 256 || raw.starts_with(['-', '+']) {
        return Err(invalid_price());
    }
    let mut exponent_parts = raw.split(['e', 'E']);
    let mantissa = exponent_parts.next().ok_or_else(invalid_price)?;
    let exponent = match exponent_parts.next() {
        None => 0,
        Some(text) => {
            let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
            if unsigned.is_empty()
                || unsigned.len() > 3
                || !unsigned.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid_price());
            }
            let exponent = text.parse::<i32>().map_err(|_| invalid_price())?;
            if exponent.abs() > 256 {
                return Err(invalid_price());
            }
            exponent
        }
    };
    if exponent_parts.next().is_some() {
        return Err(invalid_price());
    }
    let mut parts = mantissa.split('.');
    let integer = parts.next().ok_or_else(invalid_price)?;
    let fraction = parts.next();
    if parts.next().is_some()
        || integer.is_empty()
        || (integer.len() > 1 && integer.starts_with('0'))
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || fraction
            .is_some_and(|text| text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(invalid_price());
    }
    let fraction = fraction.unwrap_or("");
    let digits = format!("{integer}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Err(invalid_price());
    }
    let coefficient = digits.trim_end_matches('0');
    let power = exponent - fraction.len() as i32 + (digits.len() - coefficient.len()) as i32;
    let fractional_places = (-power).max(0);
    let integer_places = (coefficient.len() as i32 + power).max(1) as usize;
    if fractional_places > MAX_FRACTION_DIGITS || integer_places > MAX_INTEGER_DIGITS {
        return Err(invalid_price());
    }
    if power >= 0 {
        return Ok(format!("{coefficient}{}", "0".repeat(power as usize)));
    }
    let at = coefficient.len() as i32 + power;
    if at > 0 {
        let at = at as usize;
        Ok(format!("{}.{}", &coefficient[..at], &coefficient[at..]))
    } else {
        Ok(format!("0.{}{coefficient}", "0".repeat((-at) as usize)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Lookup {
    Coin(String),
    Token(&'static str, String),
}

impl Lookup {
    fn asset(asset: &AssetId) -> Result<Self> {
        let unsupported = || {
            failure(
                "PRICE_ASSET_UNSUPPORTED",
                "Asset has no registered exchange resolver or reviewed EVM network/contract identity.",
            )
        };
        // Platform IDs and native_coin_id=ethereum were verified against the
        // provider /asset_platforms for chain IDs 1, 42161, 4663 and 8453.
        // https://docs.coingecko.com/reference/asset-platforms-list
        let platform = match asset.network.as_str() {
            "ethereum:1" => "ethereum",
            "ethereum:42161" => "arbitrum-one",
            "ethereum:4663" => "robinhood",
            "ethereum:8453" => "base",
            _ => return Err(unsupported()),
        };
        if asset.reference == "native" {
            return if asset.decimals == 18 {
                Ok(Self::Coin("ethereum".into()))
            } else {
                Err(unsupported())
            };
        }
        let digits = asset.reference.strip_prefix("0x").ok_or_else(unsupported)?;
        if digits.len() != 40
            || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            || digits.bytes().all(|byte| byte == b'0')
        {
            return Err(unsupported());
        }
        Ok(Self::Token(platform, asset.reference.to_ascii_lowercase()))
    }

    fn response_key(&self) -> &str {
        match self {
            Self::Coin(id) => id,
            Self::Token(_, contract) => contract,
        }
    }

    fn path(&self) -> String {
        let (path, field, identity) = match self {
            Self::Coin(id) => ("/simple/price".to_owned(), "ids", id.as_str()),
            Self::Token(platform, contract) => (
                format!("/simple/token_price/{platform}"),
                "contract_addresses",
                contract.as_str(),
            ),
        };
        let mut url = reqwest::Url::parse("https://pricing.invalid").expect("fixed URL");
        url.set_path(&path);
        url.query_pairs_mut()
            .append_pair(field, identity)
            .append_pair("vs_currencies", "usd")
            .append_pair("include_last_updated_at", "true")
            .append_pair("precision", "full");
        format!("{}?{}", url.path(), url.query().expect("fixed query"))
    }

    fn source(&self) -> String {
        match self {
            Self::Coin(id) => format!("CoinGecko public USD quote; coin_id={id}"),
            Self::Token(platform, contract) => {
                format!("CoinGecko public USD quote; platform={platform}; contract={contract}")
            }
        }
    }
}

#[derive(Deserialize)]
struct PriceFields {
    usd: Box<RawValue>,
    last_updated_at: i64,
}

// Reject duplicate top-level identities rather than accepting whichever value
// happens to appear last. Nested required fields reject duplicates via derive.
struct PriceResponse(BTreeMap<String, Box<RawValue>>);
impl<'de> Deserialize<'de> for PriceResponse {
    fn deserialize<D: de::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct ResponseVisitor;
        impl<'de> de::Visitor<'de> for ResponseVisitor {
            type Value = PriceResponse;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded map of unique quote identities")
            }
            fn visit_map<M: de::MapAccess<'de>>(
                self,
                mut entries: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut map = BTreeMap::new();
                while let Some((key, value)) = entries.next_entry::<String, Box<RawValue>>()? {
                    if map.len() == 16 || key.len() > 128 || map.insert(key, value).is_some() {
                        return Err(de::Error::custom("invalid quote identity map"));
                    }
                }
                Ok(PriceResponse(map))
            }
        }
        deserializer.deserialize_map(ResponseVisitor)
    }
}

fn provider_time(timestamp: i64, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    provider_time_with_age(timestamp, now, MAX_PROVIDER_AGE_SECONDS)
}
fn provider_time_with_age(
    timestamp: i64,
    now: DateTime<Utc>,
    max_age: i64,
) -> Result<DateTime<Utc>> {
    let updated = DateTime::from_timestamp(timestamp, 0)
        .filter(|_| timestamp > 0)
        .ok_or_else(|| {
            failure(
                "PRICE_DATA_INVALID",
                "Provider observation time is invalid.",
            )
        })?;
    if updated > now || now - updated >= chrono::Duration::seconds(max_age) {
        return Err(failure(
            "PRICE_STALE",
            &format!("CoinGecko quote is future-dated or at least {max_age} seconds old."),
        ));
    }
    Ok(updated)
}

fn parse_quote(bytes: &[u8], lookup: &Lookup, now: DateTime<Utc>) -> Result<UsdQuote> {
    parse_quote_with_age(bytes, lookup, now, MAX_PROVIDER_AGE_SECONDS)
}
fn parse_quote_with_age(
    bytes: &[u8],
    lookup: &Lookup,
    now: DateTime<Utc>,
    max_age: i64,
) -> Result<UsdQuote> {
    let response = serde_json::from_slice::<PriceResponse>(bytes)
        .map_err(|_| failure("PRICE_DATA_INVALID", "Public quote response is malformed."))?;
    let raw = response.0.get(lookup.response_key()).ok_or_else(|| {
        failure(
            "PRICE_UNAVAILABLE",
            "CoinGecko returned no quote for the exact requested asset identity.",
        )
    })?;
    let fields = serde_json::from_str::<PriceFields>(raw.get()).map_err(|_| {
        failure(
            "PRICE_DATA_INVALID",
            "Public quote lacks a valid USD number or provider timestamp.",
        )
    })?;
    let usd_per_unit = exact_decimal(fields.usd.get())?;
    let updated = provider_time_with_age(fields.last_updated_at, now, max_age)?;
    Ok(UsdQuote {
        usd_per_unit,
        updated_at: updated.to_rfc3339(),
        observed_at: now.to_rfc3339(),
        source: lookup.source(),
    })
}

struct CachedQuote {
    quote: UsdQuote,
    fetched_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

pub struct CoinGeckoPrices {
    client: reqwest::Client,
    authentication: Option<(ApiPlan, HeaderValue)>,
    // Holding this mutex through a bounded fetch coalesces requests, including
    // native ETH shared across chains. Cancellation drops the lock/request and
    // inserts no incomplete result; another caller can retry.
    cache: Mutex<BTreeMap<Lookup, CachedQuote>>,
    exchanges: BTreeMap<String, RegisteredExchange>,
    identities: Mutex<IdentityCache>,
    rate_limited_until: Mutex<Option<DateTime<Utc>>>,
    next_request: Mutex<Option<tokio::time::Instant>>,
    pacing_seconds: std::sync::atomic::AtomicU64,
    identity_store: Option<Arc<dyn IdentityStore>>,
    #[cfg(test)]
    test_pacing: Option<Duration>,
    #[cfg(test)]
    test_rate_retry: bool,
    #[cfg(test)]
    test_base_url: Option<String>,
    #[cfg(test)]
    test_clock: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,
    #[cfg(test)]
    test_timeout: Option<Duration>,
}

impl CoinGeckoPrices {
    pub fn new() -> Result<Self> {
        Self::configured(None)
    }

    /// Called only by the protected credential owner. Keys never enter URLs,
    /// discovery, Debug output or an anonymous fallback on authorization failure.
    /// https://docs.coingecko.com/demo/reference/authentication
    /// https://docs.coingecko.com/reference/authentication
    pub fn with_api_key(plan: ApiPlan, key: &str) -> Result<Self> {
        if !valid_api_key(key) {
            return Err(failure(
                "PRICE_AUTHENTICATION_FAILED",
                "Configured CoinGecko API-key format is invalid.",
            ));
        }
        let mut value = HeaderValue::from_str(key).map_err(|_| {
            failure(
                "PRICE_AUTHENTICATION_FAILED",
                "Configured CoinGecko API-key format is invalid.",
            )
        })?;
        value.set_sensitive(true);
        Self::configured(Some((plan, value)))
    }

    fn configured(authentication: Option<(ApiPlan, HeaderValue)>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("fin-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .https_only(!cfg!(test))
            .no_proxy()
            .build()
            .map_err(|_| {
                failure(
                    "PRICE_UNAVAILABLE",
                    "Public CoinGecko transport could not be initialized.",
                )
            })?;
        let exchanges = BTreeMap::from([(
            "binance".into(),
            RegisteredExchange {
                provider_exchange: "binance".into(),
                source: Arc::new(resolution::BinanceMarkets::new(client.clone())),
            },
        )]);
        let pacing_seconds = if authentication.is_some() { 1 } else { 6 };
        Ok(Self {
            client,
            authentication,
            cache: Mutex::new(BTreeMap::new()),
            exchanges,
            identities: Mutex::new(IdentityCache::default()),
            rate_limited_until: Mutex::new(None),
            next_request: Mutex::new(None),
            pacing_seconds: std::sync::atomic::AtomicU64::new(pacing_seconds),
            identity_store: None,
            #[cfg(test)]
            test_pacing: None,
            #[cfg(test)]
            test_rate_retry: false,
            #[cfg(test)]
            test_base_url: None,
            #[cfg(test)]
            test_clock: None,
            #[cfg(test)]
            test_timeout: None,
        })
    }

    /// Register a venue metadata reader and this provider's exact exchange ID.
    /// This registers an integration, never an asset-symbol mapping or credentials.
    pub fn with_exchange_market_source(
        mut self,
        exchange: &str,
        provider_exchange: &str,
        source: Arc<dyn ExchangeMarketSource>,
    ) -> Result<Self> {
        resolution::validate_identifier(exchange)?;
        resolution::validate_identifier(provider_exchange)?;
        self.identities = Mutex::new(IdentityCache::default());
        self.exchanges.insert(
            exchange.into(),
            RegisteredExchange {
                provider_exchange: provider_exchange.into(),
                source,
            },
        );
        Ok(self)
    }

    /// Enrollment-only read. Quotes do not issue a separate ping, and this check
    /// proves API access, not quote freshness or access to every market product.
    /// https://docs.coingecko.com/reference/ping-server
    pub async fn validate_access(&self) -> Result<()> {
        let bytes = self.bounded_request("/ping").await?;
        #[derive(Deserialize)]
        struct Ping {
            gecko_says: String,
        }
        let ping: Ping = serde_json::from_slice(&bytes).map_err(|_| {
            failure(
                "PRICE_DATA_INVALID",
                "CoinGecko access check returned a malformed response.",
            )
        })?;
        if ping.gecko_says != "(V3) To the Moon!" {
            return Err(failure(
                "PRICE_DATA_INVALID",
                "CoinGecko access check returned an unexpected response.",
            ));
        }
        Ok(())
    }

    fn now(&self) -> DateTime<Utc> {
        #[cfg(test)]
        if let Some(clock) = &self.test_clock {
            return DateTime::from_timestamp(clock.load(std::sync::atomic::Ordering::SeqCst), 0)
                .expect("synthetic valid clock");
        }
        Utc::now()
    }

    async fn bounded_request(&self, path: &str) -> Result<Vec<u8>> {
        let timeout = REQUEST_TIMEOUT;
        #[cfg(test)]
        let timeout = self.test_timeout.unwrap_or(timeout);
        #[cfg(not(test))]
        let retries = 1;
        #[cfg(test)]
        let retries = usize::from(self.test_rate_retry);
        for attempt in 0..=retries {
            // Only public idempotent reads retry. Each wait and HTTP response
            // remains bounded; no financial submission or consent is replayed.
            if retries > 0 {
                let until = *self.rate_limited_until.lock().await;
                if let Some(until) = until {
                    let wait = until - self.now();
                    if wait > chrono::Duration::seconds(60) {
                        return Err(failure(
                            "PRICE_RATE_LIMITED",
                            "Provider cooldown exceeds the bounded read wait; retry after its window reopens.",
                        ));
                    }
                    if let Ok(wait) = wait.to_std() {
                        tokio::time::sleep(wait).await;
                    }
                }
            }
            self.request_slot().await?;
            let result = tokio::time::timeout(timeout, self.request_bytes(path))
                .await
                .map_err(|_| {
                    failure(
                        "PRICE_UNAVAILABLE",
                        "Public CoinGecko request exceeded its total timeout.",
                    )
                })?;
            if result
                .as_ref()
                .is_err_and(|error| error.code == "PRICE_RATE_LIMITED")
                && attempt < retries
            {
                continue;
            }
            return result;
        }
        unreachable!("bounded read attempt always returns")
    }

    async fn request_slot(&self) -> Result<()> {
        self.check_rate_limit().await?;
        let interval = Duration::from_secs(
            self.pacing_seconds
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        #[cfg(test)]
        let interval = self.test_pacing.unwrap_or(interval);
        let mut next = self.next_request.lock().await;
        if let Some(at) = *next {
            tokio::time::sleep_until(at).await;
        }
        self.check_rate_limit().await?;
        // Reserve only when sending: cancelling a queued read consumes no slot.
        *next = Some(tokio::time::Instant::now() + interval);
        Ok(())
    }

    async fn check_rate_limit(&self) -> Result<()> {
        if self
            .rate_limited_until
            .lock()
            .await
            .is_some_and(|until| self.now() < until)
        {
            return Err(failure(
                "PRICE_RATE_LIMITED",
                "CoinGecko Retry-After cooldown is active; retry when the provider window reopens.",
            ));
        }
        Ok(())
    }

    async fn request_bytes(&self, path: &str) -> Result<Vec<u8>> {
        self.check_rate_limit().await?;
        let base = self
            .authentication
            .as_ref()
            .map(|(plan, _)| plan.base_url())
            .unwrap_or(BASE_URL);
        #[cfg(test)]
        let base = self.test_base_url.as_deref().unwrap_or(base);
        let mut request = self.client.get(format!("{base}{path}"));
        if let Some((plan, value)) = &self.authentication {
            request = request.header(plan.header_name(), value.clone());
        }
        let mut response = request.send().await.map_err(|_| {
            failure(
                "PRICE_UNAVAILABLE",
                "Public CoinGecko request failed or timed out.",
            )
        })?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(failure(
                "PRICE_AUTHENTICATION_FAILED",
                "CoinGecko API access was rejected. Verify the protected key and selected plan.",
            ));
        }
        if response.status().as_u16() == 429 {
            let now = self.now();
            let seconds = resolution::retry_after_seconds(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok()),
                now,
            );
            *self.rate_limited_until.lock().await = Some(now + chrono::Duration::seconds(seconds));
            self.pacing_seconds
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |interval| Some((interval * 2).min(60)),
                )
                .ok();
            if let Ok(cache) = self.identities.try_lock() {
                self.save_identities(&cache)?;
            }

            return Err(failure(
                "PRICE_RATE_LIMITED",
                "CoinGecko access is rate limited; Retry-After is honored and no quote or partial identity mapping was accepted.",
            ));
        }
        if response.status().as_u16() != 200 {
            return Err(failure(
                "PRICE_UNAVAILABLE",
                "CoinGecko rejected the public quote request.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(failure(
                "PRICE_DATA_INVALID",
                "Public quote exceeds the bounded response size.",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            failure(
                "PRICE_UNAVAILABLE",
                "Public quote response failed or timed out.",
            )
        })? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(failure(
                    "PRICE_DATA_INVALID",
                    "Public quote exceeds the bounded response size.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[async_trait]
impl UsdPriceSource for CoinGeckoPrices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        self.quotes(std::slice::from_ref(asset))
            .await?
            .into_iter()
            .next()
            .ok_or_else(invalid_price)
    }

    async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<UsdQuote>> {
        if assets.len() > 64 {
            return Err(failure(
                "PRICE_REQUEST_TOO_LARGE",
                "At most 64 exact assets may be priced in one action.",
            ));
        }
        let mut selected = Vec::with_capacity(assets.len());
        for asset in assets {
            let identity = if let Some(exchange) = resolution::exchange_name(&asset.network) {
                Some(
                    self.resolve_exchange_asset(&ExchangeAssetKey {
                        exchange: exchange.into(),
                        asset: asset.reference.clone(),
                    })
                    .await?,
                )
            } else {
                None
            };
            let lookup = match &identity {
                Some(id) => Lookup::Coin(id.provider_asset_id.clone()),
                None => Lookup::asset(asset)?,
            };
            selected.push((lookup, identity));
        }
        let mut cache = self.cache.lock().await;
        let now = self.now();
        let mut missing = std::collections::BTreeSet::new();
        for (lookup, _) in &selected {
            let fresh = cache.get(lookup).is_some_and(|entry| {
                let elapsed = now - entry.fetched_at;
                elapsed >= chrono::Duration::zero()
                    && elapsed < chrono::Duration::seconds(CACHE_SECONDS)
                    && provider_time(entry.updated_at.timestamp(), now).is_ok()
            });
            if !fresh {
                cache.remove(lookup);
                missing.insert(lookup.clone());
            }
        }
        // Batch coins together and ERC20s only within the same exact platform.
        let mut groups: BTreeMap<Option<&str>, Vec<Lookup>> = BTreeMap::new();
        for lookup in missing {
            let platform = match &lookup {
                Lookup::Coin(_) => None,
                Lookup::Token(platform, _) => Some(*platform),
            };
            groups.entry(platform).or_default().push(lookup);
        }
        for lookups in groups.values() {
            for batch in lookups.chunks(16) {
                let path = batch_path(batch);
                let bytes = self.bounded_request(&path).await?;
                let fetched_at = self.now();
                // Validate the entire requested batch before accepting any price.
                let mut parsed = Vec::new();
                for lookup in batch {
                    let quote = parse_quote(&bytes, lookup, fetched_at)?;
                    let updated_at = DateTime::parse_from_rfc3339(&quote.updated_at)
                        .map_err(|_| invalid_price())?
                        .with_timezone(&Utc);
                    parsed.push((
                        lookup.clone(),
                        CachedQuote {
                            quote,
                            fetched_at,
                            updated_at,
                        },
                    ));
                }
                for (lookup, entry) in parsed {
                    if cache.len() >= MAX_CACHE_KEYS
                        && let Some(oldest) = cache
                            .iter()
                            .min_by_key(|(_, value)| value.fetched_at)
                            .map(|(key, _)| key.clone())
                    {
                        cache.remove(&oldest);
                    }
                    cache.insert(lookup, entry);
                }
            }
        }
        let checked_at = self.now();
        selected
            .into_iter()
            .map(|(lookup, identity)| {
                let cached = cache.get(&lookup).ok_or_else(invalid_price)?;
                provider_time(cached.updated_at.timestamp(), checked_at)?;
                let mut quote = cached.quote.clone();
                if let Some(identity) = identity {
                    quote.source.push_str(&identity.source_binding());
                }
                Ok(quote)
            })
            .collect()
    }
}

fn batch_path(lookups: &[Lookup]) -> String {
    if lookups.len() == 1 {
        return lookups[0].path();
    }
    let (path, field) = match &lookups[0] {
        Lookup::Coin(_) => ("/simple/price".into(), "ids"),
        Lookup::Token(platform, _) => (
            format!("/simple/token_price/{platform}"),
            "contract_addresses",
        ),
    };
    let ids = lookups
        .iter()
        .map(Lookup::response_key)
        .collect::<Vec<_>>()
        .join(",");
    let mut url = reqwest::Url::parse("https://pricing.invalid").expect("fixed URL");
    url.set_path(&path);
    url.query_pairs_mut()
        .append_pair(field, &ids)
        .append_pair("vs_currencies", "usd")
        .append_pair("include_last_updated_at", "true")
        .append_pair("precision", "full");
    format!("{}?{}", url.path(), url.query().expect("fixed query"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::Quantity;
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicI64, Ordering},
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::{JoinHandle, JoinSet},
    };

    const NOW: i64 = 1_800_000_000;
    const CONTRACT: &str = "0x1111111111111111111111111111111111111111";

    #[tokio::test]
    async fn spot_http_path_batches_only_stablecoin_ids_and_never_sends_keys_to_binance() {
        let updated = Utc::now().timestamp();
        let rates = format!(
            "{{\"usd-coin\":{{\"usd\":0.99,\"last_updated_at\":{updated}}},\"tether\":{{\"usd\":1.02,\"last_updated_at\":{updated}}}}}"
        );
        let fx = Server::new(vec![(http(200, &rates), Duration::ZERO)]).await;
        let metadata = r#"{"symbols":[{"symbol":"SEIUSDT","baseAsset":"SEI","quoteAsset":"USDT","status":"TRADING","isSpotTradingAllowed":true},{"symbol":"APTUSDC","baseAsset":"APT","quoteAsset":"USDC","status":"TRADING","isSpotTradingAllowed":true},{"symbol":"BNBUSDT","baseAsset":"BNB","quoteAsset":"USDT","status":"TRADING","isSpotTradingAllowed":true}]}"#;
        let books = r#"[{"symbol":"SEIUSDT","bidPrice":"0.49","askPrice":"0.5","bidQty":"100","askQty":"100"},{"symbol":"APTUSDC","bidPrice":"9","askPrice":"10","bidQty":"100","askQty":"100"},{"symbol":"BNBUSDT","bidPrice":"19","askPrice":"20","bidQty":"100","askQty":"100"}]"#;
        let venue = Server::new(vec![
            (http(200, metadata), Duration::ZERO),
            (http(200, books), Duration::ZERO),
        ])
        .await;
        let mut cg = CoinGeckoPrices::with_api_key(ApiPlan::Demo, "SYNTHETIC-DEMO-KEY").unwrap();
        cg.test_base_url = Some(fx.base.clone());
        cg.test_pacing = Some(Duration::ZERO);
        cg.exchanges.clear(); // Accidental legacy resolution must fail, not reach production.
        let cg = Arc::new(cg);
        let spot = exchange_spot::BinanceSpot::new().unwrap().test_urls(
            format!("{}/exchangeInfo", venue.base),
            format!("{}/bookTicker", venue.base),
        );
        let provider = ExchangeSpotPrices::with_sources(cg.clone(), cg, Arc::new(spot));
        let assets = [
            asset("binance", "APT", 36),
            asset("binance", "BNB", 36),
            asset("binance", "SEI", 36),
            asset("binance", "USDC", 36),
            asset("binance", "USDT", 36),
        ];
        let quotes = provider.quotes(&assets).await.unwrap();
        assert_eq!(
            quotes
                .iter()
                .map(|q| q.usd_per_unit.as_str())
                .collect::<Vec<_>>(),
            ["9.9", "20.4", "0.51", "0.99", "1.02"]
        );
        provider.quotes(&assets).await.unwrap();
        assert_eq!(fx.count(), 1);
        assert_eq!(venue.count(), 2);
        let requests = fx.requests.lock().unwrap();
        let request = &requests[0];
        assert!(request.starts_with("GET /simple/price?"));
        assert!(request.contains("ids=usd-coin%2Ctether"));
        assert!(request.contains("x-cg-demo-api-key: SYNTHETIC-DEMO-KEY"));
        assert!(!request.contains("/exchanges/"));
        assert!(!request.contains("aptos"));
        assert!(!request.contains("binancecoin"));
        let requests = venue.requests.lock().unwrap();
        assert!(requests[1].contains("symbols="));
        assert!(requests[1].contains("symbolStatus=TRADING"));
        for request in requests.iter() {
            assert!(!request.to_ascii_lowercase().contains("api-key"));
            assert!(!request.contains("SYNTHETIC-DEMO-KEY"));
        }
    }

    #[tokio::test]
    async fn stablecoin_http_batch_rejects_missing_member_and_preserves_raw_decimals() {
        let timestamp = Utc::now().timestamp();
        let missing = format!("{{\"usd-coin\":{{\"usd\":0.99,\"last_updated_at\":{timestamp}}}}}");
        let valid = format!(
            "{{\"usd-coin\":{{\"usd\":0.99000000000000000001,\"last_updated_at\":{timestamp}}},\"tether\":{{\"usd\":1.02000000000000000001,\"last_updated_at\":{timestamp}}}}}"
        );
        let server = Server::new(vec![
            (http(200, &missing), Duration::ZERO),
            (http(200, &valid), Duration::ZERO),
        ])
        .await;
        let mut provider = CoinGeckoPrices::new().unwrap();
        provider.test_base_url = Some(server.base.clone());
        provider.test_pacing = Some(Duration::ZERO);
        provider.exchanges.clear();
        let error = provider.stablecoin_rates().await.unwrap_err();
        assert!(error.message.contains("USDT/USD"));
        let rates = provider.stablecoin_rates().await.unwrap();
        assert_eq!(rates.usdc.usd_per_unit, "0.99000000000000000001");
        assert_eq!(rates.usdt.usd_per_unit, "1.02000000000000000001");
        assert_eq!(server.count(), 2);
        for request in server.requests.lock().unwrap().iter() {
            assert!(request.contains("ids=usd-coin%2Ctether"));
        }
    }

    #[tokio::test]
    async fn spot_http_rejects_numeric_prices_and_honors_venue_cooldown() {
        let malformed =
            r#"[{"symbol":"APTUSDC","bidPrice":9,"askPrice":"10","bidQty":"100","askQty":"100"}]"#;
        let server = Server::new(vec![
            (http(200, malformed), Duration::ZERO),
            (b"HTTP/1.1 429 Fixture\r\nContent-Length: 0\r\nRetry-After: 60\r\nConnection: close\r\n\r\n".to_vec(), Duration::ZERO),
        ]).await;
        let source = exchange_spot::BinanceSpot::new().unwrap().test_urls(
            format!("{}/exchangeInfo", server.base),
            format!("{}/bookTicker", server.base),
        );
        let markets = vec!["APTUSDC".to_owned()];
        assert_eq!(
            source.books("binance", &markets).await.unwrap_err().code,
            "PRICE_SPOT_DATA_INVALID"
        );
        assert_eq!(
            source.books("binance", &markets).await.unwrap_err().code,
            "PRICE_VENUE_RATE_LIMITED"
        );
        assert_eq!(
            source.books("binance", &markets).await.unwrap_err().code,
            "PRICE_VENUE_RATE_LIMITED"
        );
        assert_eq!(server.count(), 2, "cooldown must not issue another request");
    }

    #[tokio::test]
    async fn conversion_parser_accepts_older_rate_but_onchain_and_expired_rates_stay_strict() {
        let timestamp = Utc::now().timestamp() - 150;
        let response = format!(
            "{{\"usd-coin\":{{\"usd\":0.99,\"last_updated_at\":{timestamp}}},\"tether\":{{\"usd\":1.02,\"last_updated_at\":{timestamp}}}}}"
        );
        let server = Server::new(vec![(http(200, &response), Duration::ZERO)]).await;
        let mut provider = CoinGeckoPrices::new().unwrap();
        provider.test_base_url = Some(server.base.clone());
        provider.test_pacing = Some(Duration::ZERO);
        let rates = provider.stablecoin_rates().await.unwrap();
        assert_eq!(
            rates.usdc.updated_at,
            DateTime::from_timestamp(timestamp, 0).unwrap().to_rfc3339()
        );
        assert_eq!(rates.usdc.usd_per_unit, "0.99");
        assert!(
            parse_quote(
                &body("ethereum", "1", NOW - 150).into_bytes(),
                &Lookup::Coin("ethereum".into()),
                at(NOW)
            )
            .is_err()
        );
        assert!(
            parse_quote_with_age(
                &body("usd-coin", "0.99", NOW - 300).into_bytes(),
                &Lookup::Coin("usd-coin".into()),
                at(NOW),
                300
            )
            .is_err()
        );
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }
    fn asset(network: &str, reference: &str, decimals: u8) -> AssetId {
        AssetId {
            network: network.into(),
            reference: reference.into(),
            decimals,
        }
    }
    fn native() -> AssetId {
        asset("ethereum:1", "native", 18)
    }
    fn body(key: &str, price: &str, updated: i64) -> String {
        format!("{{\"{key}\":{{\"usd\":{price},\"last_updated_at\":{updated}}}}}")
    }
    fn http(status: u16, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    struct Server {
        base: String,
        requests: Arc<StdMutex<Vec<String>>>,
        task: JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Server {
        async fn new(responses: Vec<(Vec<u8>, Duration)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let queue = Arc::new(StdMutex::new(VecDeque::from(responses)));
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let recorded = requests.clone();
            let task = tokio::spawn(async move {
                let mut clients = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((mut socket, _)) = accepted else { break };
                            let queue = queue.clone(); let recorded = recorded.clone();
                            clients.spawn(async move {
                                let mut request = Vec::new(); let mut chunk = [0; 1024];
                                while request.len() < 8192 && !request.windows(4).any(|part| part == b"\r\n\r\n") {
                                    match socket.read(&mut chunk).await {
                                        Ok(0) | Err(_) => return,
                                        Ok(count) => request.extend_from_slice(&chunk[..count]),
                                    }
                                }
                                recorded.lock().unwrap().push(String::from_utf8(request).unwrap());
                                let (response, delay) = queue.lock().unwrap().pop_front().unwrap_or_else(|| (http(404, ""), Duration::ZERO));
                                tokio::time::sleep(delay).await;
                                let _ = socket.write_all(&response).await;
                            });
                        }
                        _ = clients.join_next(), if !clients.is_empty() => {}
                    }
                }
            });
            Self {
                base,
                requests,
                task,
            }
        }
        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
        async fn await_count(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while self.count() < count {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    async fn fixture(
        responses: Vec<(Vec<u8>, Duration)>,
    ) -> (Server, CoinGeckoPrices, Arc<AtomicI64>) {
        let server = Server::new(responses).await;
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut provider = CoinGeckoPrices::new().unwrap();
        provider.test_pacing = Some(Duration::ZERO);
        provider.test_base_url = Some(server.base.clone());
        provider.test_clock = Some(clock.clone());
        (server, provider, clock)
    }
    async fn single(response: Vec<u8>, selected: &AssetId) -> Result<UsdQuote> {
        let (_server, provider, _) = fixture(vec![(response, Duration::ZERO)]).await;
        provider.quote(selected).await
    }

    #[test]
    fn exact_lexemes_preserve_large_numbers_decimals_and_exponents() {
        for (raw, expected) in [
            ("9007199254740993", "9007199254740993"),
            (
                "9007199254740993.123456789012345678901234567890123456",
                "9007199254740993.123456789012345678901234567890123456",
            ),
            ("1.2300", "1.23"),
            ("1.23e3", "1230"),
            ("1.23E-3", "0.00123"),
            ("1e+3", "1000"),
            ("100e-2", "1"),
            ("0.0100e2", "1"),
            ("100000e-41", "0.000000000000000000000000000000000001"),
            ("1.0000000000000000000000000000000000000", "1"),
        ] {
            assert_eq!(exact_decimal(raw).unwrap(), expected, "{raw}");
        }
    }

    #[test]
    fn rejects_nonpositive_invalid_or_overflowing_numbers_without_rounding() {
        for raw in [
            "", "0", "0e3", "-0", "-1", "+1", "NaN", "Infinity", "null", "\"1\"", "1.", ".1", "01",
            "1e", "1e+", "1e1e1", "1e999", "1e99999", "1e-37", "1e100", "1e 3", "1 2", "1e--1",
        ] {
            assert!(exact_decimal(raw).is_err(), "{raw}");
        }
        assert!(exact_decimal(&"1".repeat(257)).is_err());
        assert_eq!(exact_decimal("1e99").unwrap().len(), MAX_INTEGER_DIGITS);
    }

    #[test]
    fn raw_value_decoding_does_not_round_numbers_above_two_to_the_53() {
        let quote = parse_quote(
            body("ethereum", "9007199254740993.123456789012345678", NOW).as_bytes(),
            &Lookup::Coin("ethereum".into()),
            at(NOW),
        )
        .unwrap();
        assert_eq!(quote.usd_per_unit, "9007199254740993.123456789012345678");
        let price = Quantity::from_decimal(&quote.usd_per_unit, 36).unwrap();
        let atoms = Quantity::new("9007199254740993000001").unwrap();
        let denominator = Quantity::new(format!("1{}", "0".repeat(48))).unwrap();
        let valued = atoms.mul_div_ceil(&price, &denominator).unwrap();
        assert_eq!(valued.as_str(), "81129638414606700822196420");
    }

    #[test]
    fn exact_36_place_price_is_not_rounded_to_zero() {
        let quote = parse_quote(
            body("ethereum", "1e-36", NOW).as_bytes(),
            &Lookup::Coin("ethereum".into()),
            at(NOW),
        )
        .unwrap();
        assert_eq!(
            Quantity::from_decimal(&quote.usd_per_unit, 36).unwrap(),
            Quantity::from_u64(1)
        );
    }

    #[test]
    fn duplicates_wrong_types_missing_fields_and_wrong_identity_fail_closed() {
        for raw in [
            "null".to_string(),
            "[]".to_string(),
            "{".to_string(),
            "{}".to_string(),
            body("wrong-contract", "1", NOW),
            "{\"ethereum\":{\"usd\":1}}".to_string(),
            format!("{{\"ethereum\":{{\"usd\":\"1\",\"last_updated_at\":{NOW}}}}}"),
            format!("{{\"ethereum\":{{\"usd\":1,\"last_updated_at\":\"{NOW}\"}}}}"),
            format!("{{\"ethereum\":{{\"usd\":1,\"last_updated_at\":{NOW}.0}}}}"),
            format!("{{\"ethereum\":{{\"usd\":1,\"usd\":2,\"last_updated_at\":{NOW}}}}}"),
            format!(
                "{{\"ethereum\":{{\"usd\":1,\"last_updated_at\":{NOW}}},\"ethereum\":{{\"usd\":2,\"last_updated_at\":{NOW}}}}}"
            ),
            body("ethereum", "1e999999999", NOW),
            body("ethereum", "NaN", NOW),
        ] {
            assert!(
                parse_quote(raw.as_bytes(), &Lookup::Coin("ethereum".into()), at(NOW)).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn timestamps_are_required_positive_current_and_strictly_under_120_seconds_old() {
        for timestamp in [0, -1, NOW + 1, NOW - 120, i64::MAX] {
            assert!(
                parse_quote(
                    body("ethereum", "1", timestamp).as_bytes(),
                    &Lookup::Coin("ethereum".into()),
                    at(NOW)
                )
                .is_err()
            );
        }
        let quote = parse_quote(
            body("ethereum", "1", NOW - 119).as_bytes(),
            &Lookup::Coin("ethereum".into()),
            at(NOW),
        )
        .unwrap();
        assert_eq!(quote.updated_at, at(NOW - 119).to_rfc3339());
        assert_eq!(quote.observed_at, at(NOW).to_rfc3339());
        assert!(quote.source.contains("coin_id=ethereum"));
    }

    #[test]
    fn native_assets_select_exact_id_lookups() {
        for network in [
            "ethereum:1",
            "ethereum:42161",
            "ethereum:4663",
            "ethereum:8453",
        ] {
            assert_eq!(
                Lookup::asset(&asset(network, "native", 18)).unwrap(),
                Lookup::Coin("ethereum".into())
            );
        }
    }

    #[tokio::test]
    async fn unsupported_coin_network_and_unsafe_contract_refuse_before_http() {
        let (server, provider, _) = fixture(vec![]).await;
        for selected in [
            asset("exchange:unregistered", "UNKNOWN", 6),
            asset("ethereum:99", CONTRACT, 6),
            asset("ethereum:1", "USDC", 6),
            asset("ethereum:1", "native", 6),
            asset(
                "ethereum:1",
                "0x0000000000000000000000000000000000000000",
                18,
            ),
            asset(
                "ethereum:1",
                "0x1111111111111111111111111111111111111111?api_key=secret",
                6,
            ),
        ] {
            assert_eq!(
                provider.quote(&selected).await.unwrap_err().code,
                "PRICE_ASSET_UNSUPPORTED"
            );
        }
        assert_eq!(server.count(), 0);
    }

    #[tokio::test]
    async fn actual_http_requests_use_exact_ids_and_full_precision_source_timestamps() {
        let (server, provider, _) = fixture(vec![(
            http(200, &body("ethereum", "0.973421", NOW - 10)),
            Duration::ZERO,
        )])
        .await;
        let quote = provider.quote(&native()).await.unwrap();
        assert_eq!(quote.usd_per_unit, "0.973421");
        assert_eq!(quote.updated_at, at(NOW - 10).to_rfc3339());
        assert_eq!(quote.observed_at, at(NOW).to_rfc3339());
        let requests = server.requests.lock().unwrap();
        assert!(requests[0].starts_with("GET /simple/price?ids=ethereum&vs_currencies=usd&include_last_updated_at=true&precision=full HTTP/1.1"));
        assert!(!requests[0].contains("api-key"));
        assert!(!requests[0].contains("symbols="));
    }

    #[tokio::test]
    async fn contracts_are_normalized_and_network_platforms_cannot_borrow_other_quotes() {
        let mixed = "0xAbCd111111111111111111111111111111111111";
        let contract = mixed.to_ascii_lowercase();
        let (server, provider, _) = fixture(vec![
            (http(200, &body(&contract, "1.25", NOW)), Duration::ZERO),
            (http(200, &body(&contract, "2.5", NOW)), Duration::ZERO),
            (http(200, &body(&contract, "3.75", NOW)), Duration::ZERO),
            (http(200, &body(&contract, "4.5", NOW)), Duration::ZERO),
        ])
        .await;
        for (network, platform, value) in [
            ("ethereum:1", "ethereum", "1.25"),
            ("ethereum:42161", "arbitrum-one", "2.5"),
            ("ethereum:4663", "robinhood", "3.75"),
            ("ethereum:8453", "base", "4.5"),
        ] {
            let quote = provider.quote(&asset(network, mixed, 6)).await.unwrap();
            assert_eq!(quote.usd_per_unit, value);
            assert!(quote.source.contains(platform));
            assert!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .starts_with(&format!(
                        "GET /simple/token_price/{platform}?contract_addresses={contract}&"
                    ))
            );
        }
        assert_eq!(server.count(), 4);
        assert!(
            single(
                http(200, &body(CONTRACT, "1", NOW)),
                &asset("ethereum:1", mixed, 6)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn http_zero_negative_malformed_future_and_stale_quotes_fail_closed() {
        for (price, updated) in [
            ("0", NOW),
            ("-1", NOW),
            ("1e-37", NOW),
            ("1", NOW + 1),
            ("1", NOW - 120),
        ] {
            assert!(
                single(http(200, &body("ethereum", price, updated)), &native())
                    .await
                    .is_err()
            );
        }
        assert!(single(http(200, "{}"), &native()).await.is_err());
        assert!(
            single(http(200, "raw-private-response"), &native())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rates_redirects_truncated_and_failed_responses_are_safe_errors() {
        for response in [
            http(429, "raw-secret-api-key"), http(403, "raw-secret-api-key"), http(500, "raw-secret-api-key"),
            b"HTTP/1.1 302 Found\r\nLocation: https://private.invalid/?api_key=secret\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\n{}".to_vec(),
        ] {
            let error = single(response, &native()).await.unwrap_err();
            let rendered = serde_json::to_string(&error).unwrap();
            assert!(!rendered.contains("secret")); assert!(!rendered.contains("private.invalid"));
        }
        assert_eq!(
            single(http(429, ""), &native()).await.unwrap_err().code,
            "PRICE_RATE_LIMITED"
        );
    }

    #[tokio::test]
    async fn declared_and_chunked_bodies_are_bounded_before_json_decoding() {
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            MAX_RESPONSE_BYTES + 1,
            " ".repeat(MAX_RESPONSE_BYTES + 1)
        )
        .into_bytes();
        for response in [declared, chunked] {
            assert_eq!(
                single(response, &native()).await.unwrap_err().code,
                "PRICE_DATA_INVALID"
            );
        }
    }

    #[tokio::test]
    async fn cache_keeps_both_timestamps_and_refreshes_once_at_30_seconds() {
        let (server, provider, clock) = fixture(vec![
            (
                http(200, &body("ethereum", "1.25", NOW - 10)),
                Duration::ZERO,
            ),
            (
                http(200, &body("ethereum", "1.5", NOW + 30)),
                Duration::ZERO,
            ),
        ])
        .await;
        let first = provider.quote(&native()).await.unwrap();
        clock.store(NOW + 29, Ordering::SeqCst);
        for network in ["ethereum:42161", "ethereum:4663", "ethereum:8453"] {
            let cached = provider.quote(&asset(network, "native", 18)).await.unwrap();
            assert_eq!(cached.updated_at, first.updated_at);
            assert_eq!(cached.observed_at, first.observed_at);
            assert_eq!(cached.usd_per_unit, first.usd_per_unit);
        }
        assert_eq!(
            provider
                .quote(&asset("ethereum:4663", "native", 18))
                .await
                .unwrap()
                .observed_at,
            first.observed_at
        );
        assert_eq!(server.count(), 1);
        clock.store(NOW + 30, Ordering::SeqCst);
        let fresh = provider.quote(&native()).await.unwrap();
        assert_eq!(fresh.usd_per_unit, "1.5");
        assert_eq!(fresh.updated_at, at(NOW + 30).to_rfc3339());
        assert_eq!(fresh.observed_at, at(NOW + 30).to_rfc3339());
        provider.quote(&native()).await.unwrap();
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn provider_timestamp_expiry_refetches_even_a_recent_cache_entry() {
        let (server, provider, clock) = fixture(vec![
            (http(200, &body("ethereum", "1", NOW - 119)), Duration::ZERO),
            (http(200, &body("ethereum", "1", NOW - 119)), Duration::ZERO),
        ])
        .await;
        provider.quote(&native()).await.unwrap();
        clock.store(NOW + 1, Ordering::SeqCst);
        assert_eq!(
            provider.quote(&native()).await.unwrap_err().code,
            "PRICE_STALE"
        );
        assert_eq!(server.count(), 2);
        assert!(provider.cache.lock().await.is_empty());
    }

    #[tokio::test]
    async fn disconnect_or_rate_limit_during_refresh_never_serves_the_prior_quote() {
        let (server, provider, clock) = fixture(vec![
            (http(200, &body("ethereum", "1", NOW)), Duration::ZERO),
            (http(429, ""), Duration::ZERO),
            (http(200, &body("ethereum", "2", NOW + 90)), Duration::ZERO),
        ])
        .await;
        provider.quote(&native()).await.unwrap();
        clock.store(NOW + 30, Ordering::SeqCst);
        assert_eq!(
            provider.quote(&native()).await.unwrap_err().code,
            "PRICE_RATE_LIMITED"
        );
        assert!(provider.cache.lock().await.is_empty());
        assert_eq!(
            provider.quote(&native()).await.unwrap_err().code,
            "PRICE_RATE_LIMITED"
        );
        assert_eq!(server.count(), 2);
        clock.store(NOW + 90, Ordering::SeqCst);
        assert_eq!(provider.quote(&native()).await.unwrap().usd_per_unit, "2");
        assert_eq!(server.count(), 3);
        let (_server, mut disconnected, _) = fixture(vec![]).await;
        disconnected.test_base_url = Some("http://127.0.0.1:0".into());
        assert_eq!(
            disconnected.quote(&native()).await.unwrap_err().code,
            "PRICE_UNAVAILABLE"
        );
    }

    #[tokio::test]
    async fn concurrent_requests_coalesce_to_one_verified_response() {
        let (server, provider, _) = fixture(vec![(
            http(200, &body("ethereum", "1.25", NOW)),
            Duration::from_millis(20),
        )])
        .await;
        let provider = Arc::new(provider);
        let mut tasks = JoinSet::new();
        for _ in 0..16 {
            let provider = provider.clone();
            tasks.spawn(async move { provider.quote(&native()).await.unwrap() });
        }
        while let Some(result) = tasks.join_next().await {
            let quote = result.unwrap();
            assert_eq!(quote.usd_per_unit, "1.25");
            assert_eq!(quote.observed_at, at(NOW).to_rfc3339());
        }
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn cancellation_drops_request_and_lock_without_caching_then_allows_retry() {
        let (server, provider, _) = fixture(vec![
            (
                http(200, &body("ethereum", "1", NOW)),
                Duration::from_secs(1),
            ),
            (http(200, &body("ethereum", "2", NOW)), Duration::ZERO),
        ])
        .await;
        let provider = Arc::new(provider);
        let first = provider.clone();
        let task = tokio::spawn(async move { first.quote(&native()).await });
        server.await_count(1).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(provider.cache.lock().await.is_empty());
        assert_eq!(provider.quote(&native()).await.unwrap().usd_per_unit, "2");
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn total_timeout_includes_slow_response_and_retry_can_succeed() {
        let (server, mut provider, _) = fixture(vec![
            (
                http(200, &body("ethereum", "1", NOW)),
                Duration::from_millis(100),
            ),
            (http(200, &body("ethereum", "2", NOW)), Duration::ZERO),
        ])
        .await;
        provider.test_timeout = Some(Duration::from_millis(10));
        assert_eq!(
            provider.quote(&native()).await.unwrap_err().code,
            "PRICE_UNAVAILABLE"
        );
        assert!(provider.cache.lock().await.is_empty());
        assert_eq!(provider.quote(&native()).await.unwrap().usd_per_unit, "2");
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn arbitrary_contract_cache_stores_only_bounded_latest_entries() {
        let responses = (1..=MAX_CACHE_KEYS + 1)
            .map(|number| {
                (
                    http(200, &body(&format!("0x{number:040x}"), "1", NOW)),
                    Duration::ZERO,
                )
            })
            .collect();
        let (server, provider, _) = fixture(responses).await;
        for number in 1..=MAX_CACHE_KEYS + 1 {
            provider
                .quote(&asset("ethereum:1", &format!("0x{number:040x}"), 6))
                .await
                .unwrap();
        }
        assert_eq!(provider.cache.lock().await.len(), MAX_CACHE_KEYS);
        assert_eq!(server.count(), MAX_CACHE_KEYS + 1);
    }

    fn authenticated(plan: ApiPlan, server: &Server) -> CoinGeckoPrices {
        let mut provider =
            CoinGeckoPrices::with_api_key(plan, "SYNTHETIC-COINGECKO-KEY-NEVER-LIVE").unwrap();
        provider.test_pacing = Some(Duration::ZERO);
        provider.test_base_url = Some(server.base.clone());
        provider.test_clock = Some(Arc::new(AtomicI64::new(NOW)));
        provider
    }

    #[test]
    fn api_plan_origins_and_headers_are_fixed_and_keys_have_sensitive_debug() {
        assert_eq!(ApiPlan::Demo.base_url(), "https://api.coingecko.com/api/v3");
        assert_eq!(
            ApiPlan::Pro.base_url(),
            "https://pro-api.coingecko.com/api/v3"
        );
        assert_eq!(serde_json::to_string(&ApiPlan::Demo).unwrap(), "\"demo\"");
        assert_eq!(serde_json::to_string(&ApiPlan::Pro).unwrap(), "\"pro\"");
        assert!(serde_json::from_str::<ApiPlan>("\"unexpected\"").is_err());
        for plan in [ApiPlan::Demo, ApiPlan::Pro] {
            let provider =
                CoinGeckoPrices::with_api_key(plan, "SYNTHETIC-COINGECKO-KEY-NEVER-LIVE").unwrap();
            let (_, value) = provider.authentication.as_ref().unwrap();
            assert!(value.is_sensitive());
            assert!(!format!("{value:?}").contains("SYNTHETIC"));
            let request = provider
                .client
                .get(format!("{}/ping", plan.base_url()))
                .header(plan.header_name(), value.clone())
                .build()
                .unwrap();
            assert!(!format!("{request:?}").contains("SYNTHETIC"));
            assert!(request.headers()[plan.header_name()].is_sensitive());
            assert!(!request.url().as_str().contains("SYNTHETIC"));
        }
        assert!(CoinGeckoPrices::new().unwrap().authentication.is_none());
    }

    #[test]
    fn malformed_keys_are_refused_with_generic_errors_before_transport() {
        for key in [
            "",
            "SYNTHETIC KEY",
            "SYNTHETIC\r\nInjected: value",
            "SYNTHETIC\0KEY",
            "SYNTHETIC-ü",
            "SYNTHETIC:COLON",
        ] {
            for plan in [ApiPlan::Demo, ApiPlan::Pro] {
                let error = CoinGeckoPrices::with_api_key(plan, key).err().unwrap();
                assert_eq!(error.code, "PRICE_AUTHENTICATION_FAILED");
                assert!(!error.to_string().contains("SYNTHETIC"));
            }
        }
        assert!(CoinGeckoPrices::with_api_key(ApiPlan::Demo, &"S".repeat(257)).is_err());
    }

    #[tokio::test]
    async fn local_ping_and_quotes_use_only_the_selected_plan_header_without_query_secrets() {
        for plan in [ApiPlan::Demo, ApiPlan::Pro] {
            let server = Server::new(vec![
                (
                    http(200, "{\"gecko_says\":\"(V3) To the Moon!\"}"),
                    Duration::ZERO,
                ),
                (http(200, &body("ethereum", "1.25", NOW)), Duration::ZERO),
            ])
            .await;
            let provider = authenticated(plan, &server);
            provider.validate_access().await.unwrap();
            let quote = provider.quote(&native()).await.unwrap();
            assert_eq!(quote.usd_per_unit, "1.25");
            assert_eq!(server.count(), 2); // quote did not insert another ping
            let requests = server.requests.lock().unwrap();
            assert!(requests[0].starts_with("GET /ping HTTP/1.1"));
            for request in requests.iter() {
                let request = request.to_ascii_lowercase();
                assert!(request.contains(&format!(
                    "{}: synthetic-coingecko-key-never-live\r\n",
                    plan.header_name()
                )));
                let other = if plan == ApiPlan::Demo {
                    ApiPlan::Pro
                } else {
                    ApiPlan::Demo
                };
                assert!(!request.contains(other.header_name()));
                assert!(!request.lines().next().unwrap().contains("synthetic"));
            }
        }
    }

    #[tokio::test]
    async fn ping_authentication_rate_and_malformed_failures_never_fall_back_anonymously() {
        for (status, body, code) in [
            (
                401,
                "SYNTHETIC-PRIVATE-RESPONSE",
                "PRICE_AUTHENTICATION_FAILED",
            ),
            (
                403,
                "SYNTHETIC-PRIVATE-RESPONSE",
                "PRICE_AUTHENTICATION_FAILED",
            ),
            (429, "SYNTHETIC-PRIVATE-RESPONSE", "PRICE_RATE_LIMITED"),
            (200, "{}", "PRICE_DATA_INVALID"),
            (
                200,
                "{\"gecko_says\":\"SYNTHETIC-PRIVATE-RESPONSE\"}",
                "PRICE_DATA_INVALID",
            ),
            (
                200,
                "{\"gecko_says\":\"(V3) To the Moon!\",\"gecko_says\":\"duplicate\"}",
                "PRICE_DATA_INVALID",
            ),
        ] {
            let server = Server::new(vec![(http(status, body), Duration::ZERO)]).await;
            let provider = authenticated(ApiPlan::Demo, &server);
            let error = provider.validate_access().await.unwrap_err();
            assert_eq!(error.code, code);
            assert!(!error.to_string().contains("SYNTHETIC"));
            assert_eq!(server.count(), 1);
            assert!(provider.authentication.is_some());
        }
    }

    #[tokio::test]
    async fn configured_quote_authentication_failure_is_not_retried_without_the_key() {
        for plan in [ApiPlan::Demo, ApiPlan::Pro] {
            let server = Server::new(vec![(
                http(401, "SYNTHETIC-PRIVATE-RESPONSE"),
                Duration::ZERO,
            )])
            .await;
            let provider = authenticated(plan, &server);
            let error = provider.quote(&native()).await.unwrap_err();
            assert_eq!(error.code, "PRICE_AUTHENTICATION_FAILED");
            assert!(!error.to_string().contains("SYNTHETIC"));
            assert_eq!(server.count(), 1);
            assert!(provider.cache.lock().await.is_empty());
        }
    }

    #[tokio::test]
    async fn authenticated_ping_obeys_body_bounds_redirect_refusal_and_total_timeout() {
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            MAX_RESPONSE_BYTES + 1,
            " ".repeat(MAX_RESPONSE_BYTES + 1)
        )
        .into_bytes();
        for (response, delay, code) in [
            (chunked, Duration::ZERO, "PRICE_DATA_INVALID"),
            (b"HTTP/1.1 302 Found\r\nLocation: https://unreviewed.invalid\r\nContent-Length: 0\r\n\r\n".to_vec(), Duration::ZERO, "PRICE_UNAVAILABLE"),
            (http(200, "{\"gecko_says\":\"(V3) To the Moon!\"}"), Duration::from_millis(100), "PRICE_UNAVAILABLE"),
        ] {
            let server = Server::new(vec![(response, delay)]).await;
            let mut provider = authenticated(ApiPlan::Pro, &server);
            provider.test_timeout = Some(Duration::from_millis(10));
            assert_eq!(provider.validate_access().await.unwrap_err().code, code);
            assert_eq!(server.count(), 1);
        }
    }
    mod exchange_resolution {
        use super::*;
        include!("resolution_tests.rs");
    }
}
