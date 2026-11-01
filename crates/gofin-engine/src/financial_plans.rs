use super::*;
use gofin_domain::financial_plans::{
    self, FinancialPlanAuthorizationRecord, FinancialPlanAuthorizationRequest,
    FinancialPlanAuthorizationStatus, FinancialPlanExactPermission, FinancialPlanFundingIdentity,
    FinancialPlanProceeds, FinancialPlanReview, FinancialPlanRevision, FinancialPlanStatus,
    FinancialPlanStepPermission, FinancialPlanStepProgress, FinancialPlanStepProgressState,
    FinancialPlanUsage,
};
use gofin_domain::product_names::MetadataValueExt;

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
use non_cash_plan_sources::{
    exact_non_cash_receipt_scope, substantive_plan_source_effects,
    validate_exact_non_cash_receipt_candidate,
};

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
    pub(super) fn validate_plan_candidate(
        &self,
        adapter: &dyn gofin_domain::Adapter,
        revision: &FinancialPlanRevision,
        step_id: &str,
        raw_input: &Value,
        expected_input: &Value,
        attempt_id: &str,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        let step = revision
            .spec
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .ok_or_else(|| {
                plan_error(
                    "PLAN_STEP_NOT_FOUND",
                    "Step does not belong to this plan.",
                    "Use a step from the active plan revision.",
                )
            })?;
        super::validate_authority_expiry_for_adapter(
            adapter,
            &step.operation,
            prepared,
            &revision.expires_at,
        )?;
        validate_action_deadline(adapter, revision, step_id, raw_input, prepared)?;
        let permission = permission_for_step(revision, step_id)?;
        if let Some(exact) = &permission.exact {
            validate_exact_plan_candidate(
                adapter,
                &step.operation,
                &step.input,
                exact,
                raw_input,
                prepared,
            )?;
        } else {
            let scope = permission.permission.as_ref().ok_or_else(|| {
                plan_error(
                    "PLAN_PERMISSION_MISSING",
                    "The active revision has no permission scope for this step.",
                    "Create a new complete plan revision.",
                )
            })?;
            adapter.validate_boundary_candidate(&scope.spec, prepared)?;
        }
        if prepared.input != *expected_input {
            return Err(plan_error(
                "PLAN_ATTEMPT_INPUT_MISMATCH",
                "The adapter prepared an input different from the engine-bound plan attempt.",
                "Keep the exact prepared attempt and reconcile it before any retry.",
            ));
        }
        validate_generated_plan_candidate(
            adapter,
            revision,
            step_id,
            raw_input,
            expected_input,
            attempt_id,
            prepared,
        )
    }

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

pub(super) fn validate_exact_plan_candidate(
    adapter: &dyn gofin_domain::Adapter,
    operation: &str,
    approved_input: &Value,
    exact: &FinancialPlanExactPermission,
    raw_input: &Value,
    prepared: &PreparedOperation,
) -> Result<()> {
    if raw_input != approved_input || prepared.operation != operation {
        return Err(plan_error(
            "PLAN_STEP_SCOPE_MISMATCH",
            "The concrete operation differs from the exact approved input.",
            "Submit the exact operation and input in the active plan revision.",
        ));
    }
    let definition = adapter
        .definitions()
        .into_iter()
        .find(|definition| definition.name == operation)
        .ok_or_else(|| internal("registry definition disappeared"))?;
    let seconds = prepared
        .action_deadline
        .as_ref()
        .map(|deadline| deadline.window_seconds);
    let schema = gofin_domain::plan_timing::plan_input_schema(&definition, raw_input, seconds)?;
    validate_plan_json_schema(&schema, raw_input)?;
    validate_exact_plan_identity(adapter, operation, exact)
}

