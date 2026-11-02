use super::*;
use gofin_domain::financial_plans::{
    self, FinancialPlanAuthorizationRecord, FinancialPlanAuthorizationRequest,
    FinancialPlanAuthorizationStatus, FinancialPlanFundingIdentity, FinancialPlanProceeds,
    FinancialPlanReview, FinancialPlanRevision, FinancialPlanStatus, FinancialPlanStepPermission,
    FinancialPlanStepProgress, FinancialPlanStepProgressState, FinancialPlanUsage,
};

/// Build the native review of the caller's plan narrative and broker-owned scope.
pub fn financial_plan_approval_review(
    plan: &financial_plans::FinancialPlanSpec,
    valid_for_seconds: u64,
) -> gofin_domain::approval_review::ApprovalReview {
    gofin_domain::approval_review::ApprovalReview {
        version: 2,
        terms: vec![],
        advisories: vec![],
        action: gofin_domain::approval_review::ApprovalAction::ExecutePlan,
        service: "GoFin".into(),
        market: None,
        input: None,
        output: None,
        other_details: plan
            .steps
            .iter()
            .any(|step| {
                step.operation == "coinbase.withdraw"
                    && step.input.get("feeMode").and_then(Value::as_str) == Some("venue_calculated")
            })
            .then(|| "Coinbase fee uncapped; reserve is not cap.".into()),
        description: Some(match &plan.summary {
            Some(summary) => format!("{}: {}", plan.name, summary),
            None => plan.name.clone(),
        }),
        step_count: Some(plan.steps.len() as u64),
        valid_for_seconds: Some(valid_for_seconds),
    }
}

#[path = "non_cash_plan_sources.rs"]
mod non_cash_plan_sources;
use non_cash_plan_sources::{exact_non_cash_receipt_scope, substantive_plan_source_effects};

impl Engine {
    /// Prepare balance-independent adapter permissions and return the canonical
    /// review hash. Preview is read-only and never creates an approval request.
    pub async fn preview_financial_plan(
        &self,
        _request: FinancialPlanAuthorizationRequest,
    ) -> Result<FinancialPlanReview> {
        Err(retired_execution_interface())
    }

    /// Obtain one fresh approval for a named plan revision. Request IDs are
    /// idempotent and revision rows remain immutable after approval.
    pub async fn authorize_financial_plan(
        &self,
        _request: FinancialPlanAuthorizationRequest,
    ) -> Result<FinancialPlanStatus> {
        Err(retired_execution_interface())
    }

