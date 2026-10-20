//! Exact reviewed rounding kernels; never infer math from a revision number alone.
//! Horizon source: aave/aave-v3-horizon@311a59fb1cec23ee254e84b28759fd1faf9c3711.
use super::super::*;
use gofin_evm::quantity;
pub const HORIZON_POOL: &str = "0xae05cd22df81871bc7cc2a04becfb516bfe332c8";
pub const HORIZON_SOURCE_COMMIT: &str = "311a59fb1cec23ee254e84b28759fd1faf9c3711";
/// Finite mint exposure used identically by plan review and concrete borrowing.
pub(crate) fn borrow_rounding_limit(index: U256) -> Result<U256> {
    index_rounding_limit(
        index,
        "DEBT_INDEX_INVALID",
        "Borrow normalized index exceeds reviewed finite bounds.",
    )
}
/// Finite supply rounding exposure, even when one observed execution rounds to zero.
pub(crate) fn supply_rounding_limit(index: U256) -> Result<U256> {
    index_rounding_limit(
        index,
        "SUPPLY_INDEX_INVALID",
        "Supply normalized index exceeds reviewed finite bounds.",
    )
}
fn index_rounding_limit(index: U256, code: &str, message: &str) -> Result<U256> {
    require(
        index >= pow10(27)? && index < (U256::from(1) << 128),
        code,
        message,
    )?;
    add(
        add(index, pow10(27)? - U256::from(1))? / pow10(27)?,
        U256::from(2),
    )
}

impl EthereumAdapter {
    pub(crate) fn aave_added_debt_base(
        &self,
        amount: U256,
        price: U256,
        decimals: u8,
    ) -> Result<U256> {
        require(
            !price.is_zero(),
            "ORACLE_INVALID",
            "Debt risk price must be positive.",
        )?;
        let product = mul(amount, price)?;
        let unit = pow10(decimals)?;
        if self.horizon_math() {
            Ok(product / unit)
        } else {
            Ok(add(product, unit - U256::from(1))? / unit)
        }
    }

    pub(crate) fn horizon_math(&self) -> bool {
        self.config.chain_id == 1
            && self
                .config
                .aave
                .as_ref()
                .is_some_and(|m| m.pool == HORIZON_POOL)
    }
    pub(crate) fn supply_revision(&self) -> U256 {
        U256::from(if self.horizon_math() { 3 } else { 5 })
    }
    pub(crate) fn debt_revision(&self) -> U256 {
        U256::from(if self.horizon_math() { 1 } else { 5 })
    }
    pub(crate) fn backing_method(&self) -> &'static str {
        if self.horizon_math() {
            "aave-horizon-revision-3-1-half-up-backing"
        } else {
            "aave-v3-revision-5-exact-scaled-backing"
        }
    }
    pub(crate) fn position_backing(
        &self,
        scaled: U256,
        index: U256,
        kind: &PositionKind,
    ) -> Result<U256> {
        if self.horizon_math() {
            Ok(add(mul(scaled, index)?, pow10(27)? / U256::from(2))? / pow10(27)?)
        } else {
            anchors::backing(scaled, index, kind)
        }
    }
    pub(crate) fn scaled_change(
        &self,
        amount: U256,
        index: U256,
        kind: &PositionKind,
        mint: bool,
    ) -> Result<U256> {
        require(
            index >= pow10(27)?,
            "AAVE_INDEX_INVALID",
            "Normalized index must be at least one ray.",
        )?;
        let rounding = if self.horizon_math() {
            index / U256::from(2)
        } else if (*kind == PositionKind::VariableDebt) == mint {
            index - U256::from(1)
        } else {
            U256::ZERO
        };
        Ok(add(mul(amount, pow10(27)?)?, rounding)? / index)
    }
}
pub(crate) fn signed_difference(positive: U256, negative: U256) -> SignedQuantity {
    if positive >= negative {
        SignedQuantity::positive(&quantity(positive - negative))
    } else {
        SignedQuantity::negative(&quantity(negative - positive))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn horizon_full_close_inverts_backing_exactly_for_both_positions() {
        let mut a = crate::tests::adapter(Arc::new(crate::tests::Fixture::new()));
        a.config.aave = Some(AaveConfig {
            pool: HORIZON_POOL.into(),
            oracle: "0x985bcfab7e0f4ef2606cc5b64fc1a16311880442".into(),
        });
        let ray = pow10(27).unwrap();
        for index in [
            ray,
            ray * U256::from(11) / U256::from(10),
            ray * U256::from(3) / U256::from(2),
            ray * U256::from(3),
        ] {
            for scaled in 1u64..100 {
                for kind in [PositionKind::Supply, PositionKind::VariableDebt] {
                    let claim = a
                        .position_backing(U256::from(scaled), index, &kind)
                        .unwrap();
                    assert_eq!(
                        a.scaled_change(claim, index, &kind, false).unwrap(),
                        U256::from(scaled)
                    );
                }
            }
        }
    }
}
