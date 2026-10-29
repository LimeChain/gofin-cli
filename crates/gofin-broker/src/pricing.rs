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
        let (solana, other) = tokio::try_join!(
            async {
                if solana_assets.is_empty() {
                    Ok(Vec::new())
                } else {
                    self.solana.quotes(&solana_assets).await
                }
            },
            async {
                if other_assets.is_empty() {
                    Ok(Vec::new())
                } else {
                    self.fallback.quotes(&other_assets).await
                }
            },
        )?;
        if solana.len() != solana_assets.len() || other.len() != other_assets.len() {
            return Err(invalid_quote());
        }
        let mut solana = solana.into_iter();
        let mut other = other.into_iter();
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
        self.apply_quote(asset, &units, &quote)
    }
    fn apply_quote(
        &self,
        asset: &AssetId,
        units: &Quantity,
        quote: &UsdQuote,
    ) -> gofin_domain::Result<(Quantity, String, String)> {
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
        Ok((micros, quote_source(asset, quote), quote.updated_at.clone()))
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

    async fn usd_micros_batch(
        &self,
        amounts: &[Amount],
    ) -> gofin_domain::Result<Vec<(String, String, String)>> {
        let mut assets = Vec::new();
        let mut quotes = BTreeMap::new();
        for amount in amounts {
            let asset = &amount.asset;
            if quotes.contains_key(asset) || assets.contains(asset) {
                continue;
            }
            if gofin_domain::unpriced::papertrade_principal_source(asset).is_some() {
                quotes.insert(asset.clone(), self.quote(asset).await?);
            } else {
                assets.push(asset.clone());
            }
        }
        if !assets.is_empty() {
            let batch = match self.source.quotes(&assets).await {
                Ok(batch) => batch,
                // Preserve the existing exact Binance unsupported-pair marker;
                // an outage, stale quote or any other source failure stays an error.
                Err(error) if error.code == "PRICE_SPOT_PAIR_UNSUPPORTED" => {
                    let mut batch = Vec::with_capacity(assets.len());
                    for asset in &assets {
                        batch.push(self.quote(asset).await?);
                    }
                    batch
                }
                Err(error) => return Err(error),
            };
            if batch.len() != assets.len() {
                return Err(invalid_quote());
            }
            quotes.extend(assets.into_iter().zip(batch));
        }
        amounts
            .iter()
            .map(|amount| {
                let quote = quotes.get(&amount.asset).ok_or_else(invalid_quote)?;
                let (micros, source, updated) =
                    self.apply_quote(&amount.asset, &amount.atoms, quote)?;
                Ok((micros.to_string(), updated, source))
            })
            .collect()
    }

    async fn asset_prices(&self, assets: &[AssetId]) -> gofin_domain::Result<Vec<AssetPrice>> {
        let amounts = assets
            .iter()
            .map(|asset| {
                if asset.decimals > 36 {
                    return Err(error(
                        "PRICE_ASSET_UNSUPPORTED",
                        "valuation",
                        "Asset precision exceeds the reviewed valuation boundary.",
                        "Use an asset with configured exact decimals.",
                    ));
                }
                Ok(Amount {
                    asset: asset.clone(),
                    atoms: Quantity::new(pow10(asset.decimals as u32).to_string())?,
                })
            })
            .collect::<gofin_domain::Result<Vec<_>>>()?;
        // This applies each full provider decimal price to one unit, exactly as
        // asset_price does, while snapshotting all source quotes in one batch.
        let values = self.usd_micros_batch(&amounts).await?;
        amounts
            .into_iter()
            .zip(values)
            .map(|(amount, (micros, observed_at, source))| {
                Ok(AssetPrice {
                    asset: amount.asset,
                    usd_micros_per_unit: Quantity::new(micros)?,
                    unit_atoms: amount.atoms,
                    observed_at,
                    source,
                })
            })
            .collect()
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
    async fn quote_observations(
        &self,
        amounts: &[Amount],
    ) -> gofin_domain::Result<Vec<(Quantity, String, String)>> {
        self.usd_micros_batch(amounts)
            .await?
            .into_iter()
            .map(|(micros, observed, source)| Ok((Quantity::new(micros)?, source, observed)))
            .collect()
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

    struct BatchSource {
        requests: std::sync::Mutex<Vec<Vec<AssetId>>>,
        rows: Vec<UsdQuote>,
    }
    #[async_trait]
    impl UsdPriceSource for BatchSource {
        async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
            panic!("amount batch must not fetch individual quotes")
        }
        async fn quotes(&self, assets: &[AssetId]) -> gofin_domain::Result<Vec<UsdQuote>> {
            self.requests.lock().unwrap().push(assets.to_vec());
            Ok(self.rows.clone())
        }
    }
    fn batch_quote(price: &str) -> UsdQuote {
        UsdQuote {
            usd_per_unit: price.into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic exact batch".into(),
        }
    }
    fn batch_amount(network: &str, decimals: u8, value: &str) -> Amount {
        Amount {
            asset: AssetId {
                network: network.into(),
                reference: "SAME".into(),
                decimals,
            },
            atoms: Quantity::from_decimal(value, decimals).unwrap(),
        }
    }

    #[tokio::test]
    async fn asset_prices_batch_preserves_unit_conversion_order_and_exact_identity() {
        let first = batch_amount("ethereum:1", 6, "1").asset;
        let other = batch_amount("ethereum:42161", 6, "1").asset;
        let precision = batch_amount("ethereum:1", 18, "1").asset;
        let assets = vec![
            first.clone(),
            other.clone(),
            first.clone(),
            precision.clone(),
        ];
        let source = Arc::new(BatchSource {
            requests: Default::default(),
            rows: vec![
                batch_quote("1.0000001"),
                batch_quote("2000.123456789012345678"),
                batch_quote("0.0000001"),
            ],
        });
        let provider = SharedUsdValuator::with_source(source.clone());
        let rows = provider.asset_prices(&assets).await.unwrap();
        assert_eq!(
            *source.requests.lock().unwrap(),
            vec![vec![first, other, precision]]
        );
        for ((row, asset), expected) in
            rows.iter()
                .zip(&assets)
                .zip(["1000001", "2000123457", "1000001", "1"])
        {
            assert_eq!(&row.asset, asset);
            assert_eq!(
                row.unit_atoms,
                Quantity::from_decimal("1", asset.decimals).unwrap()
            );
            assert_eq!(row.usd_micros_per_unit.as_str(), expected);
            assert_eq!(row.observed_at, source.rows[0].updated_at);
        }
        assert!(provider.asset_prices(&[]).await.unwrap().is_empty());
        assert_eq!(source.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn asset_prices_default_preserves_single_price_overrides_and_errors() {
        struct Override;
        #[async_trait]
        impl AssetValuator for Override {
            async fn usd_micros(
                &self,
                _: &Amount,
            ) -> gofin_domain::Result<(String, String, String)> {
                panic!("batch default must preserve the asset_price override")
            }
            async fn asset_price(&self, asset: &AssetId) -> gofin_domain::Result<AssetPrice> {
                if asset.reference == "FAIL" {
                    return Err(invalid_quote());
                }
                Ok(AssetPrice {
                    asset: asset.clone(),
                    usd_micros_per_unit: Quantity::from_u64(123),
                    unit_atoms: Quantity::from_decimal("1", asset.decimals)?,
                    observed_at: now(),
                    source: "synthetic override".into(),
                })
            }
        }
        let asset = batch_amount("ethereum:1", 6, "1").asset;
        let rows = Override
            .asset_prices(&[asset.clone(), asset.clone()])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.asset == asset && row.usd_micros_per_unit.as_str() == "123")
        );
        let mut failure = asset.clone();
        failure.reference = "FAIL".into();
        assert_eq!(
            Override
                .asset_prices(&[asset, failure])
                .await
                .unwrap_err()
                .code,
            "PRICE_DATA_INVALID"
        );
    }

    #[tokio::test]
    async fn asset_prices_batch_rejects_incomplete_stale_and_unsupported_precision() {
        let asset = batch_amount("ethereum:1", 6, "1").asset;
        let mut stale = batch_quote("1");
        stale.updated_at = "2000-01-01T00:00:00Z".into();
        for quotes in [
            vec![],
            vec![batch_quote("1"), batch_quote("2")],
            vec![stale],
            vec![batch_quote("0")],
        ] {
            let provider = SharedUsdValuator::with_source(Arc::new(BatchSource {
                requests: Default::default(),
                rows: quotes,
            }));
            assert!(
                provider
                    .asset_prices(std::slice::from_ref(&asset))
                    .await
                    .is_err()
            );
        }
        let source = Arc::new(BatchSource {
            requests: Default::default(),
            rows: vec![],
        });
        let provider = SharedUsdValuator::with_source(source.clone());
        let mut invalid = asset;
        invalid.decimals = 37;
        assert_eq!(
            provider.asset_prices(&[invalid]).await.unwrap_err().code,
            "PRICE_ASSET_UNSUPPORTED"
        );
        assert!(source.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wallet_valuation_uses_one_batch_and_keeps_price_times_and_full_precision() {
        let amounts = [
            batch_amount("ethereum:1", 18, "10"),
            batch_amount("ethereum:1", 18, "1"),
        ];
        let source = Arc::new(BatchSource {
            requests: Default::default(),
            rows: vec![batch_quote("0.0000001")],
        });
        let valuator = SharedUsdValuator::with_source(source.clone());
        let values = gofin_ethereum::WalletValuation::quote_observations(&valuator, &amounts)
            .await
            .unwrap();
        assert_eq!(values[0].0, Quantity::from_u64(1));
        assert_eq!(values[1].0, Quantity::from_u64(1));
        assert_eq!(values[0].2, source.rows[0].updated_at);
        assert!(values[0].1.starts_with(&source.rows[0].source));
        assert_eq!(
            *source.requests.lock().unwrap(),
            [vec![amounts[0].asset.clone()]]
        );
    }

    #[tokio::test]
    async fn independent_network_sources_are_polled_together_and_keep_input_order() {
        struct CoordinatedSource {
            barrier: Arc<tokio::sync::Barrier>,
            price: &'static str,
        }
        #[async_trait]
        impl UsdPriceSource for CoordinatedSource {
            async fn quote(&self, _: &AssetId) -> gofin_domain::Result<UsdQuote> {
                self.barrier.wait().await;
                Ok(batch_quote(self.price))
            }
        }
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let source = NetworkUsdPriceSource::new(
            Arc::new(CoordinatedSource {
                barrier: barrier.clone(),
                price: "2",
            }),
            Arc::new(CoordinatedSource {
                barrier,
                price: "3",
            }),
        );
        let assets = [
            batch_amount("ethereum:1", 18, "1").asset,
            batch_amount("solana:mainnet", 9, "1").asset,
        ];
        let quotes =
            tokio::time::timeout(std::time::Duration::from_secs(1), source.quotes(&assets))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            quotes
                .iter()
                .map(|q| q.usd_per_unit.as_str())
                .collect::<Vec<_>>(),
            ["2", "3"]
        );
    }

    #[tokio::test]
    async fn amount_batch_deduplicates_exact_identities_preserves_order_and_full_price_precision() {
        let first = batch_amount("ethereum:1", 6, "10");
        let other_network = batch_amount("ethereum:42161", 6, "1");
        let same_asset = batch_amount("ethereum:1", 6, "1");
        let other_precision = batch_amount("ethereum:1", 9, "2");
        let source = Arc::new(BatchSource {
            requests: Default::default(),
            rows: vec![
                batch_quote("0.0000001"),
                batch_quote("2.0000001"),
                batch_quote("3"),
            ],
        });
        let valuator = SharedUsdValuator::with_source(source.clone());
        let amounts = [
            first.clone(),
            other_network.clone(),
            same_asset,
            other_precision.clone(),
        ];
        let values = valuator.usd_micros_batch(&amounts).await.unwrap();
        assert_eq!(
            values
                .iter()
                .map(|value| value.0.as_str())
                .collect::<Vec<_>>(),
            ["1", "2000001", "1", "6000000"]
        );
        assert_eq!(
            *source.requests.lock().unwrap(),
            [vec![
                first.asset,
                other_network.asset,
                other_precision.asset
            ]]
        );
        assert_eq!(values[0].1, source.rows[0].updated_at);
        assert_eq!(values[2], values[0]);
        // Rounding the tiny whole-unit price to one micro first would incorrectly
        // value ten units at ten micros instead of the exact one micro above.
        assert!(valuator.usd_micros_batch(&[]).await.unwrap().is_empty());
        assert_eq!(source.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn amount_batch_rejects_stale_invalid_and_wrong_cardinality_quotes() {
        let mut stale = batch_quote("1");
        stale.updated_at = (Utc::now() - chrono::Duration::seconds(121)).to_rfc3339();
        let mut future = batch_quote("1");
        future.updated_at = (Utc::now() + chrono::Duration::seconds(20)).to_rfc3339();
        let mut no_source = batch_quote("1");
        no_source.source.clear();
        for rows in [
            vec![],
            vec![batch_quote("1"), batch_quote("1")],
            vec![stale],
            vec![future],
            vec![no_source],
            vec![batch_quote("0")],
            vec![batch_quote("invalid")],
        ] {
            let source = Arc::new(BatchSource {
                requests: Default::default(),
                rows,
            });
            assert!(
                SharedUsdValuator::with_source(source.clone())
                    .usd_micros_batch(&[batch_amount("ethereum:1", 6, "1")])
                    .await
                    .is_err()
            );
            assert_eq!(source.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn network_batch_rejects_extra_provider_rows_before_reordering() {
        let source = Arc::new(BatchSource {
            requests: Default::default(),
            rows: vec![batch_quote("1"), batch_quote("2")],
        });
        let valuator = SharedUsdValuator::with_source(Arc::new(NetworkUsdPriceSource::new(
            source.clone(),
            source.clone(),
        )));
        for network in ["ethereum:1", "solana:mainnet-beta"] {
            assert_eq!(
                valuator
                    .usd_micros_batch(&[batch_amount(network, 6, "1")])
                    .await
                    .unwrap_err()
                    .code,
                "PRICE_DATA_INVALID"
            );
        }
        assert_eq!(source.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn amount_batch_preserves_exact_unpriced_exceptions_and_rejects_outages() {
        struct Unsupported {
            individual: std::sync::atomic::AtomicUsize,
            outage: bool,
        }
        #[async_trait]
        impl UsdPriceSource for Unsupported {
            async fn quote(&self, asset: &AssetId) -> gofin_domain::Result<UsdQuote> {
                self.individual
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if asset.network == "binance" {
                    Err(GoFinError::new(
                        "PRICE_SPOT_PAIR_UNSUPPORTED",
                        "valuation",
                        "Synthetic unsupported pair",
                        "Inspect exact asset",
                    ))
                } else {
                    Ok(batch_quote("2"))
                }
            }
            async fn quotes(&self, _: &[AssetId]) -> gofin_domain::Result<Vec<UsdQuote>> {
                Err(GoFinError::new(
                    if self.outage {
                        "PRICE_UNAVAILABLE"
                    } else {
                        "PRICE_SPOT_PAIR_UNSUPPORTED"
                    },
                    "valuation",
                    "Synthetic batch failure",
                    "Retry read",
                ))
            }
        }
        let paper = Amount {
            asset: AssetId {
                network: gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_NETWORK.into(),
                reference: gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_REFERENCE.into(),
                decimals: gofin_domain::unpriced::PAPERTRADE_PRINCIPAL_DECIMALS,
            },
            atoms: Quantity::from_u64(1),
        };
        let binance = batch_amount("binance", 36, "1");
        let source = Arc::new(Unsupported {
            individual: Default::default(),
            outage: false,
        });
        let values = SharedUsdValuator::with_source(source.clone())
            .usd_micros_batch(&[
                paper.clone(),
                binance.clone(),
                batch_amount("ethereum:1", 6, "1"),
                paper.clone(),
            ])
            .await
            .unwrap();
        assert_eq!(
            values
                .iter()
                .map(|value| value.0.as_str())
                .collect::<Vec<_>>(),
            ["0", "0", "2000000", "0"]
        );
        assert!(gofin_domain::unpriced::is_supported_unpriced_source(
            &paper.asset,
            &values[0].2
        ));
        assert!(gofin_domain::unpriced::is_supported_unpriced_source(
            &binance.asset,
            &values[1].2
        ));
        assert_eq!(
            source.individual.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        let outage = Arc::new(Unsupported {
            individual: Default::default(),
            outage: true,
        });
        assert_eq!(
            SharedUsdValuator::with_source(outage.clone())
                .usd_micros_batch(&[binance])
                .await
                .unwrap_err()
                .code,
            "PRICE_UNAVAILABLE"
        );
        assert_eq!(
            outage.individual.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
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