    /// Inspect protected plan bindings without observing chains or granting authority.
    pub fn financial_plan_rpc_read_scope(&self, plan_id: &str) -> Result<bool> {
        let record = self.load_financial_plan(plan_id)?;
        // Only unresolved submissions collect RPC proof; a future wallet step
        // alone must not remove a status command's existing response budget.
        // An earlier revision can still own an unresolved original submission.
        for attempt in &record.attempts {
            if self.status_rpc_read_scope(&attempt.attempt_id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn financial_plan_status(&self, plan_id: &str) -> Result<FinancialPlanStatus> {
        let record = self.load_financial_plan(plan_id)?;
        self.build_plan_status(&record, &record.revision)
    }

    pub async fn revoke_financial_plan(&self, plan_id: &str) -> Result<FinancialPlanStatus> {
        let mut record = self.load_financial_plan(plan_id)?;
        if record.revision.status == FinancialPlanAuthorizationStatus::Active {
            let expected = record.clone();
            record.revision.status = FinancialPlanAuthorizationStatus::Revoked;
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .compare_update_financial_plan(
                    plan_id,
                    &to_value(&expected)?,
                    &to_value(&record)?,
                    &self.clock.now().to_rfc3339(),
                )
                .map_err(map_store)?;
        }
        let revision = record.revision.clone();
        self.build_plan_status(&record, &revision)
    }

    pub async fn preview_financial_plan_step(
        &self,
        _plan_id: &str,
        _step_id: &str,
        _operation: &str,
        _input: &Value,
    ) -> Result<PreparedOperation> {
        Err(retired_execution_interface())
    }

    /// Execute one explicitly submitted plan step. Each call performs fresh
    /// preparation and reservation; the plan itself never triggers execution.
    pub async fn execute_financial_plan_step(
        &self,
        _plan_id: &str,
        _step_id: &str,
        _request: OperationRequest,
    ) -> Result<OperationRecord> {
        Err(retired_execution_interface())
    }

    pub async fn execute_financial_plan_step_replacing(
        &self,
        _plan_id: &str,
        _step_id: &str,
        _previous_attempt_id: &str,
        _request: OperationRequest,
    ) -> Result<OperationRecord> {
        Err(retired_execution_interface())
    }

    fn validate_next_plan_reservation(
        &self,
        spec: &financial_plans::FinancialPlanSpec,
        completed_steps: &[String],
    ) -> Result<()> {
        let Some(step) = spec
            .steps
            .iter()
            .find(|step| !completed_steps.contains(&step.step_id))
        else {
            return Ok(());
        };
        let (adapter, kind) = self.resolve(&step.operation)?;
        // A preceding read may reconcile state. Future writes retain dispatch
        // checks; do not reject the whole plan based on their projected funding.
        if kind != OperationKind::Write {
            return Ok(());
        }
        let Some(resources) = adapter.plan_exclusive_resources(&step.operation, &step.input)?
        else {
            return Ok(());
        };
        let requested = resources.into_iter().collect::<BTreeSet<_>>();
        if requested.is_empty() {
            return Ok(());
        }
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        for value in store.list_records().map_err(map_store)? {
            let record: OperationRecord = from_value(value)?;
            if record.status.terminal() {
                continue;
            }
            let conflicting =
                ordinary_policy::effective_conflict_resources(&store, &record).map(|held| {
                    requested
                        .intersection(&held)
                        .cloned()
                        .collect::<BTreeSet<_>>()
                });
            if conflicting.as_ref().is_ok_and(BTreeSet::is_empty) {
                continue;
            }
            let uninspectable = conflicting.is_err();
            return Err(plan_error(
                "RESOURCE_RESERVED",
                if uninspectable {
                    "An unresolved operation has no inspectable reservation; the next plan write cannot safely acquire its resources."
                } else {
                    "An unresolved operation reserves a resource required by the next plan write."
                },
                &format!("Reconcile original operation {} with `gofin status` before reviewing this plan again. Preserve its identity and do not replay the write or automatically clear its lock. If recovery remains inconclusive, inspect `gofin reservations preview` and request a separate human Touch ID approval with `gofin reservations release` for its exact eligible account locks.", record.id),
            ).with_details(json!({
                "step_id":step.step_id,
                "operation":step.operation,
                "before_plan_approval":true,
                "blocking_operation_id":record.id,
                "blocking_operation":record.operation,
                "blocking_status":record.status,
                "conflicting_resources":conflicting.unwrap_or_default(),
                "reservation_uninspectable":uninspectable,
            })));
        }
        Ok(())
    }

    // Keep the candidate's raw and broker-bound inputs explicit at this trust
    // boundary; grouping them would obscure which values are caller supplied.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn load_financial_plan(
        &self,
        plan_id: &str,
    ) -> Result<FinancialPlanAuthorizationRecord> {
        let value = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .financial_plan(plan_id)
            .map_err(map_store)?
            .ok_or_else(|| {
                plan_error(
                    "PLAN_NOT_FOUND",
                    "Financial plan does not exist.",
                    "Use the original plan ID.",
                )
            })?;
        from_value(value)
    }

    fn recovery_step_for_active_attempt(
        &self,
        record: &FinancialPlanAuthorizationRecord,
        progress: &[FinancialPlanStepProgress],
    ) -> Result<Option<String>> {
        let Some(active_id) = record.active_attempt_id.as_deref() else {
            return Ok(None);
        };
        let parent = record
            .attempts
            .iter()
            .find(|attempt| attempt.attempt_id == active_id)
            .ok_or_else(|| recovery_parent_invalid("The active plan attempt is not linked."))?;
        let parent_operation = self.load_record(active_id)?;
        if !matches!(
            parent_operation.status,
            Status::Submitted | Status::Pending | Status::Unknown
        ) || parent_operation.submission.is_none()
        {
            return Ok(None);
        }
        let parent_position = record
            .revision
            .spec
            .steps
            .iter()
            .position(|step| step.step_id == parent.step_id)
            .ok_or_else(|| recovery_parent_invalid("The active parent step is missing."))?;
        if progress
            .iter()
            .take(parent_position)
            .any(|step| step.state != FinancialPlanStepProgressState::Completed)
        {
            return Ok(None);
        }
        for (index, step) in record
            .revision
            .spec
            .steps
            .iter()
            .enumerate()
            .skip(parent_position + 1)
        {
            let permission = permission_for_step(&record.revision, &step.step_id)?;
            let exact_parent = permission
                .permission
                .as_ref()
                .and_then(|permission| permission.recovery_parent.as_ref())
                .is_some_and(|recovery| recovery.step_id == parent.step_id);
            if !exact_parent {
                // A normal financial step between an unresolved write and
                // cleanup must never be skipped merely because a later step
                // also names that write as its recovery parent.
                return Ok(None);
            }
            let state = progress
                .iter()
                .find(|state| state.step_id == step.step_id)
                .ok_or_else(|| internal("plan progress omitted a recovery step"))?;
            if state.state == FinancialPlanStepProgressState::NotStarted
                && !record
                    .attempts
                    .iter()
                    .any(|attempt| attempt.step_id == step.step_id)
            {
                // Do not skip an incomplete ordinary step between the parent
                // and its cleanup action. The recovery relation is an
                // explicit exception only for this pending parent.
                if progress
                    .iter()
                    .take(index)
                    .skip(parent_position + 1)
                    .any(|state| state.state != FinancialPlanStepProgressState::Completed)
                {
                    return Ok(None);
                }
                return Ok(Some(step.step_id.clone()));
            }
        }
        Ok(None)
    }

    fn build_plan_status(
        &self,
        record: &FinancialPlanAuthorizationRecord,
        revision: &FinancialPlanRevision,
    ) -> Result<FinancialPlanStatus> {
        let consumed_obligations_by_scope = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            cumulative_obligation_usage_by_scope(record, revision, &store)?
        };
        let mut steps = Vec::new();
        let mut reserved_debits = record.usage.consumed_debits.clone();
        let mut reserved_costs = record.usage.consumed_costs.clone();
        for reservation in &record.usage.reservations {
            reserved_debits = add_amounts(&reserved_debits, &reservation.debits)?;
            reserved_costs = add_cost_bounds(&reserved_costs, &reservation.costs)?;
        }
        let remaining_debits = remaining_debits(
            &revision.spec.aggregate_bounds.maximum_debits,
            Some(&reserved_debits),
        )?;
        let remaining_costs = remaining_costs(
            &revision.spec.aggregate_bounds.maximum_costs,
            Some(&reserved_costs),
        )?;
        let bridge_transfers = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .bridge_transfers()
            .map_err(map_store)?
            .into_iter()
            .filter(|transfer| {
                transfer
                    .plan_scope
                    .as_ref()
                    .is_some_and(|scope| scope.plan_id == revision.plan_id)
            })
            .collect::<Vec<_>>();
        for step in &revision.spec.steps {
            let attempts = record
                .attempts
                .iter()
                .filter(|attempt| attempt.step_id == step.step_id)
                .collect::<Vec<_>>();
            let mut step_effects = Vec::new();
            let mut attempt_summaries = Vec::new();
            let mut completed_bridge_deposit_attempts = Vec::new();
            let mut human_resolutions = Vec::new();
            for attempt in &attempts {
                let operation = self.load_record(&attempt.attempt_id)?;
                if let Some(resolution) = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .operation_resolution(&operation.id)
                    .map_err(map_store)?
                {
                    human_resolutions.push(resolution);
                }
                if let Some(verified) = &operation.effects {
                    step_effects.extend(verified.effects.clone());
                }
                if operation.status == Status::Completed
                    && matches!(
                        operation.operation.as_str(),
                        "bridge.transfer" | "bridge.execute"
                    )
                {
                    completed_bridge_deposit_attempts.extend(
                        bridge_transfers
                            .iter()
                            .filter(|transfer| {
                                plan_bridge_deposit_link(transfer, &revision.plan_id, &step.step_id)
                                    .is_some_and(|link| {
                                        link.operation_id.as_deref() == Some(operation.id.as_str())
                                            && link.state == gofin_domain::BridgeStepState::Settled
                                    })
                            })
                            .map(|transfer| transfer.id.clone()),
                    );
                }
                attempt_summaries.push(financial_plans::FinancialPlanAttemptSummary {
                    attempt_id: operation.id.clone(),
                    request_id: operation.request_id.clone(),
                    logical_request_id: attempt.logical_request_id.clone(),
                    step_id: step.step_id.clone(),
                    status: operation.status.clone(),
                    authorization_source: operation
                        .approval
                        .as_ref()
                        .and_then(|audit| audit.source.clone()),
                    yolo_provenance: operation
                        .approval
                        .as_ref()
                        .and_then(|audit| audit.yolo_provenance.clone()),
                    submission: operation.submission.clone(),
                    error: operation.error.clone(),
                    result: attempt.result.clone(),
                });
            }
            // External destination settlement costs are persisted separately
            // from the immutable source operation. Include their idempotent
            // accounting rows in this logical step's completion and usage view.
            {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                for transfer in &bridge_transfers {
                    let Some(link) =
                        plan_bridge_deposit_link(transfer, &revision.plan_id, &step.step_id)
                    else {
                        continue;
                    };
                    if link.state != gofin_domain::BridgeStepState::Settled
                        || !attempts.iter().any(|attempt| {
                            link.operation_id.as_deref() == Some(attempt.attempt_id.as_str())
                        })
                    {
                        continue;
                    }
                    for proof in &transfer.settlement_proofs {
                        if proof.outcome != gofin_domain::BridgeSettlementOutcome::Destination {
                            continue;
                        }
                        if let Some((_, raw_costs)) = store
                            .financial_plan_bridge_settlement_costs(
                                &revision.plan_id,
                                &transfer.id,
                                &proof.proof_digest,
                            )
                            .map_err(map_store)?
                        {
                            let costs: Vec<CostBound> = from_value(raw_costs)?;
                            step_effects.extend(costs.into_iter().map(|cost| {
                                FinancialEffect::Cost {
                                    location: cost.location,
                                    amount: cost.amount,
                                    category: "withheld_from_output".into(),
                                    treatment: cost.treatment,
                                }
                            }));
                        }
                    }
                }
            }
            let operation_succeeded = attempt_summaries
                .iter()
                .any(|attempt| attempt.status == Status::Completed);
            let mut completion_effects = step_effects.clone();
            let has_committed_destination_proof =
                completed_bridge_deposit_attempts.iter().any(|transfer_id| {
                    bridge_transfers.iter().any(|transfer| {
                        transfer.id == *transfer_id
                            && committed_plan_bridge_destination_proof(
                                transfer,
                                &revision.plan_id,
                                &step.step_id,
                                &attempt_summaries,
                            )
                    })
                });
            if has_committed_destination_proof {
                include_committed_plan_receipt(
                    &step.completion,
                    &mut completion_effects,
                    &record.usage,
                    &step.step_id,
                )?;
            }
            let completed = human_resolutions
                .iter()
                .any(|resolution| resolution["outcome"] == "succeeded")
                || completion_met(&step.completion, &completion_effects, operation_succeeded)?;
            let unresolved = record
                .active_attempt_id
                .as_ref()
                .is_some_and(|id| attempts.iter().any(|attempt| &attempt.attempt_id == id));
            let (consumed_debits, consumed_costs, verified_receipts) =
                summarize_plan_effects(&step_effects, &step.step_id, &record.usage)?;
            let mut blockers = Vec::new();
            if unresolved {
                blockers.push("A previous attempt must be reconciled.".into());
            }
            let awaiting_bridge_destination = !completed
                && step.completion.minimum_net_receipt.is_some()
                && completed_bridge_deposit_attempts.iter().any(|transfer_id| {
                    bridge_transfers.iter().any(|transfer| {
                        transfer.id == *transfer_id
                            && plan_bridge_deposit_link(transfer, &revision.plan_id, &step.step_id)
                                .is_some()
                            && transfer.state != gofin_domain::BridgeTransferState::Completed
                    })
                });
            if awaiting_bridge_destination {
                let recovery_required = bridge_transfers.iter().any(|transfer| {
                    completed_bridge_deposit_attempts
                        .iter()
                        .any(|transfer_id| transfer_id == &transfer.id)
                        && matches!(
                            transfer.state,
                            gofin_domain::BridgeTransferState::Refunded
                                | gofin_domain::BridgeTransferState::Failed
                                | gofin_domain::BridgeTransferState::NeedsAttention
                        )
                });
                blockers.push(if recovery_required {
                    "The bridge source write is complete; its parent has a refund or failure outcome, so the destination target requires recovery instead of resubmission.".into()
                } else {
                    "The bridge source write is complete; wait for its verified destination receipt instead of submitting it again.".into()
                });
            }
            let permission = permission_for_step(revision, &step.step_id)?;
            let consumed_obligations = permission
                .permission
                .as_ref()
                .and_then(|adapter| adapter.cumulative_scope.as_ref())
                .and_then(|scope| consumed_obligations_by_scope.get(&scope.scope_id))
                .cloned()
                .unwrap_or_default();
            let adapter_non_cash_source = step
                .completion
                .minimum_net_receipt
                .as_ref()
                .zip(permission.permission.as_ref())
                .is_some_and(|(target, permission)| {
                    self.resolve(&step.operation).is_ok_and(|(adapter, _)| {
                        adapter.plan_has_finite_non_cash_source(&step.operation, permission, target)
                    })
                });
            let exhausted = !completed
                && !unresolved
                && step_authority_cannot_complete(
                    step,
                    permission,
                    &consumed_debits,
                    &remaining_debits,
                    &consumed_obligations,
                    &step_effects,
                    adapter_non_cash_source,
                )?;
            if exhausted {
                blockers.push(if (exact_non_cash_receipt_scope(step, permission) || adapter_non_cash_source)
                    && substantive_plan_source_effects(&step_effects)?
                {
                    "This noncash receipt step already used its finite source attempt; a remaining receipt requires fresh authority for a separate step or plan.".into()
                } else {
                    "Remaining step authority cannot satisfy its completion target.".into()
                });
            }
            let state = if completed {
                FinancialPlanStepProgressState::Completed
            } else if exhausted {
                FinancialPlanStepProgressState::Blocked
            } else if unresolved || !attempts.is_empty() {
                FinancialPlanStepProgressState::InProgress
            } else {
                FinancialPlanStepProgressState::NotStarted
            };
            steps.push(FinancialPlanStepProgress {
                step_id: step.step_id.clone(),
                state,
                attempts: attempt_summaries,
                consumed_debits,
                consumed_costs,
                verified_receipts,
                blockers,
                human_resolutions,
            });
        }
        let mut blockers = Vec::new();
        if record.active_attempt_id.is_some() {
            blockers.push("A plan attempt is unresolved.".into());
        }
        let authorization_active = revision.status == FinancialPlanAuthorizationStatus::Active
            && parse_time(&revision.expires_at)? > self.clock.now();
        let pending_bridge_settlements = bridge_transfers
            .iter()
            .filter(|transfer| {
                !matches!(
                    transfer.state,
                    gofin_domain::BridgeTransferState::Completed
                        | gofin_domain::BridgeTransferState::Refunded
                        | gofin_domain::BridgeTransferState::Failed
                )
            })
            .count();
        if pending_bridge_settlements > 0 {
            blockers.push(format!(
                "{pending_bridge_settlements} plan-linked bridge transfer(s) still need verified destination settlement."
            ));
        }
        let recovery_bridge_count = bridge_transfers
            .iter()
            .filter(|transfer| {
                matches!(
                    transfer.state,
                    gofin_domain::BridgeTransferState::Refunded
                        | gofin_domain::BridgeTransferState::Failed
                        | gofin_domain::BridgeTransferState::NeedsAttention
                )
            })
            .count();
        if recovery_bridge_count > 0 {
            blockers.push(format!(
                "{recovery_bridge_count} plan-linked bridge transfer(s) require recovery; their principal is not successful destination proceeds."
            ));
        }
        let mut next = if record.active_attempt_id.is_some() {
            self.recovery_step_for_active_attempt(record, &steps)?
        } else {
            if let Some(step_id) = select_next_eligible_step(&steps) {
                let permission = permission_for_step(revision, &step_id)?;
                if permission
                    .permission
                    .as_ref()
                    .and_then(|permission| permission.recovery_parent.as_ref())
                    .is_none()
                {
                    Some(step_id)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if authorization_active && record.active_attempt_id.is_none() && next.is_some() {
            let completed = steps
                .iter()
                .filter(|step| step.state == FinancialPlanStepProgressState::Completed)
                .map(|step| step.step_id.clone())
                .collect::<Vec<_>>();
            if let Err(error) = self.validate_next_plan_reservation(&revision.spec, &completed) {
                if error.code != "RESOURCE_RESERVED" {
                    return Err(error);
                }
                let message = format!("{} {}", error.message, error.next_action);
                if let Some(step) = steps
                    .iter_mut()
                    .find(|step| Some(&step.step_id) == next.as_ref())
                {
                    step.state = FinancialPlanStepProgressState::Blocked;
                    step.blockers.push(message.clone());
                }
                blockers.push(message);
                next = None;
            }
        }
        // Expiry stops dispatching unfinished work. It does not invalidate
        // source operations that already settled or block proof-only bridge
        // reconciliation after all writable steps are done.
        if !authorization_active && next.is_some() {
            blockers.push("Plan authorization is expired, revoked, or superseded.".into());
        }
        let at = self.clock.now();
        let remaining_seconds = parse_time(&revision.expires_at)?
            .signed_duration_since(at)
            .num_seconds()
            .max(0) as u64;
        let required_window = next
            .as_ref()
            .and_then(|id| revision.spec.steps.iter().find(|step| &step.step_id == id))
            .and_then(|step| step.action_deadline_seconds);
        let authorization_timing = Some(gofin_domain::plan_timing::PlanAuthorizationTiming {
            observed_at: at.to_rfc3339(),
            expires_at: revision.expires_at.clone(),
            remaining_seconds,
            required_action_window_seconds: required_window,
            can_prepare_next_action: authorization_active
                && next.is_some()
                && required_window.is_none_or(|window| window <= remaining_seconds),
        });
        Ok(FinancialPlanStatus {
            authorization_timing,
            plan_id: revision.plan_id.clone(),
            revision: revision.clone(),
            usage: record.usage.clone(),
            steps,
            remaining_debits,
            remaining_costs,
            next_eligible_step_id: if !authorization_active { None } else { next },
            blockers,
        })
    }
}

pub(super) fn add_cost_bounds(
    base: &[CostBound],
    additions: &[CostBound],
) -> Result<Vec<CostBound>> {
    let mut result = base.to_vec();
    for addition in additions {
        if let Some(item) = result.iter_mut().find(|item| {
            item.location == addition.location
                && item.amount.asset == addition.amount.asset
                && item.treatment == addition.treatment
        }) {
            item.amount.atoms = item.amount.atoms.checked_add(&addition.amount.atoms)?;
        } else {
            result.push(addition.clone());
        }
    }
    Ok(result)
}

fn completion_met(
    completion: &financial_plans::FinancialStepCompletion,
    effects: &[FinancialEffect],
    operation_succeeded: bool,
) -> Result<bool> {
    let projected = super::conversion_sequences::project_effects(effects)?;
    let effects = projected.as_slice();
    if completion.operation_succeeded && !operation_succeeded {
        return Ok(false);
    }
    let mut spent = BTreeMap::<String, BigUint>::new();
    let receipts = completion_receipt_totals(effects)?;
    for effect in effects {
        match effect {
            FinancialEffect::CashDebit { resource, amount } => {
                add_big_amount(&mut spent, resource, &amount.asset, amount.atoms.integer())?
            }
            FinancialEffect::Transfer {
                from,
                debit,
                to: _,
                credit: _,
            } => {
                add_big_amount(&mut spent, from, &debit.asset, debit.atoms.integer())?;
            }
            FinancialEffect::EscrowProgress {
                wallet_resource,
                input_asset,
                wallet_debit,
                ..
            } => add_big_amount(
                &mut spent,
                wallet_resource,
                input_asset,
                wallet_debit.integer(),
            )?,
            FinancialEffect::EscrowPositionProgress {
                wallet_resource,
                input_asset,
                wallet_debit,
                ..
            } => add_big_amount(
                &mut spent,
                wallet_resource,
                input_asset,
                wallet_debit.integer(),
            )?,
            FinancialEffect::Conversion {
                resource,
                spent: debit,
                received,
            } => {
                add_big_amount(&mut spent, resource, &debit.asset, debit.atoms.integer())?;
                let _ = received;
            }
            FinancialEffect::UsableCredit { .. } => {}
            FinancialEffect::Cost { .. } => {}
            _ => {}
        }
    }
    let spend_ok = match &completion.principal_spend {
        Some(target) => spent
            .get(&resource_asset_key(target)?)
            .is_some_and(|actual| actual >= &target.amount.atoms.integer()),
        None => true,
    };
    let receipt_ok = match &completion.minimum_net_receipt {
        Some(target) => receipts
            .get(&resource_asset_key(target)?)
            .is_some_and(|actual| actual >= &target.amount.atoms.integer()),
        None => true,
    };
    Ok(spend_ok && receipt_ok && (!completion.operation_succeeded || operation_succeeded))
}

fn completion_receipt_totals(effects: &[FinancialEffect]) -> Result<BTreeMap<String, BigUint>> {
    let projected = super::conversion_sequences::project_effects(effects)?;
    let effects = projected.as_slice();
    let mut receipts = BTreeMap::<String, BigUint>::new();
    for effect in effects {
        match effect {
            FinancialEffect::Transfer { to, credit, .. } => {
                add_big_amount(&mut receipts, to, &credit.asset, credit.atoms.integer())?;
            }
            FinancialEffect::Conversion {
                resource, received, ..
            } => {
                add_big_amount(
                    &mut receipts,
                    resource,
                    &received.asset,
                    received.atoms.integer(),
                )?;
            }
            FinancialEffect::UsableCredit { to, amount } => {
                add_big_amount(&mut receipts, to, &amount.asset, amount.atoms.integer())?;
            }
            FinancialEffect::EscrowProgress {
                wallet_resource,
                input_asset,
                output_asset,
                output_filled,
                input_refunded,
                ..
            } => {
                add_big_amount(
                    &mut receipts,
                    wallet_resource,
                    output_asset,
                    output_filled.integer(),
                )?;
                add_big_amount(
                    &mut receipts,
                    wallet_resource,
                    input_asset,
                    input_refunded.integer(),
                )?;
            }
            FinancialEffect::EscrowPositionProgress {
                wallet_resource,
                input_asset,
                input_refunded,
                ..
            } => {
                add_big_amount(
                    &mut receipts,
                    wallet_resource,
                    input_asset,
                    input_refunded.integer(),
                )?;
            }
            _ => {}
        }
    }
    for effect in effects {
        if let FinancialEffect::Cost {
            location,
            amount,
            treatment: gofin_domain::CostTreatment::DeductedFromCredit,
            ..
        } = effect
        {
            let key = resource_asset_key(&ResourceAmount {
                resource: location.resource().clone(),
                amount: Amount {
                    asset: amount.asset.clone(),
                    atoms: gofin_domain::Quantity::zero(),
                },
            })?;
            let actual = receipts.get_mut(&key).ok_or_else(|| {
                plan_error(
                    "PLAN_RECEIPT_FEE_UNFUNDED",
                    "A deducted output fee has no attributable receipt.",
                    "Reconcile the proof and preserve the original submission.",
                )
            })?;
            let fee = amount.atoms.integer();
            if *actual < fee {
                return Err(plan_error(
                    "PLAN_RECEIPT_FEE_UNFUNDED",
                    "A deducted output fee exceeds attributable receipt proceeds.",
                    "Reconcile the proof and preserve the original submission.",
                ));
            }
            *actual -= fee;
        }
    }
    Ok(receipts)
}

/// External bridge settlement is persisted as plan proceeds, not rewritten
/// into the source operation's immutable proof. For progress/completion only,
/// expose a synthetic typed receipt for the portion of committed proceeds not
/// already represented by the operation effects. Include proceeds later
/// consumed by dependent steps: receipt completion records what was received,
/// not what remains spendable.
fn include_committed_plan_receipt(
    completion: &financial_plans::FinancialStepCompletion,
    effects: &mut Vec<FinancialEffect>,
    usage: &FinancialPlanUsage,
    step_id: &str,
) -> Result<()> {
    let Some(target) = completion.minimum_net_receipt.as_ref() else {
        return Ok(());
    };
    let key = resource_asset_key(target)?;
    let observed = completion_receipt_totals(effects)?
        .get(&key)
        .cloned()
        .unwrap_or_default();
    let mut committed = BigUint::zero();
    for proceeds in usage.verified_proceeds.iter().filter(|proceeds| {
        proceeds.producing_step_id == step_id
            && proceeds.resource == target.resource
            && proceeds.asset == target.amount.asset
    }) {
        let available = proceeds.available.integer();
        if proceeds.consumed.integer() > available {
            return Err(plan_error(
                "PLAN_PROCEEDS_INVALID",
                "Consumed plan proceeds exceed the verified receipt total.",
                "Reconcile the durable financial journal before continuing.",
            ));
        }
        committed += available;
    }
    if committed > observed {
        effects.push(FinancialEffect::UsableCredit {
            to: target.resource.clone(),
            amount: Amount {
                asset: target.amount.asset.clone(),
                atoms: gofin_domain::Quantity::new((committed - observed).to_string())?,
            },
        });
    }
    Ok(())
}

fn summarize_plan_effects(
    effects: &[FinancialEffect],
    step_id: &str,
    usage: &FinancialPlanUsage,
) -> Result<(
    Vec<ResourceAmount>,
    Vec<CostBound>,
    Vec<FinancialPlanProceeds>,
)> {
    let projected = super::conversion_sequences::project_effects(effects)?;
    let effects = projected.as_slice();
    let mut debits = Vec::new();
    let mut costs = Vec::new();
    let mut credits = BTreeMap::<String, (ResourceId, gofin_domain::AssetId, BigUint)>::new();
    for effect in effects {
        match effect {
            FinancialEffect::CashDebit { resource, amount } => {
                add_resource_amount(&mut debits, resource, amount)?
            }
            FinancialEffect::Transfer {
                from,
                debit,
                to,
                credit,
            } => {
                add_resource_amount(&mut debits, from, debit)?;
                collect_amount(&mut credits, to, &credit.asset, credit.atoms.integer());
            }
            FinancialEffect::Conversion {
                resource,
                spent,
                received,
            } => {
                add_resource_amount(&mut debits, resource, spent)?;
                collect_amount(
                    &mut credits,
                    resource,
                    &received.asset,
                    received.atoms.integer(),
                );
            }
            FinancialEffect::EscrowProgress {
                wallet_resource,
                input_asset,
                output_asset,
                wallet_debit,
                output_filled,
                input_refunded,
                ..
            } => {
                add_resource_amount(
                    &mut debits,
                    wallet_resource,
                    &Amount {
                        asset: input_asset.clone(),
                        atoms: wallet_debit.clone(),
                    },
                )?;
                collect_amount(
                    &mut credits,
                    wallet_resource,
                    output_asset,
                    output_filled.integer(),
                );
                collect_amount(
                    &mut credits,
                    wallet_resource,
                    input_asset,
                    input_refunded.integer(),
                );
            }
            FinancialEffect::EscrowPositionProgress {
                wallet_resource,
                input_asset,
                wallet_debit,
                input_refunded,
                ..
            } => {
                add_resource_amount(
                    &mut debits,
                    wallet_resource,
                    &Amount {
                        asset: input_asset.clone(),
                        atoms: wallet_debit.clone(),
                    },
                )?;
                collect_amount(
                    &mut credits,
                    wallet_resource,
                    input_asset,
                    input_refunded.integer(),
                );
            }
            // OriginMovement is provenance linking already-counted typed
            // effects. It never contributes a second cash debit or receipt.
            FinancialEffect::UsableCredit { to, amount } => {
                collect_amount(&mut credits, to, &amount.asset, amount.atoms.integer())
            }
            FinancialEffect::Cost {
                location,
                amount,
                treatment,
                ..
            } => {
                costs = add_cost_bounds(
                    &costs,
                    &[CostBound {
                        location: location.clone(),
                        amount: amount.clone(),
                        treatment: treatment.clone(),
                    }],
                )?
            }
            _ => {}
        }
    }
    for cost in effects.iter().filter_map(|effect| match effect {
        FinancialEffect::Cost {
            location,
            amount,
            treatment: gofin_domain::CostTreatment::DeductedFromCredit,
            ..
        } => Some((location, amount)),
        _ => None,
    }) {
        let (location, amount) = cost;
        let key = resource_asset_key(&ResourceAmount {
            resource: location.resource().clone(),
            amount: Amount {
                asset: amount.asset.clone(),
                atoms: gofin_domain::Quantity::zero(),
            },
        })?;
        if let Some((_, _, total)) = credits.get_mut(&key) {
            let fee = amount.atoms.integer();
            if *total < fee {
                return Err(plan_error(
                    "PLAN_RECEIPT_FEE_UNFUNDED",
                    "A deducted output fee exceeds attributable receipt proceeds.",
                    "Reconcile the proof and preserve the original submission.",
                ));
            }
            *total -= fee;
        }
    }
    let receipts = usage
        .verified_proceeds
        .iter()
        .filter(|item| item.producing_step_id == step_id)
        .cloned()
        .collect();
    Ok((debits, costs, receipts))
}

fn add_big_amount(
    map: &mut BTreeMap<String, BigUint>,
    resource: &ResourceId,
    asset: &gofin_domain::AssetId,
    quantity: BigUint,
) -> Result<()> {
    let key = resource_asset_key(&ResourceAmount {
        resource: resource.clone(),
        amount: Amount {
            asset: asset.clone(),
            atoms: gofin_domain::Quantity::zero(),
        },
    })?;
    *map.entry(key).or_default() += quantity;
    Ok(())
}

pub(super) fn account_financial_plan_effects(
    plan: &mut FinancialPlanAuthorizationRecord,
    operation: &OperationRecord,
) -> Result<()> {
    let Some(verified) = operation.effects.as_ref() else {
        return Ok(());
    };
    let linkage = plan
        .attempts
        .iter()
        .find(|attempt| attempt.attempt_id == operation.id)
        .ok_or_else(|| {
            plan_error(
                "PLAN_ATTEMPT_MISSING",
                "Settled operation has no plan linkage.",
                "Preserve the operation record and reconcile its plan history.",
            )
        })?;
    let step_id = linkage.step_id.clone();
    let mut debit_map = BTreeMap::<String, (ResourceId, gofin_domain::AssetId, BigUint)>::new();
    let mut cost_map = BTreeMap::<
        String,
        (
            gofin_domain::AccountingLocation,
            gofin_domain::AssetId,
            gofin_domain::CostTreatment,
            BigUint,
        ),
    >::new();
    let mut credit_map = BTreeMap::<String, (ResourceId, gofin_domain::AssetId, BigUint)>::new();
    let projected = super::conversion_sequences::project_effects(&verified.effects)?;
    for effect in &projected {
        match effect {
            FinancialEffect::CashDebit { resource, amount } => collect_amount(
                &mut debit_map,
                resource,
                &amount.asset,
                amount.atoms.integer(),
            ),
            FinancialEffect::Transfer {
                from,
                debit,
                to,
                credit,
            } => {
                collect_amount(&mut debit_map, from, &debit.asset, debit.atoms.integer());
                collect_amount(&mut credit_map, to, &credit.asset, credit.atoms.integer());
            }
            FinancialEffect::Conversion {
                resource,
                spent,
                received,
            } => {
                collect_amount(
                    &mut debit_map,
                    resource,
                    &spent.asset,
                    spent.atoms.integer(),
                );
                collect_amount(
                    &mut credit_map,
                    resource,
                    &received.asset,
                    received.atoms.integer(),
                );
            }
            FinancialEffect::EscrowProgress {
                wallet_resource,
                input_asset,
                output_asset,
                wallet_debit,
                output_filled,
                input_refunded,
                ..
            } => {
                collect_amount(
                    &mut debit_map,
                    wallet_resource,
                    input_asset,
                    wallet_debit.integer(),
                );
                collect_amount(
                    &mut credit_map,
                    wallet_resource,
                    output_asset,
                    output_filled.integer(),
                );
                collect_amount(
                    &mut credit_map,
                    wallet_resource,
                    input_asset,
                    input_refunded.integer(),
                );
            }
            FinancialEffect::EscrowPositionProgress {
                wallet_resource,
                input_asset,
                wallet_debit,
                input_refunded,
                ..
            } => {
                collect_amount(
                    &mut debit_map,
                    wallet_resource,
                    input_asset,
                    wallet_debit.integer(),
                );
                collect_amount(
                    &mut credit_map,
                    wallet_resource,
                    input_asset,
                    input_refunded.integer(),
                );
            }
            // OriginMovement supplies provenance for cash, position, and
            // debt effects; it is not a second debit. The corresponding
            // typed balance effect above is the sole source of gross usage.
            FinancialEffect::OriginMovement { .. } => {}
            FinancialEffect::UsableCredit { to, amount } => {
                collect_amount(&mut credit_map, to, &amount.asset, amount.atoms.integer())
            }
            FinancialEffect::Cost {
                location,
                amount,
                treatment,
                ..
            } => {
                let key = serde_json::to_string(&(location, &amount.asset, treatment))
                    .map_err(|_| internal("plan cost key serialization failed"))?;
                let entry = cost_map.entry(key).or_insert((
                    location.clone(),
                    amount.asset.clone(),
                    treatment.clone(),
                    BigUint::zero(),
                ));
                entry.3 += amount.atoms.integer();
            }
            _ => {}
        }
    }
    for (location, asset, _, fee) in cost_map.values().filter(|(_, _, treatment, _)| {
        treatment == &gofin_domain::CostTreatment::DeductedFromCredit
    }) {
        let key = resource_asset_key(&ResourceAmount {
            resource: location.resource().clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: gofin_domain::Quantity::zero(),
            },
        })?;
        let entry = credit_map.get_mut(&key).ok_or_else(|| {
            plan_error(
                "PLAN_RECEIPT_FEE_UNFUNDED",
                "A deducted output fee has no attributable receipt.",
                "Keep the attempt unresolved until verified effects account for the fee.",
            )
        })?;
        if entry.2 < *fee {
            return Err(plan_error(
                "PLAN_RECEIPT_FEE_UNFUNDED",
                "A deducted output fee exceeds attributable receipt proceeds.",
                "Keep the attempt unresolved until verified effects account for the fee.",
            ));
        }
        entry.2 -= fee;
    }
    for (resource, asset, amount) in debit_map.into_values() {
        consume_plan_funding(plan, &step_id, &resource, &asset, &amount, false)?;
        let item = ResourceAmount {
            resource,
            amount: Amount {
                asset,
                atoms: gofin_domain::Quantity::new(amount.to_string())?,
            },
        };
        plan.usage.consumed_debits = add_amounts(&plan.usage.consumed_debits, &[item])?;
    }
    for (location, asset, _treatment, amount) in cost_map.values().filter(|(_, _, treatment, _)| {
        matches!(
            treatment,
            gofin_domain::CostTreatment::AdditionalDebit
                | gofin_domain::CostTreatment::VenueCalculatedAdditionalDebit
        )
    }) {
        consume_plan_funding(plan, &step_id, location.resource(), asset, amount, true)?;
    }
    for (location, asset, treatment, amount) in cost_map.into_values() {
        plan.usage.consumed_costs = add_cost_bounds(
            &plan.usage.consumed_costs,
            &[CostBound {
                location,
                amount: Amount {
                    asset,
                    atoms: gofin_domain::Quantity::new(amount.to_string())?,
                },
                treatment,
            }],
        )?;
    }
    for (resource, asset, amount) in credit_map
        .into_values()
        .filter(|(_, _, amount)| !amount.is_zero())
    {
        let possible = permission_credits(permission_for_step(&plan.revision, &step_id)?)
            .iter()
            .any(|identity| identity.resource == resource && identity.asset == asset);
        if possible {
            let identity = FinancialPlanFundingIdentity {
                resource: resource.clone(),
                asset: asset.clone(),
            };
            if let Some(proceeds) = plan.usage.verified_proceeds.iter_mut().find(|proceeds| {
                proceeds.producing_step_id == step_id
                    && proceeds.resource == resource
                    && proceeds.asset == asset
            }) {
                proceeds.available = proceeds
                    .available
                    .checked_add(&gofin_domain::Quantity::new(amount.to_string())?)?;
            } else {
                plan.usage.verified_proceeds.push(FinancialPlanProceeds {
                    producing_step_id: step_id.clone(),
                    resource: identity.resource,
                    asset: identity.asset,
                    available: gofin_domain::Quantity::new(amount.to_string())?,
                    consumed: gofin_domain::Quantity::zero(),
                });
            }
        }
    }
    Ok(())
}

pub(super) fn consume_plan_funding(
    plan: &mut FinancialPlanAuthorizationRecord,
    step_id: &str,
    resource: &ResourceId,
    asset: &gofin_domain::AssetId,
    amount: &BigUint,
    is_additional_cost: bool,
) -> Result<()> {
    let step = plan
        .revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| {
            plan_error(
                "PLAN_STEP_NOT_FOUND",
                "Settled attempt step is missing.",
                "Preserve the attempt history and reconcile its plan.",
            )
        })?;
    let dependencies = step
        .funding_dependencies
        .iter()
        .filter(|dependency| {
            &dependency.resource == resource
                && &dependency.asset == asset
                && (!is_additional_cost || dependency.includes_additional_costs())
        })
        .collect::<Vec<_>>();
    if dependencies.is_empty() {
        return Ok(());
    }
    let mut remaining = amount.clone();
    for dependency in dependencies {
        let matching = plan
            .usage
            .verified_proceeds
            .iter_mut()
            .chain(plan.usage.human_attested_proceeds.iter_mut())
            .filter(|proceeds| {
                proceeds.producing_step_id == dependency.producing_step_id
                    && &proceeds.resource == resource
                    && &proceeds.asset == asset
            });
        for proceeds in matching {
            let available_total = proceeds.available.integer();
            let consumed = proceeds.consumed.integer();
            if consumed > available_total {
                return Err(plan_error(
                    "PLAN_FUNDING_USAGE_INVALID",
                    "Persisted producer consumption exceeds its verified proceeds.",
                    "Preserve the plan history and repair the inconsistent funding ledger before continuing.",
                ));
            }
            let available = available_total - consumed;
            let used = std::cmp::min(available, remaining.clone());
            proceeds.consumed = proceeds
                .consumed
                .checked_add(&gofin_domain::Quantity::new(used.to_string())?)?;
            remaining -= used;
            if remaining.is_zero() {
                break;
            }
        }
        if remaining.is_zero() {
            break;
        }
    }
    if !remaining.is_zero() {
        return Err(plan_error(
            "PLAN_FUNDING_NOT_SETTLED",
            "Verified proceeds do not cover the dependent operation's settled debit.",
            "Keep the attempt unresolved and reconcile its declared funding source.",
        ));
    }
    Ok(())
}

fn collect_amount(
    map: &mut BTreeMap<String, (ResourceId, gofin_domain::AssetId, BigUint)>,
    resource: &ResourceId,
    asset: &gofin_domain::AssetId,
    amount: BigUint,
) {
    let key = serde_json::to_string(&(resource, asset)).unwrap_or_default();
    let entry = map
        .entry(key)
        .or_insert((resource.clone(), asset.clone(), BigUint::zero()));
    entry.2 += amount;
}

pub(super) fn plan_error(code: &str, message: &str, retry: &str) -> GoFinError {
    GoFinError::new(code, "financial_plan", message, retry)
}

fn select_next_eligible_step(steps: &[FinancialPlanStepProgress]) -> Option<String> {
    let first_incomplete = steps
        .iter()
        .find(|step| step.state != FinancialPlanStepProgressState::Completed)?;
    if first_incomplete.state == FinancialPlanStepProgressState::Blocked
        || first_incomplete.blockers.iter().any(|blocker| {
            blocker == "A previous attempt must be reconciled."
                || blocker.starts_with("The bridge source write is complete;")
        })
    {
        None
    } else {
        Some(first_incomplete.step_id.clone())
    }
}

fn recovery_parent_invalid(message: &str) -> GoFinError {
    plan_error(
        "PLAN_RECOVERY_PARENT_INVALID",
        message,
        "Review a cleanup step bound to exactly one earlier unresolved write in this plan.",
    )
}

fn plan_bridge_deposit_link<'a>(
    transfer: &'a gofin_domain::BridgeTransferRecord,
    plan_id: &str,
    step_id: &str,
) -> Option<&'a gofin_domain::BridgeStepLink> {
    if !transfer
        .plan_scope
        .as_ref()
        .is_some_and(|scope| scope.plan_id == plan_id)
    {
        return None;
    }
    transfer.steps.iter().find(|link| {
        matches!(
            link.kind,
            gofin_domain::BridgeStepKind::Deposit
                | gofin_domain::BridgeStepKind::Finalize
                | gofin_domain::BridgeStepKind::Mint
        ) && link.plan_authority.as_ref().is_some_and(|authority| {
            authority.plan_id == plan_id && authority.plan_step_id == step_id
        })
    })
}