fn validate_exact_plan_identity(
    adapter: &dyn gofin_domain::Adapter,
    operation: &str,
    exact: &FinancialPlanExactPermission,
) -> Result<()> {
    let definition = adapter
        .definitions()
        .into_iter()
        .find(|definition| definition.name == operation)
        .ok_or_else(|| internal("registry definition disappeared"))?;
    let definition_matches =
        financial_plans::exact_plan_definition_hash_matches(&definition, &exact.definition_hash)?;
    let connections_hash = stable_connection_hash(&adapter.authorization_identities())?;
    let connection_identity_matches = connections_hash == exact.connections_hash;
    if !definition_matches || !connection_identity_matches {
        return Err(plan_error(
            "PLAN_OPERATION_CONTRACT_CHANGED",
            "The registered operation contract or protected connection identity changed after approval.",
            "Inspect the mismatch flags, then review and authorize a fresh plan revision if the contract or protected identity materially changed.",
        ).with_details(json!({"definition_matches":definition_matches,"connection_identity_matches":connection_identity_matches})));
    }
    Ok(())
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

pub(super) fn inject_plan_marker(
    prepared: &mut PreparedOperation,
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    attempt_hash: String,
) -> Result<()> {
    let exact_input = permission_for_step(&record.revision, step_id)?
        .exact
        .is_some();
    let Some(binding) = prepared.binding.as_object_mut() else {
        return Err(plan_error(
            "PLAN_BINDING_INVALID",
            "Prepared execution binding must be an object.",
            "Use an operation with a canonical adapter binding.",
        ));
    };
    binding.insert(
        financial_plans::PLAN_METADATA_KEY.into(),
        json!({
            "version": 1,
            "plan_id": record.revision.plan_id,
            "revision_id": record.revision.revision_id,
            "step_id": step_id,
            "expires_at": record.revision.expires_at,
            "exact_input": exact_input,
            "approval_binding_hash": record.revision.approval_binding_hash,
            "attempt_binding_hash": attempt_hash,
        }),
    );
    Ok(())
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

fn valid_recovery_pointer(pointer: &str) -> bool {
    pointer.starts_with('/')
        && pointer.len() > 1
        && !pointer.split('/').skip(1).any(|part| {
            let mut chars = part.chars();
            while let Some(ch) = chars.next() {
                if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
                    return true;
                }
            }
            false
        })
}

fn set_json_pointer(root: &mut Value, pointer: &str, value: Value) -> Result<()> {
    if !valid_recovery_pointer(pointer) {
        return Err(recovery_parent_invalid(
            "Recovery input pointer is malformed.",
        ));
    }
    let mut parts = pointer
        .split('/')
        .skip(1)
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .peekable();
    let mut parent = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let object = parent.as_object_mut().ok_or_else(|| {
                recovery_parent_invalid("Recovery input pointer does not target an object field.")
            })?;
            object.insert(part, value);
            return Ok(());
        }
        parent = parent.get_mut(&part).ok_or_else(|| {
            recovery_parent_invalid(
                "Recovery input pointer parent is absent from the plan template.",
            )
        })?;
    }
    Err(recovery_parent_invalid("Recovery input pointer is empty."))
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

fn permission_costs(permission: &FinancialPlanStepPermission) -> &[gofin_domain::CostBound] {
    if let Some(exact) = &permission.exact {
        &exact.scope.maximum_costs
    } else {
        permission
            .permission
            .as_ref()
            .map(|adapter| adapter.spec.maximum_costs.as_slice())
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

pub(super) fn validate_action_deadline(
    adapter: &dyn Adapter,
    revision: &FinancialPlanRevision,
    step_id: &str,
    raw_input: &Value,
    prepared: &PreparedOperation,
) -> Result<()> {
    let step = revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| internal("step missing"))?;
    let definition = adapter
        .definitions()
        .into_iter()
        .find(|d| d.name == step.operation)
        .ok_or_else(|| internal("definition missing"))?;
    let policy = gofin_domain::plan_timing::relative_deadline_policy(
        &definition,
        step.action_deadline_seconds,
    )?;
    match (policy, &prepared.action_deadline) {
        (None, None) => Ok(()),
        (Some(policy), Some(resolved)) => {
            gofin_domain::plan_timing::plan_input_schema(
                &definition,
                raw_input,
                step.action_deadline_seconds,
            )?;
            let at = parse_time(&resolved.resolved_at)?;
            let expected = gofin_domain::plan_timing::resolve_action_deadline(
                &policy,
                step.action_deadline_seconds.expect("relative deadline"),
                at,
                &revision.expires_at,
            )?;
            if resolved != &expected
                || prepared.input.get(&policy.field) != Some(&json!(expected.deadline_unix_seconds))
            {
                return Err(plan_error(
                    "PLAN_CANDIDATE_CHANGED",
                    "The concrete deadline differs from its immutable resolution or approved window.",
                    "Reconcile the original attempt; use candidate replacement only while unsubmitted.",
                ));
            }
            Ok(())
        }
        _ => Err(plan_error(
            "PLAN_CANDIDATE_CHANGED",
            "The candidate lacks or changes its approved deadline policy.",
            "Preserve the original prepared candidate.",
        )),
    }
}

fn validate_plan_json_schema(schema: &Value, input: &Value) -> Result<()> {
    let validator = jsonschema::options().offline().build(schema).map_err(|_| {
        plan_error(
            "PLAN_SCHEMA_INVALID",
            "The registered operation input schema cannot be validated offline.",
            "Contact the integration maintainer to make its schema self-contained.",
        )
    })?;
    if validator.is_valid(input) {
        Ok(())
    } else {
        Err(plan_error(
            "PLAN_INPUT_INVALID",
            "The exact plan input does not satisfy the registered operation schema.",
            "Correct the input using the operation schema, then preview the plan again.",
        ))
    }
}

fn generated_attempt_input_fields(
    definition: &Definition,
    input: &Value,
    is_read: bool,
) -> Result<Vec<String>> {
    let Some(raw_fields) = definition
        .semantics
        .get("broker_generated_attempt_input_fields")
    else {
        return Ok(Vec::new());
    };
    let Some(values) = raw_fields.as_array() else {
        return Err(plan_error(
            "PLAN_GENERATED_INPUT_METADATA_INVALID",
            "The registered broker-generated input field metadata is invalid.",
            "Fix the operation definition before authorizing a plan.",
        ));
    };
    if is_read && !values.is_empty() {
        return Err(plan_error(
            "PLAN_READ_SCOPE_INVALID",
            "Read operations cannot declare broker-generated submission identities.",
            "Remove the generated attempt input metadata from this read operation.",
        ));
    }
    let mut fields = Vec::with_capacity(values.len());
    for value in values {
        let Some(pointer) = value.as_str() else {
            return Err(plan_error(
                "PLAN_GENERATED_INPUT_METADATA_INVALID",
                "The registered generated input metadata must contain JSON pointer strings.",
                "Fix the operation definition before authorizing a plan.",
            ));
        };
        validate_generated_input_pointer(pointer, input, &definition.input_schema)?;
        fields.push(pointer.to_owned());
    }
    fields.sort();
    if fields.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(plan_error(
            "PLAN_GENERATED_INPUT_METADATA_INVALID",
            "A generated attempt input field was declared more than once.",
            "Fix the operation definition before authorizing a plan.",
        ));
    }
    Ok(fields)
}

