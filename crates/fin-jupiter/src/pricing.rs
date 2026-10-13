//! Exact-mint Jupiter Price V3 source with RPC-derived quote age.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use fin_domain::{AssetId, FinError, Result};
use fin_pricing::{UsdPriceSource, UsdQuote};
use fin_solana::SolanaRpc;
use reqwest::Method;
use serde_json::{Value, json};

use crate::api::{JupiterApi, JupiterHost, RateBucket, RequestPriority};

const MAX_QUOTE_AGE_SECONDS: i64 = 120;
pub const MARGIN_USD_REFERENCE: &str = "jupiter:margin-usd";
pub const POSITION_USD_PREFIX: &str = "jupiter:position-usd:";

pub fn position_usd_asset(market_mint: &str) -> Result<AssetId> {
    let mint = fin_solana::parse_address(market_mint)?;
    Ok(AssetId {
        network: fin_solana::NETWORK.into(),
        reference: format!("{POSITION_USD_PREFIX}{mint}"),
        decimals: 6,
    })
}

pub struct JupiterUsdPrices {
    api: Arc<dyn JupiterApi>,
    rpc: Arc<dyn SolanaRpc>,
}

impl JupiterUsdPrices {
    pub fn new(api: Arc<dyn JupiterApi>, rpc: Arc<dyn SolanaRpc>) -> Self {
        Self { api, rpc }
    }

    async fn quote_exact(&self, asset: &AssetId) -> Result<UsdQuote> {
        validate_asset(asset)?;
        if is_synthetic_usd(asset) {
            return Ok(synthetic_usd_quote(asset));
        }
        let mint = price_mint(asset)?;
        let path = format!("/price/v3?ids={mint}");
        let response = self
            .api
            .request(
                JupiterHost::Platform,
                Method::GET,
                &path,
                None,
                RateBucket::Main,
                RequestPriority::Normal,
                Duration::from_secs(5),
            )
            .await?;
        let quote = response.get(&mint).ok_or_else(invalid_price)?;
        let decimals = quote
            .get("decimals")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_price)?;
        if decimals != u64::from(asset.decimals) {
            return Err(asset_precision_mismatch());
        }
        let (price, slot, updated) = self.finish_quote(quote, asset.decimals).await?;
        let now = Utc::now();
        Ok(UsdQuote {
            usd_per_unit: price,
            updated_at: updated.to_rfc3339(),
            observed_at: now.to_rfc3339(),
            source: format!("jupiter_price_v3:{mint}:slot:{slot}"),
        })
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
        let age = Utc::now().signed_duration_since(updated);
        if age < ChronoDuration::seconds(-30)
            || age > ChronoDuration::seconds(MAX_QUOTE_AGE_SECONDS)
        {
            return Err(FinError::new(
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
        self.quote_exact(asset).await
    }

    async fn quotes(&self, assets: &[AssetId]) -> Result<Vec<UsdQuote>> {
        if assets.is_empty() {
            return Ok(Vec::new());
        }
        for asset in assets {
            validate_asset(asset)?;
        }
        let mut output = Vec::with_capacity(assets.len());
        for chunk in assets.chunks(50) {
            let priced = chunk
                .iter()
                .filter(|asset| !is_synthetic_usd(asset))
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
                let now = Utc::now();
                priced_quotes.insert(
                    mint.clone(),
                    (
                        decimals,
                        UsdQuote {
                            usd_per_unit: price,
                            updated_at: updated.to_rfc3339(),
                            observed_at: now.to_rfc3339(),
                            source: format!("jupiter_price_v3:{mint}:slot:{slot}"),
                        },
                    ),
                );
            }
            for asset in chunk {
                if is_synthetic_usd(asset) {
                    output.push(synthetic_usd_quote(asset));
                } else {
                    let mint = price_mint(asset)?;
                    let (decimals, quote) = priced_quotes.get(&mint).ok_or_else(invalid_price)?;
                    if *decimals != asset.decimals {
                        return Err(asset_precision_mismatch());
                    }
                    output.push(quote.clone());
                }
            }
        }
        Ok(output)
    }
}

fn validate_asset(asset: &AssetId) -> Result<()> {
    if is_synthetic_usd(asset) {
        return Ok(());
    }
    if asset.network != fin_solana::NETWORK
        || (asset.reference != "native" && fin_solana::parse_address(&asset.reference).is_err())
    {
        return Err(FinError::new(
            "PRICE_ASSET_UNSUPPORTED",
            "valuation",
            "Jupiter pricing requires an exact Solana mainnet-beta mint identity and known decimals.",
            "Use the token's exact mint address and verified mint precision.",
        ));
    }
    Ok(())
}

fn is_synthetic_usd(asset: &AssetId) -> bool {
    if asset.network != fin_solana::NETWORK || asset.decimals != 6 {
        return false;
    }
    asset.reference == MARGIN_USD_REFERENCE
        || asset
            .reference
            .strip_prefix(POSITION_USD_PREFIX)
            .is_some_and(|mint| fin_solana::parse_address(mint).is_ok())
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
    fin_solana::parse_address(&asset.reference)
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

fn invalid_price() -> FinError {
    FinError::new(
        "PRICE_DATA_INVALID",
        "valuation",
        "Jupiter did not return a valid price for the exact mint and block.",
        "Retry pricing for the exact mint; no symbol-based or assumed price is used.",
    )
}

fn asset_precision_mismatch() -> FinError {
    FinError::new(
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
    }

    impl PriceApi {
        fn with_responses(responses: impl IntoIterator<Item = Value>) -> Self {
            Self {
                paths: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().collect()),
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
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| FinError::validation("Synthetic price response is missing."))
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
            network: fin_solana::NETWORK.into(),
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
                network: fin_solana::NETWORK.into(),
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
            network: fin_solana::NETWORK.into(),
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
            network: fin_solana::NETWORK.into(),
            reference: "native".into(),
            decimals: 9,
        };
        let duplicate_wrapped = AssetId {
            network: fin_solana::NETWORK.into(),
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
            network: fin_solana::NETWORK.into(),
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
}
