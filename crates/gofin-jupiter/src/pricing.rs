//! Exact-mint Jupiter Price V3 source with RPC-derived quote age.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use gofin_domain::{AssetId, GoFinError, Result};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::SolanaRpc;
use reqwest::Method;
use serde_json::{Value, json};

use crate::api::{JupiterApi, JupiterHost, RateBucket, RequestPriority};

const MAX_QUOTE_AGE_SECONDS: i64 = 120;
const CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_CACHE_ENTRIES: usize = 256;

trait PriceClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn instant(&self) -> Instant;
}
struct SystemPriceClock;
impl PriceClock for SystemPriceClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn instant(&self) -> Instant {
        Instant::now()
    }
}
struct CachedQuote {
    quote: UsdQuote,
    updated: DateTime<Utc>,
    fetched: Instant,
}
fn provider_time_fresh(updated: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(updated);
    age >= ChronoDuration::seconds(-30) && age <= ChronoDuration::seconds(MAX_QUOTE_AGE_SECONDS)
}
pub const MARGIN_USD_REFERENCE: &str = "jupiter:margin-usd";
pub const POSITION_USD_PREFIX: &str = "jupiter:position-usd:";

pub fn position_usd_asset(market_mint: &str) -> Result<AssetId> {
    let mint = gofin_solana::parse_address(market_mint)?;
    Ok(AssetId {
        network: gofin_solana::NETWORK.into(),
        reference: format!("{POSITION_USD_PREFIX}{mint}"),
        decimals: 6,
    })
}

pub struct JupiterUsdPrices {
    api: Arc<dyn JupiterApi>,
    rpc: Arc<dyn SolanaRpc>,
    // Holding this lock through refresh coalesces simultaneous misses, including
    // single-asset requests overlapping a batch. Errors never enter the cache.
    cache: tokio::sync::Mutex<BTreeMap<AssetId, CachedQuote>>,
    clock: Arc<dyn PriceClock>,
}

impl JupiterUsdPrices {
    pub fn new(api: Arc<dyn JupiterApi>, rpc: Arc<dyn SolanaRpc>) -> Self {
        Self {
            api,
            rpc,
            cache: tokio::sync::Mutex::new(BTreeMap::new()),
            clock: Arc::new(SystemPriceClock),
        }
    }

    async fn finish_quote(
        &self,
        quote: &Value,
        decimals: u8,
    ) -> Result<(String, u64, DateTime<Utc>)> {
        if quote.get("decimals").and_then(Value::as_u64) != Some(u64::from(decimals)) {
            return Err(invalid_price());
        }
        let price = quote
            .get("usdPrice")
            .map(exact_positive_decimal)
            .transpose()?
            .ok_or_else(invalid_price)?;
        let slot = quote
            .get("blockId")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_price)?;
        let block_time = self
            .rpc
            .request("getBlockTime", json!([slot]))
            .await?
            .as_i64()
            .ok_or_else(invalid_price)?;
        let updated = DateTime::from_timestamp(block_time, 0).ok_or_else(invalid_price)?;
        if !provider_time_fresh(updated, self.clock.now()) {
            return Err(GoFinError::new(
                "PRICE_DATA_STALE",
                "valuation",
                "The Jupiter quote is outside its verified block-time freshness window.",
                "Request a fresh exact-mint price before preparing the financial action.",
            ));
        }
        Ok((price, slot, updated))
    }
}

