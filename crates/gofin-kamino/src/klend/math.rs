//! Exact native KLend fractions. The pinned program uses 60 fractional bits.
use super::{protocol::invalid, types::LendingRisk};
use gofin_domain::Result;
use klend_interface::state::{BigFractionBytes, Reserve};
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};

pub(crate) fn scale() -> BigUint {
    BigUint::from(1_u8) << 60
}
pub(crate) fn integer(value: &str) -> Result<BigUint> {
    let n = value.parse::<BigUint>().map_err(|_| invalid())?;
    if n.to_string() != value {
        return Err(invalid());
    }
    Ok(n)
}
pub(crate) fn atoms(value: &str) -> Result<u64> {
    integer(value)?.to_u64().ok_or_else(invalid)
}
pub(crate) fn ceil_sf(value: &BigUint) -> BigUint {
    (value + scale() - BigUint::from(1_u8)) >> 60
}
pub(crate) fn index(value: &BigFractionBytes) -> BigUint {
    BigUint::from_bytes_le(
        &value
            .value
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )
}
pub(crate) fn accrued(before: &BigUint, old: &BigUint, new: &BigUint) -> Result<BigUint> {
    if old.is_zero() || new < old {
        return Err(invalid());
    }
    Ok(before * new / old)
}
pub(crate) fn backing_sf(reserve: &Reserve, collateral: u64) -> Result<BigUint> {
    let gross = BigUint::from(reserve.liquidity.total_available_amount) * scale()
        + BigUint::from(u128::from(reserve.liquidity.borrowed_amount_sf));
    let fees = BigUint::from(u128::from(reserve.liquidity.accumulated_protocol_fees_sf))
        + BigUint::from(u128::from(reserve.liquidity.accumulated_referrer_fees_sf))
        + BigUint::from(u128::from(reserve.liquidity.pending_referrer_fees_sf));
    if gross < fees || (reserve.collateral.mint_total_supply == 0 && collateral != 0) {
        return Err(invalid());
    }
    if collateral == 0 {
        return Ok(BigUint::zero());
    }
    Ok((gross - fees) * collateral / reserve.collateral.mint_total_supply)
}
/// Exclusive origination fee: native max(one atom, fractional fee), then round
/// to nearest atom. Referral allocation partitions this fee; it is not extra.
pub(crate) fn borrow_fee(receive: u64, rate_sf: u64) -> Result<u64> {
    if receive == 0 || rate_sf == 0 {
        return Ok(0);
    }
    let fee_sf = (BigUint::from(receive) * rate_sf).max(scale());
    if fee_sf >= BigUint::from(receive) * scale() {
        return Err(invalid());
    }
    ((fee_sf + (scale() >> 1_usize)) >> 60_usize)
        .to_u64()
        .ok_or_else(invalid)
}
pub(crate) fn check_risk(risk: &LendingRisk, limits: &super::types::RiskLimits) -> Result<()> {
    let debt = integer(&risk.borrow_factor_adjusted_debt_value_sf)?;
    let deposits = integer(&risk.deposited_value_sf)?;
    let unhealthy = integer(&risk.unhealthy_borrow_value_sf)?;
    let raw_debt = integer(&risk.borrowed_assets_market_value_sf)?;
    if !risk.refreshed
        || unhealthy * 10_000_u64 < &debt * limits.minimum_health_factor_bps
        || &debt * 10_000_u64 > deposits * limits.maximum_debt_to_collateral_bps
        || raw_debt * 1_000_000_u64 > integer(&limits.maximum_debt_usd_micros)? * scale()
    {
        return Err(super::protocol::unavailable(
            "The refreshed obligation exceeds the approved health or debt limits.",
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_index_and_fraction_rounding() {
        let b = BigUint::from(10_u8) * scale() + BigUint::from(1_u8);
        assert_eq!(ceil_sf(&b), BigUint::from(11_u8));
        assert_eq!(
            accrued(&b, &BigUint::from(7_u8), &BigUint::from(9_u8)).unwrap(),
            &b * 9_u8 / 7_u8
        );
        assert!(accrued(&b, &BigUint::zero(), &scale()).is_err());
        assert!(accrued(&b, &scale(), &(scale() - BigUint::from(1_u8))).is_err());
        assert!(atoms("01").is_err());
        assert!(atoms("18446744073709551616").is_err());
    }
    #[test]
    fn risk_caps_use_adjusted_debt_and_exact_fraction_boundaries() {
        let mut risk = LendingRisk {
            deposited_value_sf: (scale() * 100_u8).to_string(),
            borrowed_assets_market_value_sf: (scale() * 20_u8).to_string(),
            borrow_factor_adjusted_debt_value_sf: (scale() * 40_u8).to_string(),
            allowed_borrow_value_sf: (scale() * 80_u8).to_string(),
            unhealthy_borrow_value_sf: (scale() * 90_u8).to_string(),
            refreshed: true,
            protocol_state: serde_json::Value::Null,
        };
        let limits = super::super::types::RiskLimits {
            minimum_health_factor_bps: 22500,
            maximum_debt_usd_micros: "20000000".into(),
            maximum_debt_to_collateral_bps: 4000,
        };
        check_risk(&risk, &limits).unwrap();
        risk.borrow_factor_adjusted_debt_value_sf =
            (scale() * 40_u8 + BigUint::from(1_u8)).to_string();
        assert!(check_risk(&risk, &limits).is_err());
        risk.borrow_factor_adjusted_debt_value_sf = (scale() * 40_u8).to_string();
        risk.refreshed = false;
        assert!(check_risk(&risk, &limits).is_err());
    }
    #[test]
    fn fee_rounds_nearest_and_has_native_one_atom_minimum() {
        assert_eq!(borrow_fee(100, 1).unwrap(), 1);
        assert_eq!(borrow_fee(100, 1_u64 << 59).unwrap(), 50);
        assert!(borrow_fee(1, 1).is_err());
        assert_eq!(borrow_fee(100, 0).unwrap(), 0);
    }
}
