//! Reviewed product-local collateral identities; contract symbols are never priced.
use async_trait::async_trait;
use gofin_domain::{AssetId, GoFinError};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use std::sync::Arc;

pub(crate) struct KrakenCollateralPrices(pub Arc<dyn UsdPriceSource>);
#[async_trait]
impl UsdPriceSource for KrakenCollateralPrices {
    async fn quote(&self, asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
        gofin_pricing::require_kraken_tokenized_quote_unit(asset)?;
        if asset.network != "krakenfutures" {
            return self.0.quote(asset).await;
        }
        if asset.reference == "USD" {
            let now = gofin_domain::now();
            return Ok(UsdQuote {
                usd_per_unit: "1".into(),
                updated_at: now.clone(),
                observed_at: now,
                source:
                    "Kraken Derivatives USD collateral denomination (one US dollar per USD unit)"
                        .into(),
            });
        }
        // Explicit native venue identities, not symbol search or stablecoin parity.
        // Inverse wallets use xbt/eth; multi-collateral wallets use currency codes.
        let spot = match asset.reference.as_str() {
            "xbt" | "XBT" | "BTC" => "XXBT",
            "eth" | "ETH" => "XETH",
            "USDC" | "usdc" => "USDC",
            "USDT" | "usdt" => "USDT",
            // Both pinned official Accounts/Assets examples identify this
            // exact euro denomination. Its USD rate still needs a fresh book.
            "EUR" | "eur" => "ZEUR",
            _ => {
                return Err(GoFinError::new(
                    "KRAKEN_COLLATERAL_PRICE_IDENTITY_UNSUPPORTED",
                    "valuation",
                    "This Derivatives collateral currency has no reviewed cross-product pricing identity.",
                    "Use supported collateral or add reviewed native identity evidence; a symbol match cannot authorize a valuation.",
                ));
            }
        };
        let mut quote = self
            .0
            .quote(&AssetId {
                network: "kraken".into(),
                reference: spot.into(),
                decimals: asset.decimals,
            })
            .await?;
        quote.source = format!(
            "{}; reviewed collateral identity krakenfutures:{} -> kraken:{}",
            quote.source, asset.reference, spot
        );
        Ok(quote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Reject;
    struct Euro;
    #[async_trait]
    impl UsdPriceSource for Euro {
        async fn quote(&self, asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
            assert_eq!(asset.network, "kraken");
            assert_eq!(asset.reference, "ZEUR");
            assert_eq!(asset.decimals, 36);
            let at = gofin_domain::now();
            Ok(UsdQuote {
                usd_per_unit: "1.123456789123456789".into(),
                updated_at: at.clone(),
                observed_at: at,
                source: "SYNTHETIC exact native EUR/USD book fixture".into(),
            })
        }
    }
    #[async_trait]
    impl UsdPriceSource for Reject {
        async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
            Err(GoFinError::validation("No price source"))
        }
    }
    #[tokio::test]
    async fn fiat_dollars_are_explicit_and_stablecoins_require_external_quotes() {
        let source = KrakenCollateralPrices(Arc::new(Reject));
        let mut asset = AssetId {
            network: "krakenfutures".into(),
            reference: "USD".into(),
            decimals: 36,
        };
        assert_eq!(source.quote(&asset).await.unwrap().usd_per_unit, "1");
        asset.reference = "USDT".into();
        assert!(source.quote(&asset).await.is_err());
        asset.reference = "EUR".into();
        assert!(
            source.quote(&asset).await.is_err(),
            "A euro identity does not imply a USD parity quote"
        );
        asset.reference = "SYNTHETICUSD".into();
        assert!(source.quote(&asset).await.is_err());
    }
    #[tokio::test]
    async fn reviewed_euro_collateral_preserves_exact_fx_quote_and_native_identity() {
        for code in ["EUR", "eur"] {
            let quote = KrakenCollateralPrices(Arc::new(Euro))
                .quote(&AssetId {
                    network: "krakenfutures".into(),
                    reference: code.into(),
                    decimals: 36,
                })
                .await
                .unwrap();
            assert_eq!(quote.usd_per_unit, "1.123456789123456789");
            assert!(quote.source.contains("kraken:ZEUR"));
        }
    }
}

#[cfg(test)]
mod tokenized_unit_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct SymbolGuess(AtomicUsize);
    #[async_trait]
    impl UsdPriceSource for SymbolGuess {
        async fn quote(&self, asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let at = gofin_domain::now();
            Ok(UsdQuote {
                usd_per_unit: "123.123456789123456789".into(),
                updated_at: at.clone(),
                observed_at: at,
                source: format!(
                    "SYNTHETIC deliberately unproven symbol lookup {}",
                    asset.reference
                ),
            })
        }
    }
    #[tokio::test]
    async fn protected_default_price_wrapper_never_delegates_tokenized_units_to_symbol_lookup() {
        let underlying = Arc::new(SymbolGuess(AtomicUsize::new(0)));
        let source = KrakenCollateralPrices(underlying.clone());
        for unit in ["base", "rebased"] {
            for name in ["AAPLX.T", "USDC"] {
                let asset = AssetId {
                    network: format!("kraken:tokenized_asset:{unit}"),
                    reference: name.into(),
                    decimals: 36,
                };
                let error = source.quote(&asset).await.unwrap_err();
                assert_eq!(error.code, "KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN");
                assert_eq!(error.details.unwrap()["requiredUnit"], unit);
            }
        }
        assert_eq!(underlying.0.load(Ordering::SeqCst), 0);
        let ordinary = source
            .quote(&AssetId {
                network: "kraken".into(),
                reference: "XETH".into(),
                decimals: 36,
            })
            .await
            .unwrap();
        assert_eq!(ordinary.usd_per_unit, "123.123456789123456789");
        assert_eq!(underlying.0.load(Ordering::SeqCst), 1);
        let dollars = source
            .quote(&AssetId {
                network: "krakenfutures".into(),
                reference: "USD".into(),
                decimals: 36,
            })
            .await
            .unwrap();
        assert_eq!(dollars.usd_per_unit, "1");
        assert_eq!(underlying.0.load(Ordering::SeqCst), 1);
    }
}
