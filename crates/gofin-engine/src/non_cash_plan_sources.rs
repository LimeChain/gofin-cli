//! Exact receipt steps may consume one finite noncash source, not reusable
//! principal authority inferred from possible output identities.
use super::*;

/// Destination transactions finish an already-funded durable bridge lifecycle.
/// Their receipt is not authority to consume an unrelated supply position.
pub(super) fn validate_exact_bridge_receipt_candidate(
    record: &FinancialPlanAuthorizationRecord,
    step: &financial_plans::FinancialPlanStepSpec,
    permission: &FinancialPlanStepPermission,
    prepared: &PreparedOperation,
    prior_effects: &[FinancialEffect],
    store: &Store,
) -> Result<bool> {
    if !exact_non_cash_receipt_scope(step, permission) {
        return Ok(false);
    }
    let Some(marker) = prepared
        .binding
        .product_get(gofin_domain::BRIDGE_STEP_METADATA_KEY)?
    else {
        return Ok(false);
    };
    let invalid = || {
        plan_error(
            "PLAN_NON_CASH_SOURCE_INVALID",
            "An exact bridge receipt requires the original funded destination child, frozen receipt target and an unused finite attempt.",
            "Reconcile the saved bridge transfer and preserve its original child, source proof and approved bounds.",
        )
    };
    if !prepared.intent.debits.is_empty()
        || !prepared.intent.expected.is_empty()
        || substantive_plan_source_effects(prior_effects)?
    {
        return Err(invalid());
    }
    let existing_operation = record.active_attempt_id.as_deref().filter(|id| {
        record.attempts.iter().any(|attempt| {
            attempt.attempt_id == *id
                && attempt.step_id == step.step_id
                && attempt.revision_id == record.revision.revision_id
        })
    });
    if let Some(id) = existing_operation {
        let operation: OperationRecord = from_value(
            store
                .get_record(id)
                .map_err(map_store)?
                .ok_or_else(invalid)?,
        )?;
        if operation.status.terminal()
            || operation.operation != prepared.operation
            || operation.submission.is_some()
            || operation.dispatch_evidence.is_some()
            || operation.effects.is_some()
            || operation
                .preview
                .as_ref()
                .map(|preview| {
                    preview
                        .binding
                        .product_get(gofin_domain::BRIDGE_STEP_METADATA_KEY)
                })
                .transpose()?
                .flatten()
                != Some(marker)
        {
            return Err(invalid());
        }
    }
    // This shared check is also used immediately before the atomic bridge
    // claim. It validates public input, durable child/parent ownership, frozen
    // bounds, settled dependencies and original source transit. Here it only
    // reads; an existing claim must belong to this same unsubmitted attempt.
    let request = OperationRequest {
        request_id: String::new(),
        operation: prepared.operation.clone(),
        input: prepared.input.clone(),
        run_id: None,
    };
    let Some(validated) = validate_plan_bridge_attempt_scope(
        &request,
        prepared,
        &record.revision.plan_id,
        &record.revision.revision_id,
        &step.step_id,
        existing_operation,
        store,
    )?
    else {
        return Err(invalid());
    };
    let original: gofin_domain::BridgeTransferRecord = from_value(validated.expected_transfer)?;
    let target = step
        .completion
        .minimum_net_receipt
        .as_ref()
        .ok_or_else(invalid)?;
    if !validated.kind.is_destination_stage()
        || original
            .plan_scope
            .as_ref()
            .is_none_or(|scope| scope.plan_id != record.revision.plan_id)
        || original.approved_bounds.is_none()
        || original.settlement_expectation.is_none()
        || !prepared.intent.locks.contains(&target.resource)
        || validated
            .updated_transfer
            .settlement_expectation
            .as_ref()
            .is_none_or(|expectation| {
                !expectation.destination.iter().any(|destination| {
                    destination.resource == target.resource
                        && destination.asset == target.amount.asset
                        && destination.minimum_atoms >= target.amount.atoms
                })
            })
    {
        return Err(invalid());
    }
    Ok(true)
}

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
        position_kind: gofin_domain::PositionKind::Supply,
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