#[async_trait]
impl UsdPriceSource for JupiterUsdPrices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        self.quotes(std::slice::from_ref(asset))
            .await?
            .pop()
            .ok_or_else(invalid_price)
    }

    async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<UsdQuote>> {
        if assets.is_empty() {
            return Ok(Vec::new());
        }
        for asset in assets {
            validate_asset(asset)?;
        }
        let mut cache = self.cache.lock().await;
        let mut output = Vec::with_capacity(assets.len());
        let mut fetched_at = Vec::with_capacity(assets.len());
        for chunk in assets.chunks(50) {
            let now = self.clock.now();
            let instant = self.clock.instant();
            cache.retain(|_, entry| {
                instant
                    .checked_duration_since(entry.fetched)
                    .is_some_and(|age| age < CACHE_TTL)
                    && provider_time_fresh(entry.updated, now)
            });
            let priced = chunk
                .iter()
                .filter(|asset| !is_synthetic_usd(asset) && !cache.contains_key(*asset))
                .collect::<Vec<_>>();
            let mints = price_mints(&priced)?;
            let response = if mints.is_empty() {
                Value::Null
            } else {
                let path = format!(
                    "/price/v3?ids={}",
                    mints
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                );
                self.api
                    .request(
                        JupiterHost::Platform,
                        Method::GET,
                        &path,
                        None,
                        RateBucket::Main,
                        RequestPriority::Normal,
                        Duration::from_secs(5),
                    )
                    .await?
            };
            let mut priced_quotes = std::collections::BTreeMap::new();
            for mint in mints {
                let quote = response.get(&mint).ok_or_else(invalid_price)?;
                let decimals = quote
                    .get("decimals")
                    .and_then(Value::as_u64)
                    .ok_or_else(invalid_price)?;
                let decimals = u8::try_from(decimals).map_err(|_| invalid_price())?;
                let (price, slot, updated) = self.finish_quote(quote, decimals).await?;
                let now = self.clock.now();
                priced_quotes.insert(
                    mint.clone(),
                    (
                        decimals,
                        updated,
                        self.clock.instant(),
                        UsdQuote {
                            usd_per_unit: price,
                            updated_at: updated.to_rfc3339(),
                            observed_at: now.to_rfc3339(),
                            source: format!("jupiter_price_v3:{mint}:slot:{slot}"),
                        },
                    ),
                );
            }
            // Validate every requested precision before publishing this batch's
            // successful observations. Native SOL and wrapped SOL still share a
            // fetched mint row, but cached identities retain their exact decimals.
            for asset in &priced {
                let mint = price_mint(asset)?;
                let (decimals, _, _, _) = priced_quotes.get(&mint).ok_or_else(invalid_price)?;
                if *decimals != asset.decimals {
                    return Err(asset_precision_mismatch());
                }
            }
            for asset in chunk {
                if is_synthetic_usd(asset) {
                    output.push(synthetic_usd_quote(asset));
                    fetched_at.push(None);
                } else if let Some(entry) = cache.get(asset) {
                    output.push(entry.quote.clone());
                    fetched_at.push(Some(entry.fetched));
                } else {
                    let mint = price_mint(asset)?;
                    let (_, _, fetched, quote) =
                        priced_quotes.get(&mint).ok_or_else(invalid_price)?;
                    output.push(quote.clone());
                    fetched_at.push(Some(*fetched));
                }
            }
            // Populate only after assembling the ordered output, so eviction
            // cannot remove a cached row still needed by this same chunk.
            for asset in priced {
                let mint = price_mint(asset)?;
                let (_, updated, fetched, quote) =
                    priced_quotes.get(&mint).ok_or_else(invalid_price)?;
                if cache.len() >= MAX_CACHE_ENTRIES
                    && !cache.contains_key(asset)
                    && let Some(oldest) = cache
                        .iter()
                        .min_by_key(|(_, entry)| entry.fetched)
                        .map(|(key, _)| key.clone())
                {
                    cache.remove(&oldest);
                }
                cache.insert(
                    asset.clone(),
                    CachedQuote {
                        quote: quote.clone(),
                        updated: *updated,
                        fetched: *fetched,
                    },
                );
            }
        }
        // A missing mint can take time to fetch. Recheck every selected row
        // after all awaits, including cache hits selected before that refresh.
        let now = self.clock.now();
        let instant = self.clock.instant();
        let fresh = |quote: &UsdQuote, fetched: Instant| {
            instant
                .checked_duration_since(fetched)
                .is_some_and(|age| age < CACHE_TTL)
                && DateTime::parse_from_rfc3339(&quote.updated_at)
                    .is_ok_and(|updated| provider_time_fresh(updated.with_timezone(&Utc), now))
        };
        cache.retain(|_, entry| fresh(&entry.quote, entry.fetched));
        if output
            .iter()
            .zip(fetched_at)
            .any(|(quote, fetched)| fetched.is_some_and(|at| !fresh(quote, at)))
        {
            return Err(GoFinError::new(
                "PRICE_DATA_STALE",
                "valuation",
                "Jupiter price evidence expired while refreshing the requested assets.",
                "Retry pricing for the exact assets; expired observations are not reused.",
            ));
        }
        Ok(output)
    }
}