fn committed_plan_bridge_destination_proof(
    transfer: &gofin_domain::BridgeTransferRecord,
    plan_id: &str,
    step_id: &str,
    attempts: &[financial_plans::FinancialPlanAttemptSummary],
) -> bool {
    transfer.state == gofin_domain::BridgeTransferState::Completed
        && plan_bridge_deposit_link(transfer, plan_id, step_id).is_some_and(|link| {
            link.state == gofin_domain::BridgeStepState::Settled
                && link.operation_id.as_ref().is_some_and(|operation_id| {
                    attempts.iter().any(|attempt| {
                        attempt.attempt_id == *operation_id && attempt.status == Status::Completed
                    })
                })
        })
        && transfer.settlement_proofs.iter().any(|proof| {
            proof.transfer_id == transfer.id
                && proof.outcome == gofin_domain::BridgeSettlementOutcome::Destination
        })
}

pub(super) fn permission_for_step<'a>(
    revision: &'a FinancialPlanRevision,
    step_id: &str,
) -> Result<&'a FinancialPlanStepPermission> {
    revision
        .permissions
        .iter()
        .find(|permission| permission.step_id == step_id)
        .ok_or_else(|| {
            plan_error(
                "PLAN_PERMISSION_MISSING",
                "The active revision has no permission for this step.",
                "Create a new complete plan revision.",
            )
        })
}