pub(super) fn validate_generated_plan_candidate(
    adapter: &dyn gofin_domain::Adapter,
    revision: &FinancialPlanRevision,
    step_id: &str,
    raw_input: &Value,
    expected_input: &Value,
    attempt_id: &str,
    prepared: &PreparedOperation,
) -> Result<()> {
    let step = revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| internal("plan step disappeared"))?;
    let permission = permission_for_step(revision, step_id)?;
    let definition = adapter
        .definitions()
        .into_iter()
        .find(|definition| definition.name == step.operation)
        .ok_or_else(|| internal("registry definition disappeared"))?;
    let definition_fields = generated_attempt_input_fields(&definition, &step.input, false)?;
    if definition_fields != permission.generated_input_fields {
        return Err(plan_error(
            "PLAN_OPERATION_CONTRACT_CHANGED",
            "The registered broker-generated attempt fields changed after approval.",
            "Review and authorize a fresh plan revision.",
        ));
    }
    let mut ordinary_expected = plan_attempt_input(revision, step_id, raw_input, attempt_id)?;
    if let Some(recovery) = permission
        .permission
        .as_ref()
        .and_then(|permission| permission.recovery_parent.as_ref())
    {
        for pointer in [&recovery.operation_id_pointer, &recovery.submission_pointer] {
            let recovered = expected_input.pointer(pointer).ok_or_else(|| {
                recovery_parent_invalid(
                    "The engine did not resolve the approved recovery identity.",
                )
            })?;
            if recovered.is_null() {
                return Err(recovery_parent_invalid(
                    "The engine did not resolve the approved recovery identity.",
                ));
            }
            set_json_pointer(&mut ordinary_expected, pointer, recovered.clone())?;
        }
        for field in &recovery.parent_binding_fields {
            let recovered = expected_input
                .pointer(&field.child_input_pointer)
                .ok_or_else(|| {
                    recovery_parent_invalid(
                        "The engine did not resolve an approved parent binding field.",
                    )
                })?;
            set_json_pointer(
                &mut ordinary_expected,
                &field.child_input_pointer,
                recovered.clone(),
            )?;
        }
    }
    if let Some(deadline) = &prepared.action_deadline {
        deadline.apply(&mut ordinary_expected)?;
    }
    if &ordinary_expected != expected_input {
        return Err(recovery_parent_invalid(
            "The resolved recovery input changed outside its typed parent fields.",
        ));
    }
    validate_plan_json_schema(&definition.input_schema, expected_input)?;
    validate_prepared_generated_input(
        &permission.generated_input_fields,
        expected_input,
        &prepared.input,
    )
}

