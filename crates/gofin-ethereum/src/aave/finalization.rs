//! Pure final assembly for an Aave candidate and its exact simulation evidence.
//! This module does not access a clock, provider, signer, journal or adapter.
use super::super::*;
use crate::abi::Pool;
use gofin_evm::alloy::{primitives::Bytes, sol_types::SolCall};

pub(crate) struct AaveFinalFacts<'a> {
    pub operation: &'a str,
    pub input: &'a Value,
    pub request: &'a request::Request,
    pub token: &'a reads::Token,
    pub chain_id: u64,
    pub pool: &'a str,
    pub amount: U256,
    pub native_asset: &'a AssetId,
    pub binding: Binding,
    pub intent: FinancialIntent,
    pub warnings: Vec<String>,
    pub simulation: &'a Bytes,
    pub original: Option<&'a Binding>,
    /// Already verified unit-price observations; absence leaves valuation absent.
    pub prices: Option<Vec<AssetPrice>>,
    pub observed_at: String,
    pub expires_at: String,
}

fn valuation(facts: &AaveFinalFacts<'_>, binding: &Binding, gas: U256) -> Result<Valuation> {
    let prices = facts
        .prices
        .clone()
        .ok_or_else(|| GoFinError::validation("Unit price observations are unavailable."))?;
    let movement = if facts.operation == "aave.collateral" {
        binding.supplied_before.clone().unwrap_or_default()
    } else {
        quantity(facts.amount)
    };
    let additional = if facts.operation == "aave.borrow" {
        let rounding = binding
            .debt_rounding_maximum
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Borrow rounding bound is missing."))?;
        quantity(add(facts.amount, u256(rounding)?)?)
    } else {
        Quantity::zero()
    };
    value_rows(
        prices,
        &binding.token,
        facts.native_asset,
        movement,
        quantity(gas),
        additional,
    )
}

