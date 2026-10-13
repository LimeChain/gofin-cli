//! Exact valuation against broker-owned asset identities and independent USD quotes.
use crate::profile::{KrakenProfile, error};
use chrono::Utc;
use fin_domain::{AssetId, AssetPrice, CostBound, Quantity, ResourceAmount, Result, Valuation};
use std::collections::BTreeMap;
impl KrakenProfile {
    pub(crate) async fn financial_valuation(
        &self,
        debits: &[ResourceAmount],
        costs: &[CostBound],
    ) -> Result<Valuation> {
        let source = self.prices.as_ref().ok_or_else(|| error("KRAKEN_PRICE_SOURCE_UNAVAILABLE", "Kraken writes require a protected exact USD price provider for every principal and fee asset."))?;
        let mut amounts: BTreeMap<AssetId, (Quantity, Quantity)> = BTreeMap::new();
        for debit in debits {
            let totals = amounts.entry(debit.amount.asset.clone()).or_default();
            totals.0 = totals.0.checked_add(&debit.amount.atoms)?;
        }
        for cost in costs {
            let totals = amounts.entry(cost.amount.asset.clone()).or_default();
            totals.1 = totals.1.checked_add(&cost.amount.atoms)?;
        }
        let mut movement = Quantity::zero();
        let mut fees = Quantity::zero();
        let mut prices = vec![];
        for (asset, (principal, fee)) in amounts {
            let quote = source.quote(&asset).await?;
            if !fin_domain::price_freshness::is_fresh(&quote.updated_at, &quote.source, Utc::now())
            {
                return Err(error(
                    "KRAKEN_PRICE_STALE",
                    "A Kraken principal or fee quote is stale.",
                ));
            }
            let price = Quantity::from_decimal(&quote.usd_per_unit, 36)?;
            if price.is_zero() {
                return Err(error(
                    "KRAKEN_PRICE_UNKNOWN",
                    "Kraken writes require positive verified USD quotes; symbols never imply value.",
                ));
            }
            let denominator =
                Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize + 30)))?;
            movement = movement.checked_add(&principal.mul_div_ceil(&price, &denominator)?)?;
            fees = fees.checked_add(&fee.mul_div_ceil(&price, &denominator)?)?;
            prices.push(AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: price.mul_div_ceil(
                    &Quantity::from_u64(1_000_000),
                    &Quantity::new(format!("1{}", "0".repeat(36)))?,
                )?,
                unit_atoms: Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))?,
                observed_at: quote.updated_at,
                source: quote.source,
            });
        }
        Ok(Valuation {
            movement_usd_micros: movement,
            maximum_fee_usd_micros: fees,
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: fin_domain::now(),
            source: "Kraken exact protected asset USD quotes".into(),
            prices,
        })
    }
}
