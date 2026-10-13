//! Exchange-local identity evidence, cached independently of USD quotes.
//! Provider tickers supply IDs; venue pairs cross-check the exact asset labels.
use super::*;
use std::collections::{BTreeSet, HashSet};

const IDENTITY_SECONDS: i64 = 86_400;
const SCAN_SECONDS: i64 = 900;
const MISS_REFRESH_SECONDS: i64 = 30;
const MAX_TICKER_PAGES: usize = 128;
const PAGE_SIZE: usize = 100;
const MAX_VENUE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MARKETS: usize = 20_000;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExchangeAssetKey {
    pub exchange: String,
    /// Exact exchange-local identifier; never normalized to a global symbol.
    pub asset: String,
}

#[derive(Clone, Debug)]
pub struct ExchangeMarket {
    pub market_id: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub active: bool,
}

#[async_trait]
pub trait ExchangeMarketSource: Send + Sync {
    async fn markets(&self, exchange: &str) -> Result<Vec<ExchangeMarket>>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolvedExchangeAsset {
    pub provider: String,
    pub exchange: String,
    pub asset: String,
    pub provider_asset_id: String,
    pub market_id: String,
    pub observed_at: String,
}

impl ResolvedExchangeAsset {
    pub(super) fn source_binding(&self) -> String {
        format!(
            "; exchange={}; exact_asset={}; venue_market={}; identity_observed_at={}",
            self.exchange, self.asset, self.market_id, self.observed_at
        )
    }
}

#[async_trait]
pub trait ExchangeAssetResolver: Send + Sync {
    async fn resolve_exchange_asset(&self, key: &ExchangeAssetKey)
    -> Result<ResolvedExchangeAsset>;
}

pub(super) struct RegisteredExchange {
    pub provider_exchange: String,
    pub source: Arc<dyn ExchangeMarketSource>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedIdentities {
    fetched_at: DateTime<Utc>,
    mappings: BTreeMap<String, Result<ResolvedExchangeAsset>>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct IdentityCache {
    exchanges: BTreeMap<String, CachedIdentities>,
    pending: BTreeMap<String, PendingScan>,
}

/// Public identity metadata only. The protected caller owns storage permissions.
/// No API key, price, financial record or approval is stored here.
pub trait IdentityStore: Send + Sync {
    fn load(&self) -> Result<Option<Vec<u8>>>;
    fn save(&self, bytes: &[u8]) -> Result<()>;
}
#[derive(Serialize, Deserialize)]
struct StoredIdentities {
    version: u8,
    origin: String,
    #[serde(default)]
    auth_mode: String,
    exchanges: BTreeMap<String, String>,
    cache: IdentityCache,
    #[serde(default)]
    cooldown_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pacing_seconds: u64,
}

type TickerIdentity = (String, String, Option<String>, Option<String>);

#[derive(Clone, Serialize, Deserialize)]
struct PendingScan {
    started_at: DateTime<Utc>,
    next_page: usize,
    #[serde(with = "market_pairs")]
    pairs: BTreeMap<(String, String), String>,
    candidates: BTreeMap<String, Candidate>,
    seen_pages: HashSet<Vec<TickerIdentity>>,
}

mod market_pairs {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        pairs: &BTreeMap<(String, String), String>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        pairs.iter().collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<BTreeMap<(String, String), String>, D::Error> {
        let rows = Vec::<((String, String), String)>::deserialize(deserializer)?;
        let mut pairs = BTreeMap::new();
        for (pair, market) in rows {
            if pairs.insert(pair, market).is_some() {
                return Err(serde::de::Error::custom("duplicate venue pair"));
            }
        }
        Ok(pairs)
    }
}

pub(super) fn exchange_name(network: &str) -> Option<&str> {
    if matches!(network, "binance" | "kraken") {
        Some(network)
    } else {
        network.strip_prefix("exchange:")
    }
}

pub(super) fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(failure(
            "PRICE_IDENTITY_INVALID",
            "An exact bounded public identity identifier is required.",
        ));
    }
    Ok(())
}

fn unavailable() -> FinError {
    failure(
        "PRICE_IDENTITY_UNAVAILABLE",
        "No complete provider ticker and venue market evidence resolves the requested exact exchange asset; refresh is automatic on a later read.",
    )
}

fn invalid_metadata() -> FinError {
    failure(
        "PRICE_IDENTITY_DATA_INVALID",
        "Identity metadata is malformed, incomplete, repeated or exceeds bounded pagination; no partial mapping was accepted.",
    )
}

pub(super) fn retry_after_seconds(header: Option<&str>, now: DateTime<Utc>) -> i64 {
    header
        .and_then(|value| {
            value.parse::<i64>().ok().or_else(|| {
                DateTime::parse_from_rfc2822(value)
                    .ok()
                    .map(|time| (time.with_timezone(&Utc) - now).num_seconds())
            })
        })
        .unwrap_or(60)
        .clamp(1, 86_400)
}

#[derive(Deserialize)]
struct TickerPage {
    tickers: Vec<Ticker>,
}
#[derive(Deserialize)]
struct Ticker {
    base: String,
    target: String,
    market: TickerMarket,
    coin_id: Option<String>,
    target_coin_id: Option<String>,
}
#[derive(Deserialize)]
struct TickerMarket {
    identifier: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Candidate {
    ids: BTreeMap<String, String>,
    missing: bool,
}

fn add_candidate(
    candidates: &mut BTreeMap<String, Candidate>,
    asset: &str,
    id: Option<&str>,
    market: &str,
) -> Result<()> {
    let candidate = candidates.entry(asset.into()).or_default();
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        validate_identifier(id)?;
        candidate
            .ids
            .entry(id.into())
            .or_insert_with(|| market.into());
    } else {
        candidate.missing = true;
    }
    Ok(())
}

impl CoinGeckoPrices {
    /// Attach caller-controlled durable metadata storage; invalid/old metadata
    /// is discarded and resolved afresh, never used as a price fallback.
    pub fn with_identity_store(mut self, store: Arc<dyn IdentityStore>) -> Result<Self> {
        if let Some(bytes) = store.load()?
            && bytes.len() <= 16 * 1024 * 1024
            && let Ok(saved) = serde_json::from_slice::<StoredIdentities>(&bytes)
            && saved.version == 1
            && saved.origin == self.provider_origin()
            && saved.exchanges == self.exchange_ids()
            && valid_cache(&saved.cache)
        {
            self.identities = Mutex::new(saved.cache);
            if saved.auth_mode == self.auth_mode() {
                self.rate_limited_until = Mutex::new(saved.cooldown_until);
                let minimum = if self.authentication.is_some() { 1 } else { 6 };
                self.pacing_seconds.store(
                    saved.pacing_seconds.clamp(minimum, 60),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
        self.identity_store = Some(store);
        Ok(self)
    }
    fn auth_mode(&self) -> String {
        match self.authentication.as_ref().map(|(plan, _)| plan) {
            None => "anonymous",
            Some(ApiPlan::Demo) => "demo",
            Some(ApiPlan::Pro) => "pro",
        }
        .into()
    }
    fn provider_origin(&self) -> String {
        self.authentication
            .as_ref()
            .map(|(plan, _)| plan.base_url())
            .unwrap_or(BASE_URL)
            .into()
    }
    fn exchange_ids(&self) -> BTreeMap<String, String> {
        self.exchanges
            .iter()
            .map(|(name, venue)| (name.clone(), venue.provider_exchange.clone()))
            .collect()
    }
    pub(super) fn save_identities(&self, cache: &IdentityCache) -> Result<()> {
        if let Some(store) = &self.identity_store {
            let bytes = serde_json::to_vec(&StoredIdentities {
                version: 1,
                origin: self.provider_origin(),
                auth_mode: self.auth_mode(),
                exchanges: self.exchange_ids(),
                cache: cache.clone(),
                cooldown_until: self
                    .rate_limited_until
                    .try_lock()
                    .ok()
                    .and_then(|until| *until),
                pacing_seconds: self
                    .pacing_seconds
                    .load(std::sync::atomic::Ordering::Relaxed),
            })
            .map_err(|_| invalid_metadata())?;
            if bytes.len() > 16 * 1024 * 1024 {
                return Err(invalid_metadata());
            }
            store.save(&bytes)?;
        }
        Ok(())
    }

    async fn identity_snapshot(
        &self,
        key: &ExchangeAssetKey,
        registered: &RegisteredExchange,
        pending: &mut Option<PendingScan>,
    ) -> Result<CachedIdentities> {
        if pending.is_none() {
            let started_at = self.now();
            let markets = registered.source.markets(&key.exchange).await?;
            if markets.is_empty() || markets.len() > MAX_MARKETS {
                return Err(invalid_metadata());
            }
            let mut pairs = BTreeMap::new();
            let mut market_ids = BTreeSet::new();
            for market in markets {
                for value in [&market.market_id, &market.base_asset, &market.quote_asset] {
                    validate_identifier(value)?;
                }
                if !market_ids.insert(market.market_id.clone()) {
                    return Err(invalid_metadata());
                }
                if market.active
                    && (market.base_asset == market.quote_asset
                        || pairs
                            .insert((market.base_asset, market.quote_asset), market.market_id)
                            .is_some())
                {
                    return Err(invalid_metadata());
                }
            }
            *pending = Some(PendingScan {
                started_at,
                next_page: 1,
                pairs,
                candidates: BTreeMap::new(),
                seen_pages: HashSet::new(),
            });
        }
        let scan = pending.as_mut().expect("initialized scan");
        for page in scan.next_page..=MAX_TICKER_PAGES {
            let mut url = reqwest::Url::parse("https://pricing.invalid").expect("fixed URL");
            // Encode provider exchange IDs as a path segment, never as a URL.
            url.path_segments_mut().expect("fixed base").extend([
                "exchanges",
                &registered.provider_exchange,
                "tickers",
            ]);
            url.query_pairs_mut()
                .append_pair("order", "base_target")
                .append_pair("page", &page.to_string());
            let path = format!("{}?{}", url.path(), url.query().expect("fixed query"));
            let bytes = self.bounded_request(&path).await?;
            let data: TickerPage = serde_json::from_slice(&bytes).map_err(|_| {
                failure(
                    "PRICE_IDENTITY_DATA_INVALID",
                    "Provider ticker page lacks valid identity fields.",
                )
            })?;
            if data.tickers.len() > PAGE_SIZE {
                return Err(invalid_metadata());
            }
            let complete = data.tickers.len() < PAGE_SIZE;
            let page_identity = data
                .tickers
                .iter()
                .map(|ticker| {
                    (
                        ticker.base.clone(),
                        ticker.target.clone(),
                        ticker.coin_id.clone(),
                        ticker.target_coin_id.clone(),
                    )
                })
                .collect::<Vec<_>>();
            if !page_identity.is_empty() && !scan.seen_pages.insert(page_identity) {
                return Err(failure(
                    "PRICE_IDENTITY_DATA_INVALID",
                    "Provider repeated an entire ticker page; pagination did not establish a complete snapshot.",
                ));
            }
            for ticker in data.tickers {
                if ticker.market.identifier != registered.provider_exchange {
                    return Err(failure(
                        "PRICE_IDENTITY_DATA_INVALID",
                        "Provider ticker exchange identity does not match the requested exchange.",
                    ));
                }
                validate_identifier(&ticker.base)?;
                validate_identifier(&ticker.target)?;
                if let Some(market) = scan
                    .pairs
                    .get(&(ticker.base.clone(), ticker.target.clone()))
                {
                    add_candidate(
                        &mut scan.candidates,
                        &ticker.base,
                        ticker.coin_id.as_deref(),
                        market,
                    )?;
                    add_candidate(
                        &mut scan.candidates,
                        &ticker.target,
                        ticker.target_coin_id.as_deref(),
                        market,
                    )?;
                }
            }
            if complete {
                let now = scan.started_at;
                if self.now() - now >= chrono::Duration::seconds(SCAN_SECONDS) {
                    return Err(unavailable());
                }
                let mappings = std::mem::take(&mut scan.candidates).into_iter().map(|(asset, candidate)| {
                    let identity = if candidate.ids.len() > 1 {
                        Err(failure("PRICE_IDENTITY_CONFLICT", "Provider tickers disagree on the coin identity for one exact venue asset; no coin or price was selected."))
                    } else if candidate.missing || candidate.ids.is_empty() {
                        Err(unavailable())
                    } else {
                        let (id, market_id) = candidate.ids.into_iter().next().expect("one proved identity");
                        Ok(ResolvedExchangeAsset { provider: "coingecko".into(), exchange: key.exchange.clone(), asset: asset.clone(), provider_asset_id: id, market_id, observed_at: now.to_rfc3339() })
                    };
                    (asset, identity)
                }).collect();
                return Ok(CachedIdentities {
                    fetched_at: now,
                    mappings,
                });
            }
            scan.next_page = page + 1;
        }
        Err(invalid_metadata())
    }
}

#[async_trait]
impl ExchangeAssetResolver for CoinGeckoPrices {
    async fn resolve_exchange_asset(
        &self,
        key: &ExchangeAssetKey,
    ) -> Result<ResolvedExchangeAsset> {
        validate_identifier(&key.exchange)?;
        validate_identifier(&key.asset)?;
        let registered = self.exchanges.get(&key.exchange).ok_or_else(|| {
            failure(
                "PRICE_ASSET_UNSUPPORTED",
                "No venue metadata integration is registered for this exact exchange.",
            )
        })?;
        let mut cache = self.identities.lock().await;
        let now = self.now();
        if let Some(cached) = cache.exchanges.get(&key.exchange) {
            let age = now - cached.fetched_at;
            if age >= chrono::Duration::zero() && age < chrono::Duration::seconds(IDENTITY_SECONDS)
            {
                if let Some(identity) = cached.mappings.get(&key.asset) {
                    if identity.is_ok() || age < chrono::Duration::seconds(MISS_REFRESH_SECONDS) {
                        return identity.clone();
                    }
                } else if age < chrono::Duration::seconds(MISS_REFRESH_SECONDS) {
                    return Err(unavailable());
                }
            }
        }
        // Only a complete refreshed snapshot replaces the old identity evidence.
        // Failed/partial refresh never serves expired evidence or a cached price.
        cache.exchanges.remove(&key.exchange);
        self.check_rate_limit().await?;
        let mut pending = cache.pending.remove(&key.exchange).filter(|scan| {
            let age = now - scan.started_at;
            age >= chrono::Duration::zero() && age < chrono::Duration::seconds(SCAN_SECONDS)
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(240),
            self.identity_snapshot(key, registered, &mut pending),
        )
        .await;
        let snapshot = match outcome {
            Ok(Ok(snapshot)) => snapshot,
            failed => {
                let error = match failed {
                    Ok(Err(error)) => error,
                    Err(_) => failure(
                        "PRICE_IDENTITY_PENDING",
                        "Paced identity discovery is still incomplete; saved pagination will resume on the next read. No identity or price was accepted.",
                    ),
                    _ => unreachable!(),
                };
                if matches!(
                    error.code.as_str(),
                    "PRICE_RATE_LIMITED" | "PRICE_IDENTITY_PENDING"
                ) && let Some(scan) = pending
                {
                    cache.pending.insert(key.exchange.clone(), scan);
                }
                self.save_identities(&cache)?;
                return Err(error);
            }
        };
        let result = snapshot
            .mappings
            .get(&key.asset)
            .cloned()
            .unwrap_or_else(|| Err(unavailable()));
        cache.exchanges.insert(key.exchange.clone(), snapshot);
        self.save_identities(&cache)?;
        result
    }
}

fn valid_cache(cache: &IdentityCache) -> bool {
    if cache.exchanges.len() > 16 || cache.pending.len() > 16 {
        return false;
    }
    for (exchange, snapshot) in &cache.exchanges {
        if validate_identifier(exchange).is_err() || snapshot.mappings.len() > MAX_MARKETS {
            return false;
        }
        for (asset, result) in &snapshot.mappings {
            if validate_identifier(asset).is_err() {
                return false;
            }
            if let Ok(id) = result
                && (id.provider != "coingecko"
                    || id.exchange != *exchange
                    || id.asset != *asset
                    || validate_identifier(&id.provider_asset_id).is_err()
                    || validate_identifier(&id.market_id).is_err()
                    || DateTime::parse_from_rfc3339(&id.observed_at).is_err())
            {
                return false;
            }
        }
    }
    for (exchange, scan) in &cache.pending {
        if validate_identifier(exchange).is_err()
            || !(1..=MAX_TICKER_PAGES).contains(&scan.next_page)
            || scan.pairs.len() > MAX_MARKETS
            || scan.candidates.len() > MAX_MARKETS
            || scan.seen_pages.len() > MAX_TICKER_PAGES
            || scan.seen_pages.iter().any(|page| page.len() > PAGE_SIZE)
        {
            return false;
        }
        for ((base, quote), market) in &scan.pairs {
            if [base, quote, market]
                .iter()
                .any(|s| validate_identifier(s).is_err())
            {
                return false;
            }
        }
        for (asset, candidate) in &scan.candidates {
            if validate_identifier(asset).is_err()
                || candidate.ids.len() > MAX_MARKETS
                || candidate.ids.iter().any(|(id, market)| {
                    validate_identifier(id).is_err() || validate_identifier(market).is_err()
                })
            {
                return false;
            }
        }
    }
    true
}

pub(super) struct BinanceMarkets {
    client: reqwest::Client,
    rate_limited_until: Mutex<Option<DateTime<Utc>>>,
    #[cfg(test)]
    pub(super) test_url: Option<String>,
}
impl BinanceMarkets {
    pub(super) fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            rate_limited_until: Mutex::new(None),
            #[cfg(test)]
            test_url: None,
        }
    }
}

#[async_trait]
impl ExchangeMarketSource for BinanceMarkets {
    async fn markets(&self, exchange: &str) -> Result<Vec<ExchangeMarket>> {
        if exchange != "binance" {
            return Err(unavailable());
        }
        let now = Utc::now();
        if self
            .rate_limited_until
            .lock()
            .await
            .is_some_and(|until| now < until)
        {
            return Err(failure(
                "PRICE_VENUE_RATE_LIMITED",
                "Venue Retry-After cooldown is active; no identity was accepted.",
            ));
        }
        let url = "https://api.binance.com/api/v3/exchangeInfo?showPermissionSets=false";
        #[cfg(test)]
        let url = self.test_url.as_deref().unwrap_or(url);
        // API keys are attached per CoinGecko request, never to this client or venue read.
        let mut response = self.client.get(url).send().await.map_err(|_| {
            failure(
                "PRICE_VENUE_UNAVAILABLE",
                "Public venue identity metadata is unavailable.",
            )
        })?;
        if matches!(response.status().as_u16(), 429 | 418) {
            let seconds = retry_after_seconds(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok()),
                Utc::now(),
            );
            *self.rate_limited_until.lock().await =
                Some(Utc::now() + chrono::Duration::seconds(seconds));
            return Err(failure(
                "PRICE_VENUE_RATE_LIMITED",
                "Venue market metadata is rate limited; no identity was accepted.",
            ));
        }
        if response.status().as_u16() != 200 {
            return Err(failure(
                "PRICE_VENUE_UNAVAILABLE",
                "Public venue identity metadata returned an unsuccessful response.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_VENUE_BYTES as u64)
        {
            return Err(invalid_metadata());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| invalid_metadata())? {
            if chunk.len() > MAX_VENUE_BYTES - bytes.len() {
                return Err(invalid_metadata());
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Metadata {
            symbols: Vec<Symbol>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Symbol {
            symbol: String,
            base_asset: String,
            quote_asset: String,
            status: String,
            is_spot_trading_allowed: bool,
        }
        let data: Metadata = serde_json::from_slice(&bytes).map_err(|_| invalid_metadata())?;
        Ok(data
            .symbols
            .into_iter()
            .map(|row| ExchangeMarket {
                market_id: row.symbol,
                base_asset: row.base_asset,
                quote_asset: row.quote_asset,
                active: row.status == "TRADING" && row.is_spot_trading_allowed,
            })
            .collect())
    }
}