fn value_rows(
    prices: Vec<AssetPrice>,
    asset: &AssetId,
    native: &AssetId,
    movement: Quantity,
    gas: Quantity,
    additional: Quantity,
) -> Result<Valuation> {
    let from_row = |asset: &AssetId, units: Quantity| -> Result<Quantity> {
        let row = prices
            .iter()
            .find(|price| price.asset == *asset)
            .ok_or_else(|| GoFinError::validation("Exact valuation row missing."))?;
        units.mul_div_ceil(&row.usd_micros_per_unit, &row.unit_atoms)
    };
    let movement_usd_micros = from_row(asset, movement)?;
    let maximum_fee_usd_micros = from_row(native, gas)?;
    let source = prices
        .iter()
        .find(|price| price.asset == *asset)
        .ok_or_else(|| GoFinError::validation("Exact valuation row missing."))?
        .source
        .clone();
    let additional_allocation_usd_micros = if additional.is_zero() {
        Quantity::zero()
    } else {
        from_row(asset, additional)?
    };
    let observed_at = prices
        .iter()
        .map(|price| {
            chrono::DateTime::parse_from_rfc3339(&price.observed_at)
                .map(|time| (time, price.observed_at.clone()))
                .map_err(|_| GoFinError::validation("Price observation timestamp is invalid."))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .min_by_key(|(time, _)| *time)
        .ok_or_else(|| GoFinError::validation("Unit price observations are unavailable."))?
        .1;
    Ok(Valuation {
        movement_usd_micros,
        maximum_fee_usd_micros,
        additional_allocation_usd_micros,
        observed_at,
        source,
        prices,
    })
}

fn check_simulation(
    operation: &str,
    simulation: &Bytes,
    amount: U256,
    withdraw_all: bool,
    minimum_output: Option<&Quantity>,
) -> Result<()> {
    if operation == "aave.repay" {
        let paid = Pool::repayCall::abi_decode_returns(simulation).map_err(|_| {
            error(
                "SIMULATION_RETURN_INVALID",
                "preview",
                "Repay simulation lacks exact underlying amount.",
            )
        })?;
        require(
            paid > U256::ZERO && paid <= amount,
            "REPAY_SIMULATION_LIMIT",
            "Simulated repayment exceeds the approved finite debit or clears no debt.",
        )?;
    }
    if operation == "aave.withdraw" {
        let received = Pool::withdrawCall::abi_decode_returns(simulation).map_err(|_| {
            error(
                "SIMULATION_RETURN_INVALID",
                "preview",
                "Withdraw simulation lacks exact underlying amount.",
            )
        })?;
        let valid = if withdraw_all {
            let minimum = minimum_output
                .ok_or_else(|| GoFinError::validation("Withdraw minimum is missing."))?;
            received <= amount && received >= u256(minimum)?
        } else {
            received == amount
        };
        require(
            valid,
            "WITHDRAW_SIMULATION_LIMIT",
            "Simulated withdrawal differs from requested exact cash amount.",
        )?;
    }
    Ok(())
}

pub(crate) fn finalize_aave(mut facts: AaveFinalFacts<'_>) -> Result<PreparedOperation> {
    if !matches!(
        facts.operation,
        "aave.supply" | "aave.borrow" | "aave.repay" | "aave.withdraw" | "aave.collateral"
    ) {
        return Err(GoFinError::validation(
            "Unsupported Aave finalization operation.",
        ));
    }
    let all = facts.request.amount.as_deref() == Some("all");
    let repay_all = facts.operation == "aave.repay" && all;
    let withdraw_all = facts.operation == "aave.withdraw" && all;
    check_simulation(
        facts.operation,
        facts.simulation,
        facts.amount,
        withdraw_all,
        facts.binding.minimum_output.as_ref(),
    )?;
    if let Some(old) = facts.original {
        require(
            old.scaled_before == facts.binding.scaled_before
                && old.allowance_before == facts.binding.allowance_before
                && old.collateral_before == facts.binding.collateral_before,
            "STATE_CHANGED",
            "Bound scaled principal or authority changed since approval.",
        )?;
        facts.binding.supplied_before = old.supplied_before.clone();
        facts.binding.debt_before = old.debt_before.clone();
        require(
            &facts.binding == old,
            "BINDING_CHANGED",
            "Financial binding changed since approval.",
        )?;
    }
    let resource = &facts.binding.resource;
    let principal = &facts.binding.token;
    super::write_lifecycle::evaluate_accounting(
        &super::write_lifecycle::AccountingFacts {
            operation: facts.operation,
            pool: facts.pool,
            token_reference: &facts.token.reference,
            chain_id: facts.chain_id,
            resource,
            asset: principal,
            binding: &facts.binding,
            amount: facts.amount,
            withdraw_all,
            repay_all,
        },
        &mut facts.intent,
    )?;
    let gas = facts.binding.transaction.maximum_cost()?;
    facts.intent.maximum_costs.push(CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: amount(facts.native_asset, gas),
        treatment: CostTreatment::AdditionalDebit,
    });
    // Optional display prices do not grant or gate the native financial intent.
    let valuation = valuation(&facts, &facts.binding, gas).ok();
    let mut checks = vec![Check {
        name: "chain, deployment, balance and protocol prerequisites".into(), status: CheckStatus::Passed,
        detail: "Live reads validated exact wallet, deployment, token, route and allowance prerequisites.".into(),
    }];
    if facts.binding.permit_template.is_some() {
        checks.push(Check { name: "materialized permit transaction simulation".into(), status: CheckStatus::Unchecked, detail: "The unsigned permit template is not simulated as if it were signed. After authorization, GoFin signs the typed permit, materializes the frozen transaction and simulates that exact transaction before submission.".into() });
    } else {
        checks.push(Check {
            name: "exact transaction simulation".into(),
            status: CheckStatus::Passed,
            detail: "The exact frozen transaction was simulated during preparation.".into(),
        });
    }
    let quantity = quantity(facts.amount).to_decimal(facts.token.decimals);
    let label = crate::amm_permissions::safe_display_label(Some(&facts.token.symbol));
    let recipient = if facts.operation == "aave.withdraw" {
        format!(
            "; recipient {}",
            facts.request.recipient.as_deref().unwrap_or("")
        )
    } else {
        String::new()
    };
    let risk = facts
        .request
        .min_health_factor
        .as_deref()
        .map(|value| format!("; minimum health factor {value}"))
        .unwrap_or_default();
    let debt_limit = facts
        .request
        .max_debt_usd
        .as_deref()
        .map(|value| format!("; maximum outstanding debt ${value}"))
        .unwrap_or_default();
    let ratio = facts
        .request
        .max_debt_to_collateral_bps
        .map(|value| format!("; maximum debt/collateral {value} bps"))
        .unwrap_or_default();
    let action = if facts.operation == "aave.collateral" {
        format!(
            "{} {} collateral",
            if facts.request.enabled == Some(true) {
                "Enable"
            } else {
                "Disable"
            },
            label
        )
    } else {
        format!(
            "{} {} {}",
            facts
                .operation
                .strip_prefix("aave.")
                .unwrap_or(facts.operation),
            if all {
                format!("all, capped at {quantity}")
            } else {
                quantity
            },
            label
        )
    };
    let summary = format!(
        "{action} on Aave V3, chain {}; maximum fee {} ETH{recipient}{risk}{debt_limit}{ratio}. Supplied assets and borrowed debt remain separate from wallet cash.",
        facts.chain_id,
        facts.request.max_fee_eth.as_deref().unwrap_or("")
    );
    Ok(PreparedOperation {
        operation: facts.operation.into(),
        input: facts.input.clone(),
        binding: serde_json::to_value(facts.binding)
            .map_err(|_| GoFinError::validation("Aave binding cannot be serialized."))?,
        summary,
        checks,
        warnings: facts.warnings,
        observed_at: facts.observed_at,
        expires_at: facts.expires_at,
        valuation,
        intent: facts.intent,
        operation_version: 1,
        action_deadline: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(value: u64) -> Bytes {
        Bytes::copy_from_slice(&U256::from(value).to_be_bytes::<32>())
    }
    #[test]
    fn valuation_uses_rounded_unit_rows_and_oldest_observation() {
        let principal = AssetId {
            network: "ethereum:1".into(),
            reference: "principal".into(),
            decimals: 0,
        };
        let native = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 0,
        };
        let row = |asset: AssetId, usd: u64, units: u64, at: &str| AssetPrice {
            asset,
            usd_micros_per_unit: Quantity::from_u64(usd),
            unit_atoms: Quantity::from_u64(units),
            observed_at: at.into(),
            source: "fixture".into(),
        };
        let prices = vec![
            row(principal.clone(), 5, 2, "2026-01-02T00:00:00Z"),
            row(native.clone(), 3, 2, "2026-01-01T00:00:00Z"),
        ];
        let value = value_rows(
            prices.clone(),
            &principal,
            &native,
            Quantity::from_u64(3),
            Quantity::from_u64(3),
            Quantity::from_u64(4),
        )
        .unwrap();
        assert_eq!(value.movement_usd_micros, Quantity::from_u64(8));
        assert_eq!(value.maximum_fee_usd_micros, Quantity::from_u64(5));
        assert_eq!(
            value.additional_allocation_usd_micros,
            Quantity::from_u64(10)
        );
        assert_eq!(value.observed_at, "2026-01-01T00:00:00Z");
        assert!(
            value_rows(
                vec![prices[0].clone()],
                &principal,
                &native,
                Quantity::from_u64(3),
                Quantity::from_u64(3),
                Quantity::zero()
            )
            .is_err()
        );
    }

    #[test]
    fn exact_simulation_amounts_enforce_repayment_and_withdrawal_bounds() {
        assert!(check_simulation("aave.repay", &encoded(10), U256::from(10), false, None).is_ok());
        for value in [0, 11] {
            assert_eq!(
                check_simulation("aave.repay", &encoded(value), U256::from(10), false, None)
                    .unwrap_err()
                    .code,
                "REPAY_SIMULATION_LIMIT"
            );
        }
        assert_eq!(
            check_simulation("aave.repay", &Bytes::new(), U256::from(10), false, None)
                .unwrap_err()
                .code,
            "SIMULATION_RETURN_INVALID"
        );
        assert!(
            check_simulation("aave.withdraw", &encoded(10), U256::from(10), false, None).is_ok()
        );
        assert_eq!(
            check_simulation("aave.withdraw", &encoded(9), U256::from(10), false, None)
                .unwrap_err()
                .code,
            "WITHDRAW_SIMULATION_LIMIT"
        );
        let minimum = Quantity::from_u64(8);
        for value in [8, 10] {
            assert!(
                check_simulation(
                    "aave.withdraw",
                    &encoded(value),
                    U256::from(10),
                    true,
                    Some(&minimum)
                )
                .is_ok()
            );
        }
        for value in [7, 11] {
            assert_eq!(
                check_simulation(
                    "aave.withdraw",
                    &encoded(value),
                    U256::from(10),
                    true,
                    Some(&minimum)
                )
                .unwrap_err()
                .code,
                "WITHDRAW_SIMULATION_LIMIT"
            );
        }
    }
}