fn permission_debits(permission: &FinancialPlanStepPermission) -> &[gofin_domain::ResourceAmount] {
    if let Some(exact) = &permission.exact {
        &exact.scope.maximum_debits
    } else {
        permission
            .permission
            .as_ref()
            .map(|adapter| adapter.spec.maximum_debits.as_slice())
            .unwrap_or_default()
    }
}

fn permission_credits(permission: &FinancialPlanStepPermission) -> &[FinancialPlanFundingIdentity] {
    if let Some(exact) = &permission.exact {
        &exact.scope.possible_credits
    } else {
        permission
            .permission
            .as_ref()
            .map(|adapter| adapter.possible_credits.as_slice())
            .unwrap_or_default()
    }
}

pub(super) fn carry_action_deadline(
    original: &PreparedOperation,
    candidate: &mut PreparedOperation,
) -> Result<()> {
    if candidate.action_deadline.is_some() && candidate.action_deadline != original.action_deadline
    {
        return Err(plan_error(
            "PLAN_CANDIDATE_CHANGED",
            "The immutable action deadline resolution changed.",
            "Reconcile the original attempt; do not rewrite it.",
        ));
    }
    candidate
        .action_deadline
        .clone_from(&original.action_deadline);
    Ok(())
}

fn generated_attempt_input_value(
    plan_id: &str,
    revision_id: &str,
    step_id: &str,
    attempt_id: &str,
) -> Result<String> {
    let digest = binding_hash(&json!({
        "purpose": "financial_plan_attempt_input",
        "plan_id": plan_id,
        "revision_id": revision_id,
        "step_id": step_id,
        "attempt_id": attempt_id,
    }))?;
    // Binance's clientTranId/requestId fields are capped at 32 characters.
    // Keep a readable namespace while retaining 116 bits of deterministic
    // identity entropy for the plan/revision/step/attempt tuple.
    let suffix = digest.get(..29).ok_or_else(|| {
        plan_error(
            "PLAN_GENERATED_INPUT_FAILED",
            "The generated attempt identity digest is malformed.",
            "Retry with the same attempt ID or inspect the plan record.",
        )
    })?;
    Ok(format!("fp-{suffix}"))
}