fn validate_generated_input_pointer(pointer: &str, input: &Value, schema: &Value) -> Result<()> {
    let tokens = decode_json_pointer(pointer)?;
    let Some(Value::String(_)) = input.pointer(pointer) else {
        return Err(plan_error(
            "PLAN_GENERATED_INPUT_METADATA_INVALID",
            "A broker-generated attempt field must point to a string in the plan input template.",
            "Fix the operation definition or supply its required string identity field.",
        ));
    };
    let mut schema_at = schema;
    for token in tokens {
        schema_at = schema_at
            .get("properties")
            .and_then(|properties| properties.get(&token))
            .ok_or_else(|| {
                plan_error(
                    "PLAN_GENERATED_INPUT_METADATA_INVALID",
                    "A broker-generated attempt field must identify a declared object property.",
                    "Fix the operation schema and metadata before authorizing a plan.",
                )
            })?;
    }
    if schema_at.get("type") != Some(&Value::String("string".into())) {
        return Err(plan_error(
            "PLAN_GENERATED_INPUT_METADATA_INVALID",
            "A broker-generated attempt field must have a string input schema.",
            "Fix the operation schema and metadata before authorizing a plan.",
        ));
    }
    Ok(())
}

fn decode_json_pointer(pointer: &str) -> Result<Vec<String>> {
    if !pointer.starts_with('/') || pointer == "/" {
        return Err(plan_error(
            "PLAN_GENERATED_INPUT_METADATA_INVALID",
            "A broker-generated attempt field must use a nonempty JSON pointer.",
            "Use a pointer such as /requestId in the operation definition.",
        ));
    }
    pointer[1..]
        .split('/')
        .map(|part| {
            let mut decoded = String::new();
            let mut chars = part.chars();
            while let Some(character) = chars.next() {
                if character == '~' {
                    match chars.next() {
                        Some('0') => decoded.push('~'),
                        Some('1') => decoded.push('/'),
                        _ => {
                            return Err(plan_error(
                                "PLAN_GENERATED_INPUT_METADATA_INVALID",
                                "A broker-generated input pointer is malformed.",
                                "Use valid RFC 6901 JSON pointer escaping in the operation definition.",
                            ));
                        }
                    }
                } else {
                    decoded.push(character);
                }
            }
            if decoded.is_empty() {
                return Err(plan_error(
                    "PLAN_GENERATED_INPUT_METADATA_INVALID",
                    "A broker-generated input pointer has an empty path segment.",
                    "Use a pointer to a declared string property.",
                ));
            }
            Ok(decoded)
        })
        .collect()
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

fn stable_connection_hash(connections: &[gofin_domain::ConnectionMetadata]) -> Result<String> {
    let stable = connections
        .iter()
        .map(|connection| {
            json!({
                "name": connection.name,
                "provider": connection.provider,
                "resource": connection.resource,
                "role": connection.role,
                "parent": connection.parent,
                "network": connection.network,
                "address": connection.address,
                "credential_ref": connection.credential_ref,
                "signing_enabled": connection.signing_enabled,
                "details": connection.details,
            })
        })
        .collect::<Vec<_>>();
    let mut keyed = stable
        .into_iter()
        .map(|value| Ok((gofin_domain::canonical_json(&value)?, value)))
        .collect::<Result<Vec<_>>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    binding_hash(&json!({
        "connections": keyed.into_iter().map(|(_, value)| value).collect::<Vec<_>>()
    }))
}

pub(super) fn validate_candidate_plan_caps(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
    store: &Store,
) -> Result<()> {
    let permission = permission_for_step(&record.revision, step_id)?;
    let scope = plan_boundary_spec(permission)?;
    let mut prior_effects = Vec::new();
    let mut spent_debits = Vec::new();
    let mut spent_costs = Vec::new();
    for attempt in record
        .attempts
        .iter()
        .filter(|attempt| attempt.step_id == step_id)
    {
        let Some(raw) = store.get_record(&attempt.attempt_id).map_err(map_store)? else {
            return Err(plan_error(
                "PLAN_ATTEMPT_MISSING",
                "A plan attempt record is missing.",
                "Restore the durable operation record before continuing.",
            ));
        };
        let operation: OperationRecord = from_value(raw)?;
        if let Some(verified) = operation.effects {
            prior_effects.extend(verified.effects.clone());
            let (debits, costs, _) =
                summarize_plan_effects(&verified.effects, step_id, &record.usage)?;
            spent_debits = add_amounts(&spent_debits, &debits)?;
            spent_costs = add_cost_bounds(&spent_costs, &costs)?;
        }
    }
    let remaining_debits = remaining_debits(permission_debits(permission), Some(&spent_debits))?;
    let remaining_costs = remaining_costs(permission_costs(permission), Some(&spent_costs))?;
    let step = record
        .revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| {
            plan_error(
                "PLAN_STEP_NOT_FOUND",
                "Step does not belong to this plan.",
                "Use a step from the active revision.",
            )
        })?;
    if !non_cash_plan_sources::validate_exact_bridge_receipt_candidate(
        record,
        step,
        permission,
        prepared,
        &prior_effects,
        store,
    )? {
        validate_exact_non_cash_receipt_candidate(step, permission, prepared, &prior_effects)?;
    }
    if let Some(target) = &step.completion.principal_spend {
        let consumed = spent_debits
            .iter()
            .filter(|item| {
                item.resource == target.resource && item.amount.asset == target.amount.asset
            })
            .fold(BigUint::zero(), |sum, item| {
                sum + item.amount.atoms.integer()
            });
        let target_atoms = target.amount.atoms.integer();
        let target_remaining = if consumed >= target_atoms {
            BigUint::zero()
        } else {
            target_atoms - consumed
        };
        let candidate = prepared
            .intent
            .debits
            .iter()
            .filter(|item| {
                item.resource == target.resource && item.amount.asset == target.amount.asset
            })
            .fold(BigUint::zero(), |sum, item| {
                sum + item.amount.atoms.integer()
            });
        if candidate > target_remaining {
            return Err(plan_error(
                "PLAN_COMPLETION_TARGET_EXCEEDED",
                "This attempt exceeds the step's remaining principal-spend target.",
                "Reduce the input to the remaining target or revise the plan with fresh approval.",
            ));
        }
    }
    let authorization = BoundaryAuthorization {
        authorization_source: None,
        yolo_provenance: None,
        id: record.revision.plan_id.clone(),
        intent_id: record.revision.plan_id.clone(),
        operation: prepared.operation.clone(),
        bounds: Value::Null,
        spec: scope,
        approved_at: record.revision.approved_at.clone(),
        expires_at: record.revision.expires_at.clone(),
        valid_for_seconds: record.revision.valid_for_seconds,
        status: BoundaryAuthorizationStatus::Active,
        remaining_debits,
        remaining_costs,
        spent_debits,
        spent_costs,
        replaces: None,
    };
    validate_candidate_caps(&authorization, prepared)?;
    validate_cumulative_scope_caps(record, step_id, prepared, store)
}

