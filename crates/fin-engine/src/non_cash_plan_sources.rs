//! Exact receipt steps may consume one finite noncash source, not reusable
//! principal authority inferred from possible output identities.
use super::*;

pub(super) fn exact_non_cash_receipt_scope(
    step: &financial_plans::FinancialPlanStepSpec,
    permission: &FinancialPlanStepPermission,
) -> bool {
    step.completion.principal_spend.is_none()
        && permission.exact.as_ref().is_some_and(|exact| {
            exact.scope.maximum_debits.is_empty()
                && step
                    .completion
                    .minimum_net_receipt
                    .as_ref()
                    .is_some_and(|target| {
                        exact
                            .scope
                            .possible_credits
                            .contains(&FinancialPlanFundingIdentity {
                                resource: target.resource.clone(),
                                asset: target.amount.asset.clone(),
                            })
                    })
        })
}
pub(super) fn substantive_plan_source_effects(effects: &[FinancialEffect]) -> Result<bool> {
    Ok(super::conversion_sequences::project_effects(effects)?
        .iter()
        .any(|effect| match effect {
            FinancialEffect::OriginMovement { debit, credit, .. }
            | FinancialEffect::Transfer { debit, credit, .. } => {
                !debit.atoms.is_zero() || !credit.atoms.is_zero()
            }
            FinancialEffect::Conversion {
                spent, received, ..
            } => !spent.atoms.is_zero() || !received.atoms.is_zero(),
            FinancialEffect::CashDebit { amount, .. }
            | FinancialEffect::UsableCredit { amount, .. }
            | FinancialEffect::TransitPrincipal { amount, .. }
            | FinancialEffect::Cost { amount, .. } => !amount.atoms.is_zero(),
            FinancialEffect::PositionChange { delta, .. }
            | FinancialEffect::DebtChange { delta, .. }
            | FinancialEffect::Accrual { delta, .. }
            | FinancialEffect::Settlement { delta, .. } => !delta.is_zero(),
            FinancialEffect::AuthorityChange { before, after, .. } => before != after,
            // Mutation acknowledgments are substantive execution too. A receipt
            // source step cannot repurpose them as permission for another attempt.
            _ => true,
        }))
}
pub(super) fn validate_exact_non_cash_receipt_candidate(
    step: &financial_plans::FinancialPlanStepSpec,
    permission: &FinancialPlanStepPermission,
    prepared: &PreparedOperation,
    prior_effects: &[FinancialEffect],
) -> Result<()> {
    if !exact_non_cash_receipt_scope(step, permission) {
        return Ok(());
    }
    let invalid = |message| {
        plan_error(
            "PLAN_NON_CASH_SOURCE_INVALID",
            message,
            "Use the exact finite position-to-cash input; a proved substantive attempt requires fresh authority for a separate step or plan.",
        )
    };
    if substantive_plan_source_effects(prior_effects)? {
        return Err(invalid(
            "This exact noncash receipt step already proved a substantive financial attempt; it grants no reusable position principal cap.",
        ));
    }
    if !prepared.intent.debits.is_empty() || prepared.intent.expected.len() != 3 {
        return Err(invalid(
            "A noncash receipt candidate requires one bounded position decrease, its exact provenance, and a matching cash credit.",
        ));
    }
    let target = step.completion.minimum_net_receipt.as_ref().unwrap();
    let origins = prepared
        .intent
        .expected
        .iter()
        .filter_map(|effect| match effect {
            EffectConstraint::OriginMovement {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } => Some((
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [(from, to, debit_asset, credit_asset, maximum_debit, minimum_credit)] = origins.as_slice()
    else {
        return Err(invalid(
            "Possible credit identities do not authorize a noncash source without one finite typed provenance constraint.",
        ));
    };
    let AccountingLocation::Position {
        resource,
        position,
        position_kind: fin_domain::PositionKind::Supply,
    } = from
    else {
        return Err(invalid(
            "The receipt source must be an exact supply position, not borrowed or inferred cash.",
        ));
    };
    if *to
        != &(AccountingLocation::Cash {
            resource: resource.clone(),
        })
        || resource != &target.resource
        || *debit_asset != *credit_asset
        || *credit_asset != &target.amount.asset
        || position.is_empty()
        || maximum_debit.is_zero()
        || minimum_credit.is_zero()
        || *minimum_credit > *maximum_debit
        || !prepared.intent.locks.contains(resource)
    {
        return Err(invalid(
            "The finite position source and cash receipt must retain exact resource, asset, units and source identity.",
        ));
    }
    let positions = prepared
        .intent
        .expected
        .iter()
        .filter(|effect| match effect {
            EffectConstraint::Position {
                resource: r,
                position: p,
                asset,
                direction: Direction::Decrease,
                maximum_change,
            } => {
                r == resource
                    && p == position
                    && asset == *debit_asset
                    && maximum_change == *maximum_debit
            }
            _ => false,
        })
        .count();
    let credits = prepared
        .intent
        .expected
        .iter()
        .filter(|effect| match effect {
            EffectConstraint::Credit {
                to,
                asset,
                minimum_credit,
            } => {
                to == resource
                    && asset == *credit_asset
                    && !minimum_credit.is_zero()
                    && minimum_credit <= *maximum_debit
            }
            _ => false,
        })
        .count();
    if positions != 1 || credits != 1 {
        return Err(invalid(
            "The position debit and matching cash credit must be independently bounded by the exact provenance source.",
        ));
    }
    Ok(())
}
