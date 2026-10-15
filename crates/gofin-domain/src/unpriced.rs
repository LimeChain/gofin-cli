//! Explicit unavailable-price estimates, never evidence of an asset worth zero.
use crate::{AssetId, AssetPrice, Quantity};
pub const PREFIX: &str = "GoFin unpriced exchange estimate";
pub const WARNING: &str = "No verified USD price: using a $0 estimate. USD limits exclude this unpriced exchange asset; token amounts, fee caps and fresh approval still apply.";

pub fn exchange(network: &str) -> bool {
    network == "binance"
        || network
            .strip_prefix("exchange:")
            .is_some_and(|id| !id.is_empty())
}
pub fn source(asset: &AssetId) -> String {
    format!(
        "{PREFIX}; network={}; exact_asset={}; status=unpriced",
        asset.network, asset.reference
    )
}
pub fn is_source(asset: &AssetId, value: &str) -> bool {
    exchange(&asset.network) && value == source(asset)
}
pub fn is_estimate(price: &AssetPrice) -> bool {
    price.usd_micros_per_unit.is_zero()
        && is_source(&price.asset, &price.source)
        && Quantity::from_decimal("1", price.asset.decimals)
            .is_ok_and(|unit| unit == price.unit_atoms)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zero_estimate_requires_exact_exchange_identity_and_whole_unit() {
        let asset = AssetId {
            network: "binance".into(),
            reference: "APT".into(),
            decimals: 18,
        };
        let row = AssetPrice {
            asset: asset.clone(),
            unit_atoms: Quantity::from_decimal("1", 18).unwrap(),
            usd_micros_per_unit: Quantity::zero(),
            source: source(&asset),
            observed_at: "2026-10-07T00:00:00Z".into(),
        };
        assert!(is_estimate(&row));
        for field in ["asset", "network", "unit", "suffix", "price"] {
            let mut changed = row.clone();
            match field {
                "asset" => changed.asset.reference = "SEI".into(),
                "network" => {
                    changed.asset.network = "ethereum:1".into();
                    changed.source = source(&changed.asset);
                }
                "unit" => changed.unit_atoms = Quantity::from_u64(1),
                "suffix" => changed.source.push_str("; altered"),
                _ => changed.usd_micros_per_unit = Quantity::from_u64(1),
            }
            assert!(!is_estimate(&changed), "{field}");
        }
    }
}
