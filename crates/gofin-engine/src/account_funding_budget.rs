//! Narrow a receipt-discovered funding budget into existing exact constraints.
//!
//! Adapters authenticate account ownership and refund authority from finalized
//! chain evidence. The engine independently checks scope, aggregate new funding,
//! and complete cash/position conservation. No discovered identity is persisted
//! as additional authority.
use super::*;
use gofin_domain::AccountFundingLimit;

pub(super) fn validate(intent: &FinancialIntent) -> Result<()> {
    let mut scopes = BTreeSet::new();
    for constraint in &intent.expected {
        let EffectConstraint::AccountFundingBudget {
            resource,
            asset,
            maximum_debit,
        } = constraint
        else {
            continue;
        };
        let scope = serde_json::to_string(&(resource, asset))
            .map_err(|_| account_funding_effect_mismatch())?;
        if !scopes.insert(scope)
            || !intent.locks.contains(resource)
            || intent.expected.iter().any(|other| {
                matches!(other,
                    EffectConstraint::OptionalAccountFunding { resource: r, asset: a, .. }
                    | EffectConstraint::AccountFundingLifecycle { resource: r, asset: a, .. }
                    if r == resource && a == asset)
            })
        {
            return Err(account_funding_effect_mismatch());
        }
        let reserved = intent
            .debits
            .iter()
            .filter(|debit| debit.resource == *resource && debit.amount.asset == *asset)
            .fold(BigUint::zero(), |total, debit| {
                total + debit.amount.atoms.integer()
            });
        let principal = intent
            .expected
            .iter()
            .filter_map(|other| match other {
                EffectConstraint::Conversion {
                    resource: r,
                    input,
                    maximum_input,
                    ..
                } if r == resource && input == asset => Some(maximum_input.integer()),
                EffectConstraint::Debit {
                    resource: r,
                    asset: a,
                    maximum_debit,
                } if r == resource && a == asset => Some(maximum_debit.integer()),
                EffectConstraint::Transfer {
                    from,
                    debit_asset,
                    maximum_debit,
                    ..
                } if from == resource && debit_asset == asset => Some(maximum_debit.integer()),
                _ => None,
            })
            .fold(BigUint::zero(), |total, maximum| total + maximum);
        if reserved < maximum_debit.integer() + principal {
            return Err(GoFinError::new(
                "ACCOUNT_FUNDING_BUDGET_UNBACKED",
                "budget",
                "The account funding allowance must be reserved separately from principal in the same wallet and asset.",
                "Prepare the operation with the full principal and account funding reservation.",
            ));
        }
    }
    Ok(())
}

pub(super) fn expand(
    intent: &FinancialIntent,
    effects: &[FinancialEffect],
) -> Result<FinancialIntent> {
    validate(intent)?;
    let mut narrowed = intent.clone();
    narrowed
        .expected
        .retain(|constraint| !matches!(constraint, EffectConstraint::AccountFundingBudget { .. }));
    for constraint in &intent.expected {
        let EffectConstraint::AccountFundingBudget {
            resource,
            asset,
            maximum_debit,
        } = constraint
        else {
            continue;
        };
        let mut positions = BTreeSet::new();
        let mut accounts = Vec::new();
        let mut funded = BigUint::zero();
        let mut refunded = BigUint::zero();
        for effect in effects {
            let FinancialEffect::PositionChange {
                resource: actual_resource,
                position,
                asset: actual_asset,
                delta,
                scaled_receipt,
                anchor: Some(anchor),
            } = effect
            else {
                continue;
            };
            if actual_resource != resource
                || actual_asset != asset
                || anchor.position_kind != PositionKind::RefundableAccountFunding
            {
                continue;
            }
            let amount = delta.magnitude();
            if !position
                .strip_prefix("account-funding:")
                .is_some_and(|account| !account.trim().is_empty() && account.trim() == account)
                || !positions.insert(position)
                || delta.is_zero()
                || scaled_receipt.is_some()
                || anchor.scaled_receipt.is_some()
                || anchor.resource != *resource
                || anchor.position != *position
                || anchor.principal.asset != *asset
                || if delta.is_negative() {
                    !anchor.principal.atoms.is_zero()
                } else {
                    anchor.principal.atoms != amount
                }
            {
                return Err(account_funding_effect_mismatch());
            }
            let cash = AccountingLocation::Cash {
                resource: resource.clone(),
            };
            let position_location = AccountingLocation::Position {
                resource: resource.clone(),
                position: position.clone(),
                position_kind: PositionKind::RefundableAccountFunding,
            };
            let (from, to) = if delta.is_negative() {
                (position_location, cash)
            } else {
                (cash, position_location)
            };
            // Exactly one link proves this precise receipt movement. Wrong
            // direction, asset, resource, amount or duplicate links cannot
            // obtain authority from the budget.
            if effects
                .iter()
                .filter(|other| {
                    matches!(other, FinancialEffect::OriginMovement {
                        from: f, to: t, debit, credit,
                    } if *f == from && *t == to
                        && debit.asset == *asset && credit.asset == *asset
                        && debit.atoms == amount && credit.atoms == amount)
                })
                .count()
                != 1
            {
                return Err(account_funding_effect_mismatch());
            }
            if delta.is_negative() {
                refunded += amount.integer();
                narrowed.expected.push(EffectConstraint::PositionOfKind {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: asset.clone(),
                    position_kind: PositionKind::RefundableAccountFunding,
                    direction: Direction::Decrease,
                    maximum_change: amount.clone(),
                });
                narrowed.expected.push(EffectConstraint::OriginMovement {
                    from,
                    to,
                    debit_asset: asset.clone(),
                    credit_asset: asset.clone(),
                    maximum_debit: amount.clone(),
                    minimum_credit: amount,
                });
            } else {
                funded += amount.integer();
                accounts.push(AccountFundingLimit {
                    position: position.clone(),
                    maximum_amount: amount,
                });
            }
        }
        if funded > maximum_debit.integer() {
            return Err(account_funding_effect_mismatch());
        }
        let mut cash_debits = BigUint::zero();
        let mut cash_credits = BigUint::zero();
        for effect in effects {
            match effect {
                FinancialEffect::CashDebit {
                    resource: r,
                    amount,
                } if r == resource && amount.asset == *asset => {
                    if amount.atoms.is_zero() {
                        return Err(account_funding_effect_mismatch());
                    }
                    cash_debits += amount.atoms.integer();
                }
                FinancialEffect::UsableCredit { to, amount }
                    if to == resource && amount.asset == *asset =>
                {
                    if amount.atoms.is_zero() {
                        return Err(account_funding_effect_mismatch());
                    }
                    cash_credits += amount.atoms.integer();
                    narrowed.expected.push(EffectConstraint::Credit {
                        to: resource.clone(),
                        asset: asset.clone(),
                        minimum_credit: amount.atoms.clone(),
                    });
                }
                _ => {}
            }
        }
        // These cash legs represent account funding only. Swap principal is
        // a separate Conversion effect and costs retain their own effect.
        // Refunds never net against newly funded principal or increase its cap.
        if cash_debits != funded || cash_credits != refunded {
            return Err(account_funding_effect_mismatch());
        }
        if !accounts.is_empty() {
            narrowed
                .expected
                .push(EffectConstraint::OptionalAccountFunding {
                    resource: resource.clone(),
                    asset: asset.clone(),
                    accounts,
                });
        }
    }
    Ok(narrowed)
}

#[cfg(test)]
mod tests;
