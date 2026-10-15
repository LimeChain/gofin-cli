//! Tokenized asset identity and pair class do not establish market price units.
//! Current native market data has no SPV-base/rebased denomination selector or
//! response field. Keep discovery available; default financial pricing fails
//! with the exact missing evidence instead of a generic token/symbol lookup.
use gofin_domain::{AssetId, GoFinError, Result};
use serde_json::json;

/// Guard the maintained default providers until an official exact-unit quote
/// source is implemented. This performs no API call, alias conversion or
/// custody mutation and grants no unit conversion authority.
pub fn require_kraken_tokenized_quote_unit(asset: &AssetId) -> Result<()> {
    let Some(unit) = asset.network.strip_prefix("kraken:tokenized_asset:") else {
        return Ok(());
    };
    if !matches!(unit, "base" | "rebased") || asset.decimals > 36 {
        return Err(GoFinError::new(
            "PRICE_IDENTITY_INVALID",
            "valuation",
            "This Kraken tokenized asset has an unsupported exact denomination identity.",
            "Use the native base or rebased identity returned by the protected funding discovery; neither implies a conversion factor.",
        ));
    }
    super::resolution::validate_identifier(&asset.reference)?;
    Err(GoFinError::new(
        "KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN",
        "valuation",
        format!("Kraken market data does not establish an exact USD quote per tokenized {unit} unit for this asset."),
        "Funding and market discovery/status remain available. Use a supported exactly priced asset for financial execution; tokenized execution needs authoritative native quote-denomination evidence. A stock symbol or base/rebased balance ratio cannot supply it.",
    ).with_details(json!({"asset":asset,"requiredUnit":unit,"proofGap":"native_market_price_denomination","assetClass":"tokenized_asset","nativeTickerHasRebaseSelector":false,"inferredConversionFactor":false,"publicSchema":"https://docs.kraken.com/api-reference/market-data/get-ticker-information"})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    fn source() -> Value {
        serde_json::from_str(include_str!("../../../../docs/kraken/spot-rest.json")).unwrap()
    }
    fn resolved_parameter(raw: &Value, value: &Value) -> Value {
        if let Some(reference) = value["$ref"].as_str() {
            raw.pointer(&reference[1..]).unwrap().clone()
        } else {
            value.clone()
        }
    }
    #[test]
    fn pinned_market_metadata_proves_classes_and_ids_but_no_tokenized_quote_denomination() {
        let raw = source();
        let ticker = &raw["paths"]["/public/Ticker"]["get"];
        let names = ticker["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                resolved_parameter(&raw, p)["name"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "pair".into(),
                "assetVersion".into(),
                "asset_class".into()
            ])
        );
        let classes = raw["components"]["schemas"]["pairs"]["properties"]
            .as_object()
            .unwrap();
        for name in ["base", "quote", "aclass_base", "aclass_quote"] {
            assert!(classes.contains_key(name));
        }
        for schema in ["info", "pairs", "ticker"] {
            let properties = raw["components"]["schemas"][schema]["properties"]
                .as_object()
                .unwrap();
            for unproven in [
                "rebase_multiplier",
                "multiplier",
                "price_denomination",
                "price_unit",
            ] {
                assert!(
                    !properties.contains_key(unproven),
                    "New native unit evidence must be reviewed: {schema}.{unproven}"
                );
            }
        }
        let units = &raw["components"]["schemas"]["rebase_multiplier"];
        assert!(
            units["description"]
                .as_str()
                .unwrap()
                .contains("SPV tokens")
        );
        assert!(
            units["description"]
                .as_str()
                .unwrap()
                .contains("underlying equity")
        );
        assert_eq!(units["default"], "rebased");
    }
    #[test]
    fn exact_native_tokenized_ticker_identity_is_insufficient_for_either_financial_unit() {
        // Independent realistic primary-shape fixture, unmistakably synthetic.
        // Pair base is the trading component; it is not proof of SPV-base units.
        let asset = json!({"SYNTHX":{"aclass":"tokenized_asset","altname":"SYNTHx","decimals":8,"status":"enabled"}});
        let pair = json!({"SYNTHXUSD":{"aclass_base":"tokenized_asset","base":"SYNTHX","aclass_quote":"currency","quote":"ZUSD","status":"online"}});
        let ticker = json!({"SYNTHXUSD":{"a":["123.123456789012345678","1","1.000"],"b":["122","1","1.000"]}});
        assert_eq!(asset["SYNTHX"]["aclass"], pair["SYNTHXUSD"]["aclass_base"]);
        assert!(ticker.get("SYNTHXUSD").is_some());
        for unit in ["base", "rebased"] {
            let native = AssetId {
                network: format!("kraken:tokenized_asset:{unit}"),
                reference: "SYNTHX".into(),
                decimals: 36,
            };
            let err = require_kraken_tokenized_quote_unit(&native).unwrap_err();
            assert_eq!(err.code, "KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN");
            assert_eq!(err.details.unwrap()["asset"], json!(native));
        }
    }
    #[test]
    fn stock_names_fiat_like_names_and_suffixes_never_provide_unit_authority() {
        for name in ["AAPLx", "AAPLX.T", "USDC", "SYNTHETIC"] {
            for unit in ["base", "rebased"] {
                assert_eq!(
                    require_kraken_tokenized_quote_unit(&AssetId {
                        network: format!("kraken:tokenized_asset:{unit}"),
                        reference: name.into(),
                        decimals: 36
                    })
                    .unwrap_err()
                    .code,
                    "KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN"
                );
            }
        }
        assert_eq!(
            require_kraken_tokenized_quote_unit(&AssetId {
                network: "kraken:tokenized_asset:guessed".into(),
                reference: "SYNTHX".into(),
                decimals: 36
            })
            .unwrap_err()
            .code,
            "PRICE_IDENTITY_INVALID"
        );
        assert!(
            require_kraken_tokenized_quote_unit(&AssetId {
                network: "kraken".into(),
                reference: "XETH".into(),
                decimals: 36
            })
            .is_ok()
        );
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    use crate::{
        ExchangeMarket, ExchangeSpotPrices, SpotBook, SpotMarketSource, StablecoinRates,
        StablecoinUsdSource, UsdPriceSource, UsdQuote,
    };
    use async_trait::async_trait;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct RejectedFallback(AtomicUsize);
    #[async_trait]
    impl UsdPriceSource for RejectedFallback {
        async fn quote(&self, _: &AssetId) -> Result<UsdQuote> {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("No tokenized symbol or chain lookup is authorized")
        }
    }
    #[async_trait]
    impl StablecoinUsdSource for RejectedFallback {
        async fn stablecoin_rates(&self) -> Result<StablecoinRates> {
            panic!("No parity or conversion leg can establish tokenized quote units")
        }
    }
    #[async_trait]
    impl SpotMarketSource for RejectedFallback {
        async fn markets(&self, _: &str) -> Result<Vec<ExchangeMarket>> {
            panic!("No ordinary currency market discovery can prove tokenized SPV price units")
        }
        async fn books(&self, _: &str, _: &[String]) -> Result<Vec<SpotBook>> {
            panic!("No ordinary currency book can substitute quote units")
        }
    }
    #[tokio::test]
    async fn default_exchange_provider_and_mixed_batches_reject_units_before_any_lookup() {
        let source = Arc::new(RejectedFallback(AtomicUsize::new(0)));
        let provider =
            ExchangeSpotPrices::with_sources(source.clone(), source.clone(), source.clone());
        let ordinary = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        for unit in ["base", "rebased"] {
            let asset = AssetId {
                network: format!("kraken:tokenized_asset:{unit}"),
                reference: "SYNTHX".into(),
                decimals: 36,
            };
            assert_eq!(
                provider.quote(&asset).await.unwrap_err().code,
                "KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN"
            );
            assert_eq!(
                provider
                    .quotes(&[ordinary.clone(), asset])
                    .await
                    .unwrap_err()
                    .code,
                "KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN"
            );
        }
        assert_eq!(source.0.load(Ordering::SeqCst), 0);
    }
}
