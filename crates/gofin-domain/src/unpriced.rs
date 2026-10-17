//! Explicit unavailable-price estimates, never evidence of an asset worth zero.
use crate::{AssetId, AssetPrice, Quantity};
pub const PREFIX: &str = "GoFin unpriced exchange estimate";
pub const WARNING: &str = "No verified USD price: using a $0 estimate. USD limits exclude this unpriced exchange asset; token amounts, fee caps and fresh approval still apply.";
pub const PAPERTRADE_PRINCIPAL_WARNING: &str = "PAPER has no USD price; this exact reviewed token principal is excluded from USD movement limits. Exact token amounts and all USDC and gas costs remain bounded and approved.";

/// The only supported on-chain asset whose principal is deliberately outside
/// the USD movement quota. This is protocol coverage, not a claim that the
/// asset is worth zero and not a general unpriced-asset exception.
pub const PAPERTRADE_PRINCIPAL_NETWORK: &str = "ethereum:999";
pub const PAPERTRADE_PRINCIPAL_REFERENCE: &str = "0xe40f17915daa230324030003a197cdaef2261c0e";
pub const PAPERTRADE_PRINCIPAL_DECIMALS: u8 = 18;
const PAPERTRADE_PRINCIPAL_SOURCE: &str = "GoFin reviewed unpriced principal coverage; protocol=papertrade; chain=999; contract=0xe40f17915daa230324030003a197cdaef2261c0e; status=no_usd_price";

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

pub fn is_papertrade_principal(asset: &AssetId) -> bool {
    asset.network == PAPERTRADE_PRINCIPAL_NETWORK
        && asset
            .reference
            .eq_ignore_ascii_case(PAPERTRADE_PRINCIPAL_REFERENCE)
        && asset.decimals == PAPERTRADE_PRINCIPAL_DECIMALS
}

pub fn papertrade_principal_source(asset: &AssetId) -> Option<&'static str> {
    is_papertrade_principal(asset).then_some(PAPERTRADE_PRINCIPAL_SOURCE)
}

pub fn is_papertrade_principal_coverage(asset: &AssetId, value: &str) -> bool {
    papertrade_principal_source(asset) == Some(value)
}

/// True for explicitly modeled unavailable USD valuation evidence. Exchange
/// opt-in remains separate: reviewed PAPER principal never enables arbitrary
/// exchange exposure, and exchange opt-in never enables arbitrary on-chain
/// assets.
pub fn is_supported_unpriced_source(asset: &AssetId, value: &str) -> bool {
    is_source(asset, value) || is_papertrade_principal_coverage(asset, value)
}

pub fn is_supported_unpriced(price: &AssetPrice) -> bool {
    price.usd_micros_per_unit.is_zero()
        && is_supported_unpriced_source(&price.asset, &price.source)
        && Quantity::from_decimal("1", price.asset.decimals)
            .is_ok_and(|unit| unit == price.unit_atoms)
}

pub fn is_estimate(price: &AssetPrice) -> bool {
    price.usd_micros_per_unit.is_zero()
        && is_source(&price.asset, &price.source)
        && Quantity::from_decimal("1", price.asset.decimals)
            .is_ok_and(|unit| unit == price.unit_atoms)
}

pub fn is_papertrade_principal_evidence(price: &AssetPrice) -> bool {
    price.usd_micros_per_unit.is_zero()
        && is_papertrade_principal_coverage(&price.asset, &price.source)
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

    #[test]
    fn papertrade_coverage_is_an_exact_no_price_principal_exclusion() {
        let asset = AssetId {
            network: PAPERTRADE_PRINCIPAL_NETWORK.into(),
            reference: PAPERTRADE_PRINCIPAL_REFERENCE.into(),
            decimals: PAPERTRADE_PRINCIPAL_DECIMALS,
        };
        let row = AssetPrice {
            asset: asset.clone(),
            unit_atoms: Quantity::from_decimal("1", PAPERTRADE_PRINCIPAL_DECIMALS).unwrap(),
            usd_micros_per_unit: Quantity::zero(),
            source: papertrade_principal_source(&asset).unwrap().into(),
            observed_at: "2026-10-07T00:00:00Z".into(),
        };
        assert!(is_papertrade_principal_evidence(&row));
        assert!(is_supported_unpriced(&row));
        assert!(
            !is_estimate(&row),
            "PAPER is not the exchange zero-estimate exception"
        );

        for mutate in ["contract", "chain", "decimals", "source", "price", "unit"] {
            let mut changed = row.clone();
            match mutate {
                "contract" => {
                    changed.asset.reference = "0x0000000000000000000000000000000000000001".into()
                }
                "chain" => changed.asset.network = "ethereum:998".into(),
                "decimals" => changed.asset.decimals = 6,
                "source" => changed.source.push_str("; other asset"),
                "price" => changed.usd_micros_per_unit = Quantity::from_u64(1),
                _ => changed.unit_atoms = Quantity::from_u64(1),
            }
            assert!(!is_papertrade_principal_evidence(&changed), "{mutate}");
            assert!(!is_supported_unpriced(&changed), "{mutate}");
        }
    }
}