fn validate_asset(asset: &AssetId) -> Result<()> {
    if is_synthetic_usd(asset) {
        return Ok(());
    }
    if asset.network != gofin_solana::NETWORK
        || (asset.reference != "native" && gofin_solana::parse_address(&asset.reference).is_err())
    {
        return Err(GoFinError::new(
            "PRICE_ASSET_UNSUPPORTED",
            "valuation",
            "Jupiter pricing requires an exact Solana mainnet-beta mint identity and known decimals.",
            "Use the token's exact mint address and verified mint precision.",
        ));
    }
    Ok(())
}

fn is_synthetic_usd(asset: &AssetId) -> bool {
    if asset.network != gofin_solana::NETWORK || asset.decimals != 6 {
        return false;
    }
    asset.reference == MARGIN_USD_REFERENCE
        || asset
            .reference
            .strip_prefix(POSITION_USD_PREFIX)
            .is_some_and(|mint| gofin_solana::parse_address(mint).is_ok())
}

fn synthetic_usd_quote(asset: &AssetId) -> UsdQuote {
    let now = Utc::now().to_rfc3339();
    UsdQuote {
        usd_per_unit: "1".into(),
        updated_at: now.clone(),
        observed_at: now,
        source: if asset.reference == MARGIN_USD_REFERENCE {
            "jupiter_perps_margin_usd:exact_protocol_usd_unit".into()
        } else {
            format!(
                "jupiter_perps_position_usd:{}:exact_protocol_usd_unit",
                asset.reference
            )
        },
    }
}

fn price_mint(asset: &AssetId) -> Result<String> {
    if asset.reference == "native" {
        if asset.decimals != 9 {
            return Err(invalid_price());
        }
        return Ok("So11111111111111111111111111111111111111112".into());
    }
    gofin_solana::parse_address(&asset.reference)
}

fn price_mints(assets: &[&AssetId]) -> Result<std::collections::BTreeSet<String>> {
    assets.iter().map(|asset| price_mint(asset)).collect()
}