struct PlanBridgeAttemptClaim {
    kind: gofin_domain::BridgeStepKind,
    expected_transfer: Value,
    updated_transfer: gofin_domain::BridgeTransferRecord,
}

fn validate_plan_bridge_attempt_scope(
    request: &OperationRequest,
    prepared: &PreparedOperation,
    plan_id: &str,
    revision_id: &str,
    plan_step_id: &str,
    existing_operation_id: Option<&str>,
    store: &Store,
) -> Result<Option<PlanBridgeAttemptClaim>> {
    let marker = prepared
        .binding
        .product_get(gofin_domain::BRIDGE_STEP_METADATA_KEY)?;
    if !matches!(
        request.operation.as_str(),
        "bridge.transfer" | "bridge.execute"
    ) {
        if marker.is_some() {
            return Err(plan_error(
                "PLAN_BRIDGE_BINDING_INVALID",
                "A non-bridge plan step contains a bridge identity marker.",
                "Review the adapter-prepared candidate and use the matching operation.",
            ));
        }
        return Ok(None);
    }
    let marker = marker.ok_or_else(|| {
        plan_error(
            "PLAN_BRIDGE_BINDING_MISSING",
            "The prepared bridge plan step lacks its durable transfer and link identity.",
            "Use a saved quote and a declared bridge step from the active plan revision.",
        )
    })?;
    let marker_text = |key: &str| {
        marker
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                plan_error(
                    "PLAN_BRIDGE_BINDING_INVALID",
                    "The prepared bridge step identity is incomplete.",
                    "Use the exact stored transfer and provider step selected in the plan.",
                )
            })
    };
    let transfer_id = marker_text("transfer_id")?;
    let bridge_step_id = marker_text("step_id")?;
    let kind: gofin_domain::BridgeStepKind = serde_json::from_value(
        marker.get("kind").cloned().unwrap_or(Value::Null),
    )
    .map_err(|_| {
        plan_error(
            "PLAN_BRIDGE_BINDING_INVALID",
            "The prepared bridge step kind is unknown.",
            "Use an approval, allowance reset, or deposit link declared by the stored quote.",
        )
    })?;
    let public_kind: Value =
        serde_json::to_value(&kind).map_err(|_| internal("bridge kind serialization failed"))?;
    let input_field = |snake: &str, camel: &str| {
        let left = request.input.get(snake);
        let right = request.input.get(camel);
        if left.is_some() && right.is_some() && left != right {
            return Err(plan_error(
                "PLAN_BRIDGE_BINDING_INVALID",
                "The public bridge request contains conflicting step identities.",
                "Use one canonical transferId, stepId, and stepKind triplet.",
            ));
        }
        Ok(left.or(right))
    };
    if input_field("transfer_id", "transferId")?.and_then(Value::as_str)
        != Some(transfer_id.as_str())
        || input_field("step_id", "stepId")?.and_then(Value::as_str)
            != Some(bridge_step_id.as_str())
        || input_field("step_kind", "stepKind")? != Some(&public_kind)
        || marker.get("grant_id").is_some_and(|grant| !grant.is_null())
    {
        return Err(plan_error(
            "PLAN_BRIDGE_BINDING_INVALID",
            "The public bridge request is not the exact ungranted step bound to this plan candidate.",
            "Use the exact transfer, step and kind from the saved quote without a separate bridge grant.",
        ));
    }
    let bounds = marker
        .get("bounds")
        .filter(|value| !value.is_null())
        .ok_or_else(|| {
            plan_error(
                "PLAN_BRIDGE_SCOPE_MISSING",
                "The bridge plan candidate lacks its selected bounds.",
                "Review explicit bridge amount and fee bounds before authorizing the plan.",
            )
        })?;
    let expectation: gofin_domain::BridgeSettlementExpectation = serde_json::from_value(
        marker
            .get("settlement_expectation")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|_| {
        plan_error(
            "PLAN_BRIDGE_SCOPE_INVALID",
            "The bridge settlement expectation is malformed.",
            "Review the exact destination and approved minimum in the plan permission.",
        )
    })?;
    let mut store_transfer = store
        .bridge_transfer(&transfer_id)
        .map_err(map_store)?
        .ok_or_else(|| {
            plan_error(
                "PLAN_BRIDGE_TRANSFER_MISSING",
                "The prepared bridge step has no durable quote record.",
                "Restore the original quote record and do not submit a replacement transfer.",
            )
        })?;
    // A logical bridge intent chooses one saved route. Once any child of that
    // route is claimed in the plan journal, later allowance/deposit/finalize
    // attempts must continue the same transfer rather than nominate a second
    // provider and risk bridging the source amount twice.
    if request.operation == "bridge.execute"
        && store
            .bridge_transfers()
            .map_err(map_store)?
            .iter()
            .any(|transfer| {
                transfer.id != transfer_id
                    && transfer.steps.iter().any(|link| {
                        link.state != gofin_domain::BridgeStepState::Unsubmitted
                            && link.plan_authority.as_ref().is_some_and(|authority| {
                                authority.plan_id == plan_id
                                    && authority.plan_step_id == plan_step_id
                            })
                    })
            })
    {
        return Err(plan_error(
            "PLAN_BRIDGE_ROUTE_FROZEN",
            "A bridge route was already claimed by this logical plan step.",
            "Continue or reconcile the saved transfer before considering another route.",
        ));
    }
    if store_transfer.grant_id.is_some()
        || (store_transfer.transit.is_some() && !kind.is_destination_stage())
        || store_transfer
            .plan_scope
            .as_ref()
            .is_some_and(|scope| scope.plan_id != plan_id)
        || !matches!(
            store_transfer.state,
            gofin_domain::BridgeTransferState::Prepared
                | gofin_domain::BridgeTransferState::ApprovalPending
                | gofin_domain::BridgeTransferState::SourcePending
                | gofin_domain::BridgeTransferState::InTransit
                | gofin_domain::BridgeTransferState::DestinationPending
        )
    {
        return Err(plan_error(
            "PLAN_BRIDGE_OWNERSHIP_CONFLICT",
            "This transfer is already owned by a standalone authorization, another plan, or a submitted settlement.",
            "Keep its original authority and use a separate unclaimed quote for this plan.",
        ));
    }
    let expected_transfer = to_value(&store_transfer)?;
    let scope = store_transfer
        .plan_scope
        .clone()
        .unwrap_or(gofin_domain::BridgePlanScope {
            plan_id: plan_id.to_owned(),
            revision_id: revision_id.to_owned(),
        });
    if scope.plan_id != plan_id {
        return Err(plan_error(
            "PLAN_BRIDGE_OWNERSHIP_CONFLICT",
            "This transfer is anchored to a different named plan.",
            "Keep the saved transfer with its original plan identity.",
        ));
    }
    match (
        &store_transfer.approved_bounds,
        &store_transfer.settlement_expectation,
    ) {
        (Some(saved_bounds), Some(saved_expectation))
            if saved_bounds == bounds && saved_expectation == &expectation => {}
        (None, None) => {
            store_transfer.approved_bounds = Some(bounds.clone());
            store_transfer.settlement_expectation = Some(expectation);
        }
        (None, Some(quote_expectation))
            if bridge_expectation_preserves_quote_targets(quote_expectation, &expectation) =>
        {
            store_transfer.approved_bounds = Some(bounds.clone());
            store_transfer.settlement_expectation = Some(expectation);
        }
        _ => {
            return Err(plan_error(
                "PLAN_BRIDGE_SCOPE_CONFLICT",
                "Bridge bounds or settlement targets differ from the scope already frozen to this parent.",
                "Preserve the original selected route and bounds or create a new plan and quote before dispatch.",
            ));
        }
    }
    let authority = gofin_domain::BridgePlanAuthority {
        plan_id: plan_id.to_owned(),
        revision_id: revision_id.to_owned(),
        plan_step_id: plan_step_id.to_owned(),
    };
    let Some(link_index) = store_transfer
        .steps
        .iter()
        .position(|link| link.step_id == bridge_step_id)
    else {
        return Err(plan_error(
            "PLAN_BRIDGE_STEP_NOT_FOUND",
            "The bridge link is not declared in its saved transfer.",
            "Use only exact approval, reset, or deposit steps from the stored quote.",
        ));
    };
    let link_snapshot = store_transfer.steps[link_index].clone();
    let dependencies_settled = link_snapshot.depends_on.iter().all(|dependency| {
        store_transfer.steps.iter().any(|prior| {
            prior.step_id == *dependency && prior.state == gofin_domain::BridgeStepState::Settled
        })
    });
    let link = &mut store_transfer.steps[link_index];
    if link.kind != kind
        || link.grant_id.is_some()
        || match existing_operation_id {
            None => {
                link.operation_id.is_some()
                    || link.state != gofin_domain::BridgeStepState::Unsubmitted
            }
            Some(id) => {
                link.operation_id.as_deref() != Some(id)
                    || link.state != gofin_domain::BridgeStepState::Submitted
                    || link.plan_authority.as_ref() != Some(&authority)
            }
        }
        || link.plan_authority.as_ref().is_some_and(|existing| {
            existing.plan_id != authority.plan_id || existing.plan_step_id != authority.plan_step_id
        })
        || !dependencies_settled
    {
        return Err(plan_error(
            "PLAN_BRIDGE_STEP_UNAVAILABLE",
            "The bridge step is claimed, outside plan scope, or its declared prerequisites are unsettled.",
            "Reconcile the original operation or complete the exact prerequisite steps before continuing.",
        ));
    }
    store_transfer.plan_scope = Some(scope);
    if kind.is_destination_stage() && store_transfer.transit.is_none() {
        return Err(plan_error(
            "PLAN_BRIDGE_SOURCE_PROOF_REQUIRED",
            "A deferred bridge stage requires settled original source proof.",
            "Reconcile the initiation before executing its proof or finalization.",
        ));
    }
    Ok(Some(PlanBridgeAttemptClaim {
        kind,
        expected_transfer,
        updated_transfer: store_transfer,
    }))
}