pub(super) fn plan_attempt_input(
    revision: &FinancialPlanRevision,
    step_id: &str,
    raw_input: &Value,
    attempt_id: &str,
) -> Result<Value> {
    let step = revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| internal("plan step disappeared"))?;
    if permission_for_step(revision, step_id)?.exact.is_some() && raw_input != &step.input {
        return Err(plan_error(
            "PLAN_STEP_SCOPE_MISMATCH",
            "The concrete operation differs from the exact approved input template.",
            "Submit the exact input in the active plan revision.",
        ));
    }
    let permission = permission_for_step(revision, step_id)?;
    if permission.generated_input_fields.is_empty() {
        return Ok(raw_input.clone());
    }
    let generated = generated_attempt_input_value(
        &revision.plan_id,
        &revision.revision_id,
        step_id,
        attempt_id,
    )?;
    let mut execution_input = raw_input.clone();
    for pointer in &permission.generated_input_fields {
        if raw_input.pointer(pointer) != step.input.pointer(pointer) {
            return Err(plan_error(
                "PLAN_STEP_SCOPE_MISMATCH",
                "A broker-generated identity field differs from the approved input template.",
                "Keep the identity field from the plan template; GoFin assigns its concrete value per attempt.",
            ));
        }
        let slot = execution_input.pointer_mut(pointer).ok_or_else(|| {
            plan_error(
                "PLAN_GENERATED_INPUT_INVALID",
                "The approved input template lacks a broker-generated attempt field.",
                "Create a fresh plan revision with the complete operation input.",
            )
        })?;
        if !slot.is_string() {
            return Err(plan_error(
                "PLAN_GENERATED_INPUT_INVALID",
                "The broker-generated attempt field is not a string in the approved template.",
                "Create a fresh plan revision with the complete operation input.",
            ));
        }
        *slot = Value::String(generated.clone());
    }
    Ok(execution_input)
}