fn exact_positive_decimal(value: &Value) -> Result<String> {
    let raw = value
        .as_number()
        .map(ToString::to_string)
        .or_else(|| value.as_str().map(str::to_owned))
        .ok_or_else(invalid_price)?;
    if raw.is_empty() || raw.len() > 128 || raw.starts_with(['-', '+']) {
        return Err(invalid_price());
    }
    let mut exponent_parts = raw.split(['e', 'E']);
    let mantissa = exponent_parts.next().ok_or_else(invalid_price)?;
    let exponent = match exponent_parts.next() {
        None => 0,
        Some(text) => {
            let exponent = text.parse::<i32>().map_err(|_| invalid_price())?;
            if exponent.unsigned_abs() > 64 {
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
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || integer.is_empty()
        || !integer.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid_price());
    }
    let digits = format!("{integer}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Err(invalid_price());
    }
    let coefficient = digits.trim_end_matches('0');
    let power = exponent - fraction.len() as i32 + (digits.len() - coefficient.len()) as i32;
    if -power > 36 || coefficient.len() as i32 + power > 100 {
        return Err(invalid_price());
    }
    if power >= 0 {
        return Ok(format!("{coefficient}{}", "0".repeat(power as usize)));
    }
    let point = coefficient.len() as i32 + power;
    if point > 0 {
        let point = point as usize;
        Ok(format!(
            "{}.{}",
            &coefficient[..point],
            &coefficient[point..]
        ))
    } else {
        Ok(format!("0.{}{coefficient}", "0".repeat((-point) as usize)))
    }
}

fn invalid_price() -> GoFinError {
    GoFinError::new(
        "PRICE_DATA_INVALID",
        "valuation",
        "Jupiter did not return a valid price for the exact mint and block.",
        "Retry pricing for the exact mint; no symbol-based or assumed price is used.",
    )
}

fn asset_precision_mismatch() -> GoFinError {
    GoFinError::new(
        "PRICE_ASSET_MISMATCH",
        "valuation",
        "Jupiter returned a different mint precision than the selected asset identity.",
        "Refresh token metadata for the exact mint before valuing or preparing a write.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    use crate::api::JupiterApi;
    use async_trait::async_trait;

    #[derive(Default)]
    struct PriceApi {
        paths: Mutex<Vec<String>>,
        responses: Mutex<VecDeque<Value>>,
        advance: Mutex<Option<(Arc<TestClock>, u64)>>,
    }

    impl PriceApi {
        fn with_responses(responses: impl IntoIterator<Item = Value>) -> Self {
            Self {
                paths: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
                advance: Mutex::new(None),
            }
        }

        fn paths(&self) -> Vec<String> {
            self.paths.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl JupiterApi for PriceApi {
        async fn request(
            &self,
            host: JupiterHost,
            method: Method,
            path: &str,
            body: Option<Value>,
            bucket: RateBucket,
            priority: RequestPriority,
            _max_wait: Duration,
        ) -> Result<Value> {
            assert_eq!(host, JupiterHost::Platform);
            assert_eq!(method, Method::GET);
            assert!(body.is_none());
            assert_eq!(bucket, RateBucket::Main);
            assert_eq!(priority, RequestPriority::Normal);
            self.paths.lock().unwrap().push(path.to_owned());
            tokio::task::yield_now().await;
            if let Some((clock, seconds)) = self.advance.lock().unwrap().take() {
                clock.advance(seconds);
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| GoFinError::validation("Synthetic price response is missing."))
        }
    }

    struct PriceRpc {
        timestamp: Option<i64>,
        slots: Mutex<Vec<u64>>,
    }

    impl PriceRpc {
        fn at(timestamp: Option<i64>) -> Self {
            Self {
                timestamp,
                slots: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SolanaRpc for PriceRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            assert_eq!(method, "getBlockTime");
            let slot = params
                .as_array()
                .and_then(|values| values.first())
                .and_then(Value::as_u64)
                .ok_or_else(invalid_price)?;
            self.slots.lock().unwrap().push(slot);
            Ok(self.timestamp.map_or(Value::Null, |time| json!(time)))
        }
    }

    fn price_source(api: Arc<PriceApi>, rpc: Arc<PriceRpc>) -> JupiterUsdPrices {
        JupiterUsdPrices::new(api, rpc)
    }

    fn mint_asset(mint: &str, decimals: u8) -> AssetId {
        AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: mint.into(),
            decimals,
        }
    }

    fn price_row(price: Value, decimals: u64, slot: u64) -> Value {
        json!({"usdPrice":price,"decimals":decimals,"blockId":slot})
    }

    #[test]
    fn prices_remain_decimal_exact_and_reject_float_exponents_and_zero() {
        let numeric = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        assert_eq!(exact_positive_decimal(&numeric("1.2300")).unwrap(), "1.23");
        assert_eq!(exact_positive_decimal(&numeric("12.340")).unwrap(), "12.34");
        assert!(exact_positive_decimal(&numeric("0")).is_err());
        assert_eq!(
            exact_positive_decimal(&numeric("1e-7")).unwrap(),
            "0.0000001"
        );
        assert_eq!(
            price_mint(&AssetId {
                network: gofin_solana::NETWORK.into(),
                reference: "native".into(),
                decimals: 9
            })
            .unwrap(),
            "So11111111111111111111111111111111111111112"
        );
    }

    #[test]
    fn perps_margin_usd_is_an_explicit_one_dollar_protocol_unit() {
        let asset = AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: MARGIN_USD_REFERENCE.into(),
            decimals: 6,
        };
        validate_asset(&asset).unwrap();
        let quote = synthetic_usd_quote(&asset);
        assert_eq!(quote.usd_per_unit, "1");
        assert_eq!(
            quote.source,
            "jupiter_perps_margin_usd:exact_protocol_usd_unit"
        );
        let mut malformed = asset;
        malformed.decimals = 9;
        assert!(validate_asset(&malformed).is_err());
        let position = position_usd_asset("So11111111111111111111111111111111111111112").unwrap();
        assert!(validate_asset(&position).is_ok());
        assert!(
            position
                .reference
                .ends_with("So11111111111111111111111111111111111111112")
        );
        let duplicate_native = AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let duplicate_wrapped = AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: "So11111111111111111111111111111111111111112".into(),
            decimals: 9,
        };
        assert_eq!(
            price_mints(&[&duplicate_native, &duplicate_wrapped])
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn native_and_wrapped_sol_share_one_exact_mint_quote_in_input_order() {
        let wsol = "So11111111111111111111111111111111111111112";
        let api = Arc::new(PriceApi::with_responses([json!({
            wsol: price_row(json!("25.125"), 9, 42)
        })]));
        let now = Utc::now().timestamp();
        let rpc = Arc::new(PriceRpc::at(Some(now)));
        let source = price_source(api.clone(), rpc.clone());
        let margin = AssetId {
            network: gofin_solana::NETWORK.into(),
            reference: MARGIN_USD_REFERENCE.into(),
            decimals: 6,
        };
        let quotes = source
            .quotes(&[mint_asset("native", 9), mint_asset(wsol, 9), margin])
            .await
            .unwrap();

        assert_eq!(quotes.len(), 3);
        assert_eq!(quotes[0].usd_per_unit, "25.125");
        assert_eq!(quotes[1].usd_per_unit, "25.125");
        assert_eq!(quotes[2].usd_per_unit, "1");
        assert_eq!(api.paths(), [format!("/price/v3?ids={wsol}")]);
        assert_eq!(*rpc.slots.lock().unwrap(), [42]);
    }

    #[tokio::test]
    async fn exact_mint_batching_splits_at_fifty_without_losing_order_or_duplicates() {
        let mints = (1_u8..=51)
            .map(|byte| bs58::encode([byte; 32]).into_string())
            .collect::<Vec<_>>();
        let first = mints[..50]
            .iter()
            .map(|mint| (mint.clone(), price_row(json!("2.5"), 6, 55)))
            .collect::<serde_json::Map<_, _>>();
        let last = [(mints[50].clone(), price_row(json!("3.5"), 6, 56))]
            .into_iter()
            .collect::<serde_json::Map<_, _>>();
        let api = Arc::new(PriceApi::with_responses([
            Value::Object(first),
            Value::Object(last),
        ]));
        let now = Utc::now().timestamp();
        let rpc = Arc::new(PriceRpc::at(Some(now)));
        let source = price_source(api.clone(), rpc);
        let assets = mints
            .iter()
            .map(|mint| mint_asset(mint, 6))
            .collect::<Vec<_>>();

        let quotes = source.quotes(&assets).await.unwrap();
        assert_eq!(quotes.len(), 51);
        assert_eq!(quotes[0].usd_per_unit, "2.5");
        assert_eq!(quotes[49].usd_per_unit, "2.5");
        assert_eq!(quotes[50].usd_per_unit, "3.5");
        let paths = api.paths();
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths[0].split("ids=").nth(1).unwrap().split(',').count(),
            50
        );
        assert_eq!(paths[1], format!("/price/v3?ids={}", mints[50]));
    }

    #[tokio::test]
    async fn batched_decimal_mismatch_is_rejected_as_an_asset_identity_error() {
        let mint = bs58::encode([72; 32]).into_string();
        let api = Arc::new(PriceApi::with_responses([json!({
            (mint.clone()): price_row(json!("1"), 9, 70)
        })]));
        let rpc = Arc::new(PriceRpc::at(Some(Utc::now().timestamp())));
        let source = price_source(api, rpc);

        let error = source.quotes(&[mint_asset(&mint, 6)]).await.unwrap_err();
        assert_eq!(error.code, "PRICE_ASSET_MISMATCH");
    }

    #[tokio::test]
    async fn exact_quote_rejects_stale_and_future_block_times_and_missing_evidence() {
        let mint = bs58::encode([73; 32]).into_string();
        let asset = mint_asset(&mint, 6);
        let row = json!({(mint.clone()): price_row(json!("1"), 6, 71)});
        let now = Utc::now().timestamp();
        for (time, expected) in [
            (Some(now - 121), "PRICE_DATA_STALE"),
            (Some(now + 31), "PRICE_DATA_STALE"),
            (None, "PRICE_DATA_INVALID"),
        ] {
            let api = Arc::new(PriceApi::with_responses([row.clone()]));
            let rpc = Arc::new(PriceRpc::at(time));
            let error = price_source(api, rpc).quote(&asset).await.unwrap_err();
            assert_eq!(error.code, expected);
        }

        let api = Arc::new(PriceApi::with_responses([json!({
            (mint.clone()): {"usdPrice":"1","decimals":6}
        })]));
        let rpc = Arc::new(PriceRpc::at(Some(now)));
        let error = price_source(api, rpc).quote(&asset).await.unwrap_err();
        assert_eq!(error.code, "PRICE_DATA_INVALID");

        let api = Arc::new(PriceApi::with_responses([json!({})]));
        let rpc = Arc::new(PriceRpc::at(Some(now)));
        let error = price_source(api, rpc).quote(&asset).await.unwrap_err();
        assert_eq!(error.code, "PRICE_DATA_INVALID");
    }

    #[tokio::test]
    async fn price_v3_values_keep_precision_beyond_f64_integer_range() {
        let mint = bs58::encode([74; 32]).into_string();
        let exact = "9007199254740993.1234567890123456789";
        let row = format!(r#"{{"{mint}":{{"usdPrice":{exact},"decimals":6,"blockId":72}}}}"#);
        let api = Arc::new(PriceApi::with_responses([
            serde_json::from_str(&row).unwrap()
        ]));
        let rpc = Arc::new(PriceRpc::at(Some(Utc::now().timestamp())));
        let quote = price_source(api, rpc)
            .quote(&mint_asset(&mint, 6))
            .await
            .unwrap();
        assert_eq!(quote.usd_per_unit, exact);
    }

    struct TestClock {
        wall: DateTime<Utc>,
        mono: Instant,
        elapsed: std::sync::atomic::AtomicU64,
    }
    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                wall: DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
                mono: Instant::now(),
                elapsed: std::sync::atomic::AtomicU64::new(0),
            })
        }
        fn advance(&self, seconds: u64) {
            self.elapsed
                .fetch_add(seconds, std::sync::atomic::Ordering::SeqCst);
        }
        fn seconds(&self) -> u64 {
            self.elapsed.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl PriceClock for TestClock {
        fn now(&self) -> DateTime<Utc> {
            self.wall + ChronoDuration::seconds(self.seconds() as i64)
        }
        fn instant(&self) -> Instant {
            self.mono + Duration::from_secs(self.seconds())
        }
    }
    fn cached_source(
        api: Arc<PriceApi>,
        clock: Arc<TestClock>,
        provider_age: i64,
    ) -> JupiterUsdPrices {
        let mut source = price_source(
            api,
            Arc::new(PriceRpc::at(Some(clock.now().timestamp() - provider_age))),
        );
        source.clock = clock;
        source
    }

    #[tokio::test]
    async fn repeated_bridge_review_and_authorization_prices_fetch_each_asset_once() {
        let sol = mint_asset("native", 9);
        let usd = mint_asset("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 6);
        let wsol = price_mint(&sol).unwrap();
        let api = Arc::new(PriceApi::with_responses([
            json!({wsol:price_row(json!("150"),9,1)}),
            json!({usd.reference.clone():price_row(json!("1"),6,1)}),
        ]));
        let source = cached_source(api.clone(), TestClock::new(), 1);
        let original = source.quote(&sol).await.unwrap();
        // Quote fees, then approval/burn/mint review and authorization, with
        // repeated gas/rent valuations and optional per-fee USD ceilings.
        for _ in 0..7 {
            for asset in [&sol, &usd, &sol, &sol, &sol, &usd, &sol] {
                source.quote(asset).await.unwrap();
            }
        }
        let reused = source.quote(&sol).await.unwrap();
        assert_eq!(api.paths().len(), 2);
        assert_eq!(reused.updated_at, original.updated_at);
        assert_eq!(reused.observed_at, original.observed_at);
        assert_eq!(reused.source, original.source);
    }

    #[tokio::test]
    async fn cache_ttl_refreshes_at_thirty_seconds_and_failed_refresh_never_serves_old_price() {
        let asset = mint_asset("native", 9);
        let mint = price_mint(&asset).unwrap();
        let row = |value| json!({mint.clone():price_row(json!(value),9,1)});
        let api = Arc::new(PriceApi::with_responses([
            row("150"),
            json!({}),
            row("160"),
        ]));
        let clock = TestClock::new();
        let source = cached_source(api.clone(), clock.clone(), 0);
        source.quote(&asset).await.unwrap();
        clock.advance(29);
        assert_eq!(source.quote(&asset).await.unwrap().usd_per_unit, "150");
        assert_eq!(api.paths().len(), 1);
        clock.advance(1);
        assert_eq!(
            source.quote(&asset).await.unwrap_err().code,
            "PRICE_DATA_INVALID"
        );
        assert!(source.cache.lock().await.is_empty());
        assert_eq!(source.quote(&asset).await.unwrap().usd_per_unit, "160");
        assert_eq!(api.paths().len(), 3);
    }

    #[tokio::test]
    async fn provider_age_expires_before_retrieval_ttl_and_errors_are_not_cached() {
        let asset = mint_asset("native", 9);
        let mint = price_mint(&asset).unwrap();
        let row = json!({mint:price_row(json!("150"),9,1)});
        let api = Arc::new(PriceApi::with_responses([row.clone(), row]));
        let clock = TestClock::new();
        let source = cached_source(api.clone(), clock.clone(), 119);
        source.quote(&asset).await.unwrap();
        clock.advance(1);
        source.quote(&asset).await.unwrap(); // Exactly 120 seconds remains valid.
        clock.advance(1);
        assert_eq!(
            source.quote(&asset).await.unwrap_err().code,
            "PRICE_DATA_STALE"
        );
        assert!(source.cache.lock().await.is_empty());
        assert!(source.quote(&asset).await.is_err()); // Transport error, no stale fallback.
        assert_eq!(api.paths().len(), 3);
    }

    #[tokio::test]
    async fn concurrent_single_and_batch_reads_coalesce_and_reject_wrong_precision() {
        let asset = mint_asset("So11111111111111111111111111111111111111112", 9);
        let mint = price_mint(&asset).unwrap();
        let row = json!({mint:price_row(json!("150"),9,1)});
        let api = Arc::new(PriceApi::with_responses([row.clone(), row]));
        let source = cached_source(api.clone(), TestClock::new(), 0);
        let repeated = vec![asset.clone(); 5];
        let (one, two, three) = tokio::join!(
            source.quote(&asset),
            source.quotes(&repeated),
            source.quote(&asset)
        );
        one.unwrap();
        two.unwrap();
        three.unwrap();
        assert_eq!(api.paths().len(), 1);
        let mut wrong = asset;
        wrong.decimals = 6;
        assert_eq!(
            source.quote(&wrong).await.unwrap_err().code,
            "PRICE_ASSET_MISMATCH"
        );
        assert_eq!(api.paths().len(), 2);
        assert!(!source.cache.lock().await.contains_key(&wrong));
    }

    #[tokio::test]
    async fn cache_is_bounded_and_partial_batches_only_fetch_missing_assets() {
        let assets = (1_u16..=300)
            .map(|i| {
                let mut bytes = [1_u8; 32];
                bytes[..2].copy_from_slice(&i.to_le_bytes());
                mint_asset(&bs58::encode(bytes).into_string(), 6)
            })
            .collect::<Vec<_>>();
        let response = |slice: &[AssetId]| {
            Value::Object(
                slice
                    .iter()
                    .map(|asset| (asset.reference.clone(), price_row(json!("1"), 6, 1)))
                    .collect(),
            )
        };
        let api = Arc::new(PriceApi::with_responses(assets.chunks(50).map(response)));
        let source = cached_source(api.clone(), TestClock::new(), 0);
        assert_eq!(source.quotes(&assets).await.unwrap().len(), 300);
        assert_eq!(source.cache.lock().await.len(), MAX_CACHE_ENTRIES);
        assert_eq!(api.paths().len(), 6);
        assert!(
            api.paths()
                .iter()
                .all(|path| path.split("ids=").nth(1).unwrap().split(',').count() <= 50)
        );
        let (hit, miss) = {
            let cache = source.cache.lock().await;
            (
                assets
                    .iter()
                    .find(|a| cache.contains_key(*a))
                    .unwrap()
                    .clone(),
                assets
                    .iter()
                    .find(|a| !cache.contains_key(*a))
                    .unwrap()
                    .clone(),
            )
        };
        api.responses
            .lock()
            .unwrap()
            .push_back(response(std::slice::from_ref(&miss)));
        source
            .quotes(&[miss.clone(), hit.clone(), miss.clone(), hit])
            .await
            .unwrap();
        assert_eq!(
            api.paths().last().unwrap(),
            &format!("/price/v3?ids={}", miss.reference)
        );
        assert_eq!(source.cache.lock().await.len(), MAX_CACHE_ENTRIES);
    }

    #[tokio::test]
    async fn mixed_refresh_rechecks_cached_ttl_and_provider_age_before_returning() {
        struct CurrentRpc(Arc<TestClock>);
        #[async_trait]
        impl SolanaRpc for CurrentRpc {
            async fn request(&self, method: &str, _: Value) -> Result<Value> {
                assert_eq!(method, "getBlockTime");
                Ok(json!(self.0.now().timestamp()))
            }
        }
        for (initial_age, delay) in [(0, 30), (119, 2)] {
            let hit = mint_asset("native", 9);
            let miss = mint_asset(&bs58::encode([123; 32]).into_string(), 6);
            let mint = price_mint(&hit).unwrap();
            let api = Arc::new(PriceApi::with_responses([
                json!({mint:price_row(json!("150"),9,1)}),
                json!({miss.reference.clone():price_row(json!("1"),6,2)}),
            ]));
            let clock = TestClock::new();
            let mut source = cached_source(api.clone(), clock.clone(), initial_age);
            source.quote(&hit).await.unwrap();
            source.rpc = Arc::new(CurrentRpc(clock.clone()));
            *api.advance.lock().unwrap() = Some((clock, delay));
            let error = source
                .quotes(&[hit.clone(), miss.clone()])
                .await
                .unwrap_err();
            assert_eq!(error.code, "PRICE_DATA_STALE");
            let cache = source.cache.lock().await;
            assert!(!cache.contains_key(&hit));
            assert!(cache.contains_key(&miss));
            assert_eq!(api.paths().len(), 2);
        }
    }
}
