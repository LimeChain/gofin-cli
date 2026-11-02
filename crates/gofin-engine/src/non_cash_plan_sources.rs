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