pub(super) fn validate_prepared_generated_input(
    generated_input_fields: &[String],
    execution_input: &Value,
    prepared_input: &Value,
) -> Result<()> {
    for pointer in generated_input_fields {
        if execution_input.pointer(pointer).is_none()
            || execution_input.pointer(pointer) != prepared_input.pointer(pointer)
        {
            return Err(plan_error(
                "PLAN_GENERATED_INPUT_MISMATCH",
                "The persisted prepared candidate does not contain its original broker-generated attempt identity.",
                "Keep the attempt unresolved and reconcile it using its original record.",
            ));
        }
    }
    Ok(())
}

fn reserved_obligations(intent: &gofin_domain::FinancialIntent) -> Vec<ResourceAmount> {
    intent
        .expected
        .iter()
        .filter_map(|effect| match effect {
            EffectConstraint::Debt {
                resource,
                asset,
                direction: Direction::Increase,
                maximum_change,
                ..
            } => Some(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: maximum_change.clone(),
                },
            }),
            _ => None,
        })
        .collect()
}

fn verified_obligations(effects: &[FinancialEffect]) -> Vec<ResourceAmount> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            FinancialEffect::DebtChange {
                resource,
                asset,
                delta,
                ..
            } if !delta.is_negative() && !delta.magnitude().is_zero() => Some(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: delta.magnitude(),
                },
            }),
            _ => None,
        })
        .collect()
}