fn bridge_expectation_preserves_quote_targets(
    quote: &gofin_domain::BridgeSettlementExpectation,
    selected: &gofin_domain::BridgeSettlementExpectation,
) -> bool {
    let targets_match = |quoted: &[gofin_domain::BridgeSettlementTarget],
                         chosen: &[gofin_domain::BridgeSettlementTarget]| {
        quoted.len() == chosen.len()
            && quoted.iter().all(|target| {
                chosen.iter().any(|candidate| {
                    candidate.resource == target.resource && candidate.asset == target.asset
                })
            })
    };
    targets_match(&quote.destination, &selected.destination)
        && targets_match(&quote.refund, &selected.refund)
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

fn validate_cumulative_scope_caps(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
    store: &Store,
) -> Result<()> {
    let Some(scope) = permission_for_step(&record.revision, step_id)?
        .permission
        .as_ref()
        .and_then(|permission| permission.cumulative_scope.as_ref())
    else {
        return Ok(());
    };
    let mut revisions = store
        .financial_plan_revisions(&record.revision.plan_id)
        .map_err(map_store)?
        .into_iter()
        .map(from_value::<FinancialPlanRevision>)
        .collect::<Result<Vec<_>>>()?;
    if !revisions
        .iter()
        .any(|revision| revision.revision_id == record.revision.revision_id)
    {
        revisions.push(record.revision.clone());
    }
    let mut spent_debits = Vec::new();
    let mut spent_costs = Vec::new();
    let mut incurred = Vec::new();
    for attempt in &record.attempts {
        if record.active_attempt_id.as_deref() == Some(attempt.attempt_id.as_str()) {
            continue;
        }
        let belongs_to_scope = revisions
            .iter()
            .find(|revision| revision.revision_id == attempt.revision_id)
            .and_then(|revision| {
                revision
                    .permissions
                    .iter()
                    .find(|permission| permission.step_id == attempt.step_id)
            })
            .and_then(|permission| permission.permission.as_ref())
            .and_then(|permission| permission.cumulative_scope.as_ref())
            .is_some_and(|candidate| candidate.scope_id == scope.scope_id);
        if !belongs_to_scope {
            continue;
        }
        let Some(raw) = store.get_record(&attempt.attempt_id).map_err(map_store)? else {
            return Err(plan_error(
                "PLAN_ATTEMPT_MISSING",
                "A cumulative-scope attempt record is missing.",
                "Restore the original plan attempt before continuing.",
            ));
        };
        let operation: OperationRecord = from_value(raw)?;
        if let Some(verified) = operation.effects {
            incurred = add_amounts(&incurred, &verified_obligations(&verified.effects))?;
            let (debits, costs, _) =
                summarize_plan_effects(&verified.effects, &attempt.step_id, &record.usage)?;
            spent_debits = add_amounts(&spent_debits, &debits)?;
            spent_costs = add_cost_bounds(&spent_costs, &costs)?;
        } else if let Some(reservation) = record
            .usage
            .reservations
            .iter()
            .find(|reservation| reservation.attempt_id == attempt.attempt_id)
        {
            if let Some(preview) = operation.preview.as_ref() {
                incurred = add_amounts(&incurred, &reserved_obligations(&preview.intent))?;
            } else {
                return Err(internal("Reserved attempt lacks its durable preview."));
            }
            spent_debits = add_amounts(&spent_debits, &reservation.debits)?;
            spent_costs = add_cost_bounds(&spent_costs, &reservation.costs)?;
        }
    }
    let proposed = add_amounts(&incurred, &reserved_obligations(&prepared.intent))?;
    for obligation in proposed
        .iter()
        .filter(|_| !scope.maximum_obligations.is_empty())
    {
        let cap = scope
            .maximum_obligations
            .iter()
            .filter(|cap| {
                cap.resource == obligation.resource && cap.amount.asset == obligation.amount.asset
            })
            .max_by(|left, right| left.amount.atoms.cmp(&right.amount.atoms));
        if cap.is_none_or(|cap| cap.amount.atoms < obligation.amount.atoms) {
            return Err(plan_error(
                "PLAN_OBLIGATION_CAP_EXCEEDED",
                "Cumulative incurred debt exceeds the approved scope; repayment does not replenish borrowing authority.",
                "Reduce the borrowing candidate within the remaining approved scope. If the human intends a larger debt limit, review and authorize a separate finite plan; revising this plan cannot widen its cumulative parent cap. Reconcile prior attempts and preserve all incurred debt and costs.",
            ).with_details(json!({
                "step_id": step_id, "scope_id": scope.scope_id,
                "resource": obligation.resource, "asset": obligation.amount.asset,
                "required_atoms": obligation.amount.atoms,
                "maximum_atoms": cap.map(|cap| &cap.amount.atoms),
                "scope_path": "/maximum_obligations",
            })));
        }
    }
    let remaining_debits = remaining_debits(&scope.maximum_debits, Some(&spent_debits))?;
    let remaining_costs = remaining_costs(&scope.maximum_costs, Some(&spent_costs))?;
    let authorization = BoundaryAuthorization {
        authorization_source: None,
        yolo_provenance: None,
        id: scope.scope_id.clone(),
        intent_id: scope.scope_id.clone(),
        operation: prepared.operation.clone(),
        bounds: Value::Null,
        spec: BoundarySpec {
            fixed: Value::Null,
            parameters: Value::Null,
            maximum_debits: scope.maximum_debits.clone(),
            maximum_costs: scope.maximum_costs.clone(),
            summary: "shared adapter cumulative scope".into(),
        },
        approved_at: record.revision.approved_at.clone(),
        expires_at: record.revision.expires_at.clone(),
        valid_for_seconds: record.revision.valid_for_seconds,
        status: BoundaryAuthorizationStatus::Active,
        remaining_debits,
        remaining_costs,
        spent_debits,
        spent_costs,
        replaces: None,
    };
    validate_candidate_caps(&authorization, prepared)
}

fn plan_boundary_spec(permission: &FinancialPlanStepPermission) -> Result<BoundarySpec> {
    if let Some(adapter) = &permission.permission {
        return Ok(adapter.spec.clone());
    }
    let exact = permission.exact.as_ref().ok_or_else(|| {
        plan_error(
            "PLAN_PERMISSION_MISSING",
            "A plan step has no permission scope.",
            "Create a complete plan revision.",
        )
    })?;
    Ok(BoundarySpec {
        fixed: json!({"mode":"exact", "definition_hash": exact.definition_hash, "connections_hash": exact.connections_hash}),
        parameters: json!({}),
        maximum_debits: exact.scope.maximum_debits.clone(),
        maximum_costs: exact.scope.maximum_costs.clone(),
        summary: "exact operation scope".into(),
    })
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
