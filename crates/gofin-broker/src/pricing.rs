//! One shared USD source for policy valuation, application previews and allocations.
use super::*;
use gofin_domain::Quantity;
use gofin_pricing::{CoinGeckoPrices, ExchangeSpotPrices, UsdPriceSource, UsdQuote};
#[cfg(test)]
#[path = "exchange_pricing_tests.rs"]
mod exchange_tests;

pub struct SharedUsdValuator {
    source: Arc<dyn UsdPriceSource>,
    clock: Arc<dyn gofin_domain::clock::Clock>,
}

/// Dispatch exact Solana mint identities to Jupiter Price V3 while retaining
/// the configured source for Binance and EVM assets.
pub struct NetworkUsdPriceSource {
    fallback: Arc<dyn UsdPriceSource>,
    solana: Arc<dyn UsdPriceSource>,
}

impl NetworkUsdPriceSource {
    pub fn new(fallback: Arc<dyn UsdPriceSource>, solana: Arc<dyn UsdPriceSource>) -> Self {
        Self { fallback, solana }
    }
    fn source(&self, asset: &AssetId) -> &Arc<dyn UsdPriceSource> {
        if asset.network.starts_with("solana:") {
            &self.solana
        } else {
            &self.fallback
        }
    }
}

#[async_trait]
impl UsdPriceSource for NetworkUsdPriceSource {
    async fn quote(&self, asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
        self.source(asset).quote(asset).await
    }
    async fn quotes(&self, assets: &[AssetId]) -> gofin_domain::Result<Vec<UsdQuote>> {
        let solana_assets = assets
            .iter()
            .filter(|asset| asset.network.starts_with("solana:"))
            .cloned()
            .collect::<Vec<_>>();
        let other_assets = assets
            .iter()
            .filter(|asset| !asset.network.starts_with("solana:"))
            .cloned()
            .collect::<Vec<_>>();
        // Some sources reject empty batch requests (and they should not be
        // contacted when this portfolio slice has no assets for them).
        let mut solana = if solana_assets.is_empty() {
            Vec::new()
        } else {
            self.solana.quotes(&solana_assets).await?
        }
        .into_iter();
        let mut other = if other_assets.is_empty() {
            Vec::new()
        } else {
            self.fallback.quotes(&other_assets).await?
        }
        .into_iter();
        assets
            .iter()
            .map(|asset| {
                let quote = if asset.network.starts_with("solana:") {
                    solana.next()
                } else {
                    other.next()
                };
                quote.ok_or_else(|| {
                    GoFinError::new(
                        "PRICE_DATA_INVALID",
                        "valuation",
                        "A configured exact-asset price source returned an incomplete batch.",
                        "Retry pricing for the exact asset identities.",
                    )
                })
            })
            .collect()
    }
}
impl SharedUsdValuator {
    pub fn new() -> gofin_domain::Result<Self> {
        Ok(Self::with_source(Arc::new(ExchangeSpotPrices::new(
            CoinGeckoPrices::new()?,
        )?)))
    }
    pub fn with_source(source: Arc<dyn UsdPriceSource>) -> Self {
        Self {
            source,
            clock: Arc::new(gofin_domain::clock::SystemClock),
        }
    }
    #[doc(hidden)]
    pub fn with_clock(mut self, clock: Arc<dyn gofin_domain::clock::Clock>) -> Self {
        self.clock = clock;
        self
    }
    async fn quote(&self, asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
        // PaperToken has no supported USD market price. Its exact, reviewed
        // protocol identity carries an explicit no-price coverage marker so
        // only its principal is omitted from USD movement quotas. Never apply
        // this treatment to the surrounding HyperEVM USDC or gas assets.
        if let Some(source) = gofin_domain::unpriced::papertrade_principal_source(asset) {
            let detected = now();
            return Ok(UsdQuote {
                usd_per_unit: "0".into(),
                updated_at: detected.clone(),
                observed_at: detected,
                source: source.into(),
            });
        }
        match self.source.quote(asset).await {
            Err(error)
                if asset.network == "binance" && error.code == "PRICE_SPOT_PAIR_UNSUPPORTED" =>
            {
                // Unknown value, not a provider price. Execution requires the
                // protected opt-in; stale/corrupt/outage evidence is never zero.
                let detected = now();
                Ok(UsdQuote {
                    usd_per_unit: "0".into(),
                    updated_at: detected.clone(),
                    observed_at: detected,
                    source: gofin_domain::unpriced::source(asset),
                })
            }
            result => result,
        }
    }
    async fn amount_quote(
        &self,
        asset: &AssetId,
        units: Quantity,
    ) -> gofin_domain::Result<(Quantity, String, String)> {
        let quote = self.quote(asset).await?;
        let current = self.clock.now();
        if !gofin_domain::price_freshness::is_fresh(&quote.updated_at, &quote.source, current)
            || quote.source.trim().is_empty()
            || quote.source.len() > 1024
            || asset.decimals > 36
        {
            return Err(invalid_quote());
        }
        let price = Quantity::from_decimal(&quote.usd_per_unit, 36)?;
        if price.is_zero()
            && !gofin_domain::unpriced::is_supported_unpriced_source(asset, &quote.source)
        {
            return Err(invalid_quote());
        }
        let denominator = Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize + 30)))?;
        let micros = units.mul_div_ceil(&price, &denominator)?;
        Ok((micros, quote_source(asset, &quote), quote.updated_at))
    }
}
fn invalid_quote() -> GoFinError {
    GoFinError::new(
        "PRICE_DATA_INVALID",
        "valuation",
        "A fresh positive USD quote with provider timestamp and source is required.",
        "Retry when each configured price leg is fresh and valid; no alternative route or stablecoin parity is assumed.",
    )
}
#[async_trait]
impl AssetValuator for SharedUsdValuator {
    async fn usd_micros(&self, amount: &Amount) -> gofin_domain::Result<(String, String, String)> {
        let (micros, source, updated) = self
            .amount_quote(&amount.asset, amount.atoms.clone())
            .await?;
        Ok((micros.to_string(), updated, source))
    }
}
#[async_trait]
impl gofin_ethereum::WalletValuation for SharedUsdValuator {
    async fn quote_usd(
        &self,
        asset: &AssetId,
        units: Quantity,
    ) -> gofin_domain::Result<(Quantity, String)> {
        let (micros, source, _) = self.amount_quote(asset, units).await?;
        Ok((micros, source))
    }
    async fn quote_observation(
        &self,
        asset: &AssetId,
        units: Quantity,
    ) -> gofin_domain::Result<(Quantity, String, String)> {
        self.amount_quote(asset, units).await
    }
}
#[async_trait]
impl gofin_binance::BinancePriceProvider for SharedUsdValuator {
    async fn prefetch_usd(&self, coins: &[String]) -> gofin_domain::Result<()> {
        let assets = coins
            .iter()
            .map(|coin| AssetId {
                network: "binance".into(),
                reference: coin.clone(),
                decimals: 36,
            })
            .collect::<Vec<_>>();
        let quotes = match self.source.quotes(&assets).await {
            Ok(quotes) => quotes,
            Err(error) if error.code == "PRICE_SPOT_PAIR_UNSUPPORTED" => {
                let mut quotes = Vec::with_capacity(assets.len());
                for asset in &assets {
                    quotes.push(self.quote(asset).await?);
                }
                quotes
            }
            Err(error) => return Err(error),
        };
        if quotes.len() != assets.len() {
            return Err(invalid_quote());
        }
        for (asset, quote) in assets.iter().zip(&quotes) {
            validate_binance_quote(asset, quote, self.clock.now())?;
        }
        Ok(())
    }
    async fn quote_usd(&self, coin: &str) -> gofin_domain::Result<gofin_binance::PriceQuote> {
        let asset = AssetId {
            network: "binance".into(),
            reference: coin.into(),
            decimals: 36,
        };
        let quote = self.quote(&asset).await?;
        // Check the same typed source contract before passing whole-unit prices
        // to Binance's exact decimal valuation path.
        validate_binance_quote(&asset, &quote, self.clock.now())?;
        let source = quote_source(&asset, &quote);
        Ok(gofin_binance::PriceQuote {
            usd_per_unit: quote.usd_per_unit,
            observed_at: quote.updated_at.clone(),
            source,
        })
    }
}
fn quote_source(asset: &AssetId, quote: &UsdQuote) -> String {
    if gofin_domain::unpriced::is_supported_unpriced_source(asset, &quote.source) {
        quote.source.clone()
    } else {
        format!("{}; updated:{}", quote.source, quote.updated_at)
    }
}
fn validate_binance_quote(
    asset: &AssetId,
    quote: &UsdQuote,
    current: chrono::DateTime<Utc>,
) -> gofin_domain::Result<()> {
    if !gofin_domain::price_freshness::is_fresh(&quote.updated_at, &quote.source, current)
        || quote.source.trim().is_empty()
        || quote.source.len() > 1024
        || (Quantity::from_decimal(&quote.usd_per_unit, 36)?.is_zero()
            && !gofin_domain::unpriced::is_source(asset, &quote.source))
    {
        return Err(invalid_quote());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub struct FixedUsdSource {
        pub price: String,
    }
    #[async_trait]
    impl UsdPriceSource for FixedUsdSource {
        async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
            Ok(UsdQuote {
                usd_per_unit: self.price.clone(),
                updated_at: now(),
                observed_at: now(),
                source: "synthetic CoinGecko USD quote".into(),
            })
        }
    }
    pub fn fixed_valuator(price: &str) -> SharedUsdValuator {
        SharedUsdValuator::with_source(Arc::new(FixedUsdSource {
            price: price.into(),
        }))
    }

    #[tokio::test]
    async fn paper_token_uses_exact_no_price_coverage_and_other_hyperevm_assets_are_priced() {
        let provider = fixed_valuator("2.5");
        let principal = AssetId {
            network: gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_NETWORK.into(),
            reference: gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_REFERENCE.into(),
            decimals: gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_DECIMALS,
        };
        let amount = Amount {
            asset: principal.clone(),
            atoms: Quantity::from_decimal("1", principal.decimals).unwrap(),
        };
        let (micros, _, source) = provider.usd_micros(&amount).await.unwrap();
        assert_eq!(micros, "0");
        assert!(gofin_domain::unpriced::is_papertrade_principal_coverage(
            &principal, &source
        ));
        assert!(source.contains("no_usd_price"));

        let usdc = Amount {
            asset: AssetId {
                network: "ethereum:999".into(),
                reference: "0x1111111111111111111111111111111111111111".into(),
                decimals: 6,
            },
            atoms: Quantity::from_decimal("1", 6).unwrap(),
        };
        let (micros, _, _) = provider.usd_micros(&usdc).await.unwrap();
        assert_eq!(micros, "2500000");
    }

    #[tokio::test]
    async fn exchange_wallet_and_allocation_paths_share_exact_prices_without_parity() {
        let provider = fixed_valuator("1.02");
        let asset = AssetId {
            network: "binance".into(),
            reference: "USDT".into(),
            decimals: 36,
        };
        for (units, expected) in [
            ("0", "0"),
            ("99", "100980000"),
            ("100.000000000000000000000000000000000001", "102000001"),
        ] {
            let amount = Amount {
                asset: asset.clone(),
                atoms: Quantity::from_decimal(units, 36).unwrap(),
            };
            assert_eq!(provider.usd_micros(&amount).await.unwrap().0, expected);
        }
        let price = provider.asset_price(&asset).await.unwrap();
        assert_eq!(price.usd_micros_per_unit.as_str(), "1020000");
        assert_eq!(price.unit_atoms, Quantity::from_decimal("1", 36).unwrap());
        let quote = gofin_binance::BinancePriceProvider::quote_usd(&provider, "USDT")
            .await
            .unwrap();
        assert_eq!(quote.usd_per_unit, "1.02");
        for network in gofin_networks::all() {
            let asset = AssetId {
                network: network.network_id.into(),
                reference: "native".into(),
                decimals: 18,
            };
            let (micros, _, updated) = gofin_ethereum::WalletValuation::quote_observation(
                &provider,
                &asset,
                Quantity::from_decimal("1", 18).unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(micros, Quantity::from_u64(1020000));
            assert!(chrono::DateTime::parse_from_rfc3339(&updated).is_ok());
        }
    }
    struct ObservedSource {
        updated: String,
    }
    #[async_trait]
    impl UsdPriceSource for ObservedSource {
        async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
            Ok(UsdQuote {
                usd_per_unit: "0.000000000000000000000000000000000001".into(),
                updated_at: self.updated.clone(),
                observed_at: now(),
                source: "synthetic exact source".into(),
            })
        }
    }
    #[tokio::test]
    async fn provider_time_and_tiny_decimal_values_are_not_float_or_retrieval_time() {
        let updated = (Utc::now() - chrono::Duration::seconds(90)).to_rfc3339();
        let provider = SharedUsdValuator::with_source(Arc::new(ObservedSource {
            updated: updated.clone(),
        }));
        let asset = AssetId {
            network: "binance".into(),
            reference: "ETH".into(),
            decimals: 36,
        };
        let (micros, time, _) = provider
            .usd_micros(&Amount {
                asset: asset.clone(),
                atoms: Quantity::from_decimal("1", 36).unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(micros, "1");
        assert_eq!(time, updated);
        for updated in [
            "malformed".into(),
            (Utc::now() - chrono::Duration::seconds(121)).to_rfc3339(),
            (Utc::now() + chrono::Duration::seconds(20)).to_rfc3339(),
        ] {
            let provider = SharedUsdValuator::with_source(Arc::new(ObservedSource { updated }));
            assert!(
                provider
                    .usd_micros(&Amount {
                        asset: asset.clone(),
                        atoms: Quantity::from_u64(1)
                    })
                    .await
                    .is_err()
            );
            assert!(
                gofin_binance::BinancePriceProvider::quote_usd(&provider, "ETH")
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn binance_action_prefetch_uses_shared_batch_source_for_exact_fee_and_movement_assets() {
        struct BatchOnly(std::sync::Mutex<Vec<String>>);
        #[async_trait]
        impl UsdPriceSource for BatchOnly {
            async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
                panic!("prefetch must use the shared batch API");
            }
            async fn quotes(&self, assets: &[AssetId]) -> gofin_domain::Result<Vec<UsdQuote>> {
                for asset in assets {
                    assert_eq!(asset.network, "binance");
                    self.0.lock().unwrap().push(asset.reference.clone());
                }
                Ok(assets
                    .iter()
                    .map(|_| UsdQuote {
                        usd_per_unit: "0.9971".into(),
                        updated_at: now(),
                        observed_at: now(),
                        source: "synthetic exact verified identity".into(),
                    })
                    .collect())
            }
        }
        let source = Arc::new(BatchOnly(Default::default()));
        let valuator = SharedUsdValuator::with_source(source.clone());
        gofin_binance::BinancePriceProvider::prefetch_usd(
            &valuator,
            &["APT".into(), "BNB".into(), "USDC".into()],
        )
        .await
        .unwrap();
        assert_eq!(*source.0.lock().unwrap(), ["APT", "BNB", "USDC"]);
    }
}