fn step_authority_cannot_complete(
    step: &financial_plans::FinancialPlanStepSpec,
    permission: &FinancialPlanStepPermission,
    consumed_debits: &[ResourceAmount],
    aggregate_remaining_debits: &[ResourceAmount],
    consumed_obligations: &[ResourceAmount],
    prior_effects: &[FinancialEffect],
    adapter_non_cash_source: bool,
) -> Result<bool> {
    let remaining = remaining_debits(
        permission_debits(permission),
        Some(&consumed_debits.to_vec()),
    )?;
    if let Some(target) = &step.completion.principal_spend {
        let consumed = consumed_debits
            .iter()
            .filter(|item| {
                item.resource == target.resource && item.amount.asset == target.amount.asset
            })
            .fold(BigUint::zero(), |sum, item| {
                sum + item.amount.atoms.integer()
            });
        let cap_remaining = remaining
            .iter()
            .find(|item| {
                item.resource == target.resource && item.amount.asset == target.amount.asset
            })
            .map(|item| item.amount.atoms.integer())
            .unwrap_or_default();
        let aggregate_cap_remaining = aggregate_remaining_debits
            .iter()
            .find(|item| {
                item.resource == target.resource && item.amount.asset == target.amount.asset
            })
            .map(|item| item.amount.atoms.integer())
            .unwrap_or_default();
        let target_atoms = target.amount.atoms.integer();
        if step.completion.minimum_net_receipt.is_some() && consumed >= target_atoms {
            // Candidate validation caps further principal input at this target.
            return Ok(true);
        }
        if consumed < target_atoms
            && std::cmp::min(cap_remaining, aggregate_cap_remaining) < target_atoms - consumed
        {
            return Ok(true);
        }
    }
    if let Some(target) = step.completion.minimum_net_receipt.as_ref() {
        let any_principal_capacity = remaining.iter().any(|item| {
            !item.amount.atoms.is_zero()
                && aggregate_remaining_debits.iter().any(|aggregate| {
                    aggregate.resource == item.resource
                        && aggregate.amount.asset == item.amount.asset
                        && !aggregate.amount.atoms.is_zero()
                })
        });
        let receipt_capacity = permission
            .permission
            .as_ref()
            .and_then(|value| value.deferred_receipt.as_ref())
            .is_some_and(|capacity| {
                capacity.resource == target.resource
                    && capacity.amount.asset == target.amount.asset
                    && capacity.amount.atoms >= target.amount.atoms
            });
        let obligation_receipt_capacity =
            obligation_receipt_capacity(permission, target, consumed_obligations)?;
        let exact_non_cash = exact_non_cash_receipt_scope(step, permission);
        let adapter_non_cash = adapter_non_cash_source
            && step.completion.principal_spend.is_none()
            && permission.permission.as_ref().is_some_and(|scope| {
                scope
                    .possible_credits
                    .contains(&FinancialPlanFundingIdentity {
                        resource: target.resource.clone(),
                        asset: target.amount.asset.clone(),
                    })
            });
        // Position caps and receipt targets can have different units. The
        // adapter proves a finite source, not an exchange rate. Never turn an
        // underperforming source attempt into reusable principal authority,
        // including when the permission also reserves account funding.
        if adapter_non_cash && substantive_plan_source_effects(prior_effects)? {
            return Ok(true);
        }
        if !any_principal_capacity
            && ((exact_non_cash && substantive_plan_source_effects(prior_effects)?)
                || (!exact_non_cash
                    && !adapter_non_cash
                    && !receipt_capacity
                    && !obligation_receipt_capacity))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn obligation_receipt_capacity(
    permission: &FinancialPlanStepPermission,
    target: &ResourceAmount,
    consumed_obligations: &[ResourceAmount],
) -> Result<bool> {
    let Some(adapter) = permission.permission.as_ref() else {
        return Ok(false);
    };
    let Some(scope) = adapter.cumulative_scope.as_ref() else {
        return Ok(false);
    };
    let credit_is_scoped = adapter
        .possible_credits
        .iter()
        .any(|credit| credit.resource == target.resource && credit.asset == target.amount.asset);
    if !credit_is_scoped {
        return Ok(false);
    }
    let remaining = remaining_debits(
        &scope.maximum_obligations,
        Some(&consumed_obligations.to_vec()),
    )?;
    Ok(remaining.iter().any(|cap| {
        cap.resource == target.resource
            && cap.amount.asset == target.amount.asset
            && cap.amount.atoms >= target.amount.atoms
    }))
}

fn cumulative_obligation_usage_by_scope(
    record: &FinancialPlanAuthorizationRecord,
    revision: &FinancialPlanRevision,
    store: &Store,
) -> Result<BTreeMap<String, Vec<ResourceAmount>>> {
    let mut revisions = store
        .financial_plan_revisions(&revision.plan_id)
        .map_err(map_store)?
        .into_iter()
        .map(from_value::<FinancialPlanRevision>)
        .collect::<Result<Vec<_>>>()?;
    if !revisions
        .iter()
        .any(|candidate| candidate.revision_id == revision.revision_id)
    {
        revisions.push(revision.clone());
    }

    let mut usage = BTreeMap::<String, Vec<ResourceAmount>>::new();
    for attempt in &record.attempts {
        let Some(scope_id) = revisions
            .iter()
            .find(|candidate| candidate.revision_id == attempt.revision_id)
            .and_then(|candidate| {
                candidate
                    .permissions
                    .iter()
                    .find(|permission| permission.step_id == attempt.step_id)
            })
            .and_then(|permission| permission.permission.as_ref())
            .and_then(|permission| permission.cumulative_scope.as_ref())
            .map(|scope| scope.scope_id.clone())
        else {
            continue;
        };
        let Some(raw) = store.get_record(&attempt.attempt_id).map_err(map_store)? else {
            return Err(plan_error(
                "PLAN_ATTEMPT_MISSING",
                "A cumulative-scope attempt record is missing.",
                "Restore the original plan attempt before continuing.",
            ));
        };
        let operation: OperationRecord = from_value(raw)?;
        let obligations = if let Some(verified) = operation.effects {
            verified_obligations(&verified.effects)
        } else if record
            .usage
            .reservations
            .iter()
            .any(|reservation| reservation.attempt_id == attempt.attempt_id)
        {
            let Some(preview) = operation.preview.as_ref() else {
                return Err(internal("Reserved attempt lacks its durable preview."));
            };
            reserved_obligations(&preview.intent)
        } else {
            Vec::new()
        };
        let current = usage.entry(scope_id).or_default();
        *current = add_amounts(current, &obligations)?;
    }
    Ok(usage)
}
