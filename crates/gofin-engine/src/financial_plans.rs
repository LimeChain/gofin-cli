use super::*;
use gofin_domain::financial_plans::{
    self, FinancialPlanAttemptLinkage, FinancialPlanAuthorizationRecord,
    FinancialPlanAuthorizationRequest, FinancialPlanAuthorizationStatus,
    FinancialPlanExactPermission, FinancialPlanExactScope, FinancialPlanFundingIdentity,
    FinancialPlanProceeds, FinancialPlanReview, FinancialPlanRevision, FinancialPlanStatus,
    FinancialPlanStepPermission, FinancialPlanStepProgress, FinancialPlanStepProgressState,
    FinancialPlanUsage,
};
use gofin_domain::product_names::MetadataValueExt;

const MAX_PLAN_CANDIDATE_REFRESHES: usize = 2;
const MAX_PLAN_RPC_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

fn plan_preview_logical_operation_id(
    plan_id: &str,
    revision_id: &str,
    step_id: &str,
    preview_attempt_id: &str,
) -> String {
    format!("plan-preview:{plan_id}:{revision_id}:{step_id}:{preview_attempt_id}")
}

fn plan_execution_logical_operation_id(
    plan_id: &str,
    revision_id: &str,
    step_id: &str,
    logical_request_id: &str,
) -> String {
    format!("plan-execution:{plan_id}:{revision_id}:{step_id}:{logical_request_id}")
}

enum VerifiedExpiryRetry {
    NotEligible,
    Refresh,
    Stop(Box<OperationRecord>),
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
        request: FinancialPlanAuthorizationRequest,
    ) -> Result<FinancialPlanReview> {
        self.validate_plan_request(&request)?;
        let prior = self.plan_replacement(&request)?;
        let completed_steps = prior
            .as_ref()
            .map(|record| self.build_plan_status(record, &record.revision))
            .transpose()?
            .map(completed_plan_step_ids)
            .unwrap_or_default();
        let permissions = self
            .prepare_plan_permissions(&request.plan, prior.as_ref(), &completed_steps)
            .await?;
        validate_plan_permissions(&request.plan, &permissions)?;
        self.validate_next_plan_reservation(&request.plan, &completed_steps)?;
        let (valid_for_seconds, timing_warnings) =
            self.resolve_plan_duration(&request, prior.as_ref())?;
        validate_plan_authorization_duration(valid_for_seconds, &permissions)?;
        let feasibility_warnings = self
            .validate_plan_feasibility(&request.plan, prior.as_ref(), &completed_steps)
            .await?;
        let carried_forward_usage = prior
            .as_ref()
            .map(|record| record.usage.clone())
            .unwrap_or_default();
        {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            validate_revision_transition_with_completed(
                prior.as_ref(),
                &request.plan,
                &permissions,
                &carried_forward_usage,
                &store,
                &completed_steps,
            )?;
        }
        let review_hash = financial_plans::financial_plan_review_hash(
            &request.plan,
            &permissions,
            valid_for_seconds,
            request.replaces_revision_id.as_deref(),
            &carried_forward_usage,
        )?;
        let remaining_debits = remaining_debits(
            &request.plan.aggregate_bounds.maximum_debits,
            Some(&carried_forward_usage.consumed_debits),
        )?;
        let remaining_costs = remaining_costs(
            &request.plan.aggregate_bounds.maximum_costs,
            Some(&carried_forward_usage.consumed_costs),
        )?;
        Ok(FinancialPlanReview {
            plan: request.plan,
            permissions,
            review_hash,
            valid_for_seconds,
            replaces_revision_id: request.replaces_revision_id,
            plan_id: prior.as_ref().map(|record| record.revision.plan_id.clone()),
            carried_forward_usage,
            remaining_debits,
            remaining_costs,
            timing_warnings,
            feasibility_warnings,
        })
    }

    /// Obtain one fresh approval for a named plan revision. Request IDs are
    /// idempotent and revision rows remain immutable after approval.
    pub async fn authorize_financial_plan(
        &self,
        request: FinancialPlanAuthorizationRequest,
    ) -> Result<FinancialPlanStatus> {
        let _guard = self.write_queue.lock().await;
        self.validate_plan_request(&request)?;
        let fingerprint = binding_hash(&json!({
            "version": 1,
            "request_id": request.request_id,
            "plan": request.plan,
            "valid_for_seconds": request.valid_for_seconds,
            "replaces_revision_id": request.replaces_revision_id,
        }))?;
        let existing_request = {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .financial_plan_request_status(&request.request_id)
                .map_err(map_store)?
        };
        if let Some((existing_fingerprint, status, plan_id, revision_id)) = existing_request {
            if existing_fingerprint != fingerprint {
                return Err(plan_error(
                    "REQUEST_ID_CONFLICT",
                    "This request identity already names different plan parameters.",
                    "Use the original request identity for retries or choose a new identity for a deliberate revision.",
                ));
            }
            if status == "approved" {
                let record = self.load_financial_plan(&plan_id)?;
                validate_financial_approval_source(&record.approval)?;
                let mut revision = self.load_financial_plan_revision(
                    &plan_id,
                    revision_id
                        .as_deref()
                        .unwrap_or(&record.revision.revision_id),
                )?;
                revision.status = if record.revision.revision_id == revision.revision_id {
                    record.revision.status.clone()
                } else {
                    FinancialPlanAuthorizationStatus::Superseded
                };
                return self.build_plan_status(&record, &revision);
            }
            return Err(plan_error(
                "PLAN_REQUEST_UNAVAILABLE",
                "The original plan approval request did not complete.",
                "Use a new request ID for a deliberate authorization attempt.",
            ));
        }

        let prior = self.plan_replacement(&request)?;
        let completed_steps = prior
            .as_ref()
            .map(|record| self.build_plan_status(record, &record.revision))
            .transpose()?
            .map(completed_plan_step_ids)
            .unwrap_or_default();
        let permissions = self
            .prepare_plan_permissions(&request.plan, prior.as_ref(), &completed_steps)
            .await?;
        validate_plan_permissions(&request.plan, &permissions)?;
        self.validate_next_plan_reservation(&request.plan, &completed_steps)?;
        let (valid_for_seconds, _timing_warnings) =
            self.resolve_plan_duration(&request, prior.as_ref())?;
        validate_plan_authorization_duration(valid_for_seconds, &permissions)?;
        self.validate_plan_feasibility(&request.plan, prior.as_ref(), &completed_steps)
            .await?;
        let carried_usage = prior
            .as_ref()
            .map(|record| record.usage.clone())
            .unwrap_or_default();
        let review_hash = financial_plans::financial_plan_review_hash(
            &request.plan,
            &permissions,
            valid_for_seconds,
            request.replaces_revision_id.as_deref(),
            &carried_usage,
        )?;
        if request
            .expected_review_hash
            .as_deref()
            .is_some_and(|expected| expected != review_hash)
        {
            return Err(plan_error(
                "PLAN_REVIEW_CHANGED",
                "Authorization review material changed after preview.",
                "Run `gofin plans preview` again and review the regenerated plan before authorization.",
            ));
        }

        // The plan ID is stable across revisions and generated on first
        // authorization. Revision IDs are separate immutable approval units.
        let plan_id = prior
            .as_ref()
            .map(|record: &FinancialPlanAuthorizationRecord| record.revision.plan_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let revision_id = Uuid::new_v4().to_string();
        let carried_usage = prior
            .as_ref()
            .map(|record| record.usage.clone())
            .unwrap_or_default();
        {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            validate_revision_transition_with_completed(
                prior.as_ref(),
                &request.plan,
                &permissions,
                &carried_usage,
                &store,
                &completed_steps,
            )?;
        }
        let binding = financial_plans::financial_plan_approval_binding(
            &plan_id,
            &revision_id,
            &request.plan,
            &permissions,
            valid_for_seconds,
            request.replaces_revision_id.as_deref(),
            &carried_usage,
        );
        let approved_binding_hash = binding_hash(&binding)?;
        plan_duration(valid_for_seconds)?;
        let challenge = AuthorizationChallenge {
            purpose: "financial_plan".into(),
            operation_id: plan_id.clone(),
            binding,
            binding_hash: approved_binding_hash.clone(),
            summary: format!(
                "Approve the plan “{}” with {} ordered steps for {} seconds.",
                request.plan.name,
                request.plan.steps.len(),
                valid_for_seconds
            ),
            nonce: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            expires_at: (self.clock.now() + chrono::Duration::seconds(90)).to_rfc3339(),
        };
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            if !store
                .begin_financial_plan_request(
                    &request.request_id,
                    &fingerprint,
                    &plan_id,
                    &self.clock.now().to_rfc3339(),
                )
                .map_err(map_store)?
            {
                return Err(plan_error(
                    "PLAN_REQUEST_UNAVAILABLE",
                    "This plan request is already being processed.",
                    "Inspect plan status and retry only with the original request identity.",
                ));
            }
        }
        gofin_domain::execution_progress::emit(
            gofin_domain::execution_progress::Stage::AwaitingApproval,
        );
        let approval = match self.authorizer.authorize(&challenge).await {
            Ok(approval) => approval,
            Err(error) => {
                self.store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .fail_financial_plan_request(
                        &request.request_id,
                        &fingerprint,
                        &self.clock.now().to_rfc3339(),
                    )
                    .map_err(map_store)?;
                return Err(error);
            }
        };
        if let Err(error) = validate_approval(&approval, &challenge, self.clock.now()) {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .fail_financial_plan_request(
                    &request.request_id,
                    &fingerprint,
                    &self.clock.now().to_rfc3339(),
                )
                .map_err(map_store)?;
            return Err(error);
        }
        let approved_at = parse_time(&approval.approved_at)?;
        let seconds = i64::try_from(valid_for_seconds).map_err(|_| {
            plan_error(
                "PLAN_DURATION_INVALID",
                "Plan duration is too large.",
                "Choose a representable positive duration.",
            )
        })?;
        let duration = chrono::Duration::try_seconds(seconds).ok_or_else(|| {
            plan_error(
                "PLAN_DURATION_INVALID",
                "Plan duration is too large.",
                "Choose a representable positive duration.",
            )
        })?;
        let expires_at = approved_at
            .checked_add_signed(duration)
            .ok_or_else(|| {
                plan_error(
                    "PLAN_DURATION_INVALID",
                    "Plan expiry cannot be represented.",
                    "Choose a shorter duration.",
                )
            })?
            .to_rfc3339();
        let mut revision = FinancialPlanRevision {
            plan_id: plan_id.clone(),
            revision_id: revision_id.clone(),
            spec: request.plan,
            permissions,
            approval_binding_hash: approved_binding_hash,
            approved_at: approval.approved_at.clone(),
            expires_at,
            authorization_source: approval.source.clone(),
            yolo_provenance: approval.yolo_provenance.clone(),
            valid_for_seconds,
            status: FinancialPlanAuthorizationStatus::Active,
            replaces_revision_id: request.replaces_revision_id.clone(),
        };
        let mut record = prior
            .clone()
            .unwrap_or_else(|| FinancialPlanAuthorizationRecord {
                revision: revision.clone(),
                approval: approval.clone(),
                request_fingerprint: fingerprint.clone(),
                usage: FinancialPlanUsage::default(),
                attempts: Vec::new(),
                active_attempt_id: None,
            });
        record.revision = revision.clone();
        record.approval = approval;
        record.request_fingerprint = fingerprint.clone();
        record.usage = carried_usage;
        let admission_lease = self
            .authorizer
            .acquire_submission_authority(&record.approval)?;
        if let Some(provenance) =
            super::validate_submission_authority_lease(&record.approval, admission_lease.as_ref())?
        {
            record.approval.yolo_provenance = Some(provenance);
            revision.yolo_provenance = record.approval.yolo_provenance.clone();
            record.revision = revision.clone();
        }
        let timestamp = self.clock.now().to_rfc3339();
        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        store
            .commit_financial_plan_revision(
                &request.request_id,
                &fingerprint,
                &plan_id,
                &revision_id,
                prior.as_ref().map(to_value).transpose()?.as_ref(),
                &to_value(&revision)?,
                &to_value(&record)?,
                &timestamp,
            )
            .map_err(map_store)?;
        drop(store);
        self.build_plan_status(&record, &revision)
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
        let Ok(_guard) = self.write_queue.try_lock() else {
            // A running execution already owns reconciliation. Return durable
            // state promptly; retry the snapshot if a settlement changes it.
            for _ in 0..3 {
                let record = self.load_financial_plan(plan_id)?;
                let status = self.build_plan_status(&record, &record.revision)?;
                if self.load_financial_plan(plan_id)? == record {
                    return Ok(status);
                }
            }
            return Err(plan_error(
                "PLAN_STATUS_BUSY",
                "Plan state changed while reading status.",
                "Retry status; no new financial action is required.",
            ));
        };
        let mut record = self.load_financial_plan(plan_id)?;
        expire_plan_if_needed(self, &mut record)?;
        let mut unresolved_ids = Vec::new();
        for attempt in &record.attempts {
            let operation = self.load_record(&attempt.attempt_id)?;
            if matches!(
                operation.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                unresolved_ids.push(operation.id);
            }
        }
        for attempt_id in unresolved_ids {
            let _ = self.reconcile_observation_serial(attempt_id).await?;
        }
        record = self.load_financial_plan(plan_id)?;
        self.build_plan_status(&record, &record.revision.clone())
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
        plan_id: &str,
        step_id: &str,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation> {
        let record = self.load_financial_plan(plan_id)?;
        self.validate_plan_step(&record, step_id, operation, input)?;
        let (adapter, kind) = self.resolve(operation)?;
        validate_plan_operation_identity(&*adapter, &record.revision, step_id)?;
        if kind == OperationKind::Read {
            let permission = permission_for_step(&record.revision, step_id)?;
            let exact = permission.exact.as_ref().ok_or_else(|| {
                plan_error(
                    "PLAN_PERMISSION_MISSING",
                    "Read steps require an exact registered operation scope.",
                    "Create a fresh plan revision with an exact read scope.",
                )
            })?;
            let definition = adapter
                .definitions()
                .into_iter()
                .find(|definition| definition.name == operation)
                .ok_or_else(|| internal("registry definition disappeared"))?;
            validate_exact_plan_identity(adapter.as_ref(), operation, exact)?;
            let output = adapter.read(operation, input).await?;
            validate_plan_json_schema(&definition.output_schema, &output)?;
            return Ok(PreparedOperation {
                operation: operation.into(),
                input: input.clone(),
                binding: json!({"plan_read_preview": true, "result": output}),
                summary: format!("Read-only plan step `{step_id}`."),
                checks: vec![],
                warnings: vec![],
                observed_at: self.clock.now().to_rfc3339(),
                expires_at: self.clock.now().to_rfc3339(),
                valuation: None,
                intent: gofin_domain::FinancialIntent::default(),
                operation_version: 1,
                action_deadline: None,
            });
        }
        let preview_attempt_id = Uuid::new_v4().to_string();
        let action_deadline =
            self.prepare_action_deadline(&*adapter, &record.revision, step_id, input)?;
        let mut execution_input =
            self.plan_step_attempt_input(&record, step_id, input, &preview_attempt_id)?;
        if let Some(deadline) = &action_deadline {
            deadline.apply(&mut execution_input)?;
        }
        let mut context = self.preparation_context(None)?;
        context.logical_operation_id = Some(plan_preview_logical_operation_id(
            &record.revision.plan_id,
            &record.revision.revision_id,
            step_id,
            &preview_attempt_id,
        ));
        gofin_domain::execution_progress::emit(gofin_domain::execution_progress::Stage::Preparing);
        let mut prepared = bounded_evidence(
            operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare_with_context(operation, &execution_input, &context),
        )
        .await
        .map_err(|error| self.plan_candidate_recovery(error, plan_id, None))?;
        if prepared.action_deadline.is_some() {
            return Err(internal("adapter supplied broker timing"));
        }
        prepared.action_deadline = action_deadline;
        self.validate_plan_candidate(
            &*adapter,
            &record.revision,
            step_id,
            input,
            &execution_input,
            &preview_attempt_id,
            &prepared,
        )?;
        {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            validate_candidate_plan_caps(&record, step_id, &prepared, &store)?;
            validate_plan_funding_with_store(&record, step_id, &prepared, &store)?;
        }
        validate_prepared(&prepared, operation, self.clock.now())
            .map_err(|error| self.plan_candidate_recovery(error, plan_id, None))?;
        Ok(prepared)
    }

    /// Execute one explicitly submitted plan step. Each call performs fresh
    /// preparation and reservation; the plan itself never triggers execution.
    pub async fn execute_financial_plan_step(
        &self,
        plan_id: &str,
        step_id: &str,
        request: OperationRequest,
    ) -> Result<OperationRecord> {
        let _guard = self.write_queue.lock().await;
        let logical_request_id = request.request_id.clone();
        self.execute_financial_plan_step_serial(
            plan_id,
            step_id,
            request,
            None,
            &logical_request_id,
            0,
        )
        .await
    }

    pub async fn execute_financial_plan_step_replacing(
        &self,
        plan_id: &str,
        step_id: &str,
        previous_attempt_id: &str,
        request: OperationRequest,
    ) -> Result<OperationRecord> {
        let _guard = self.write_queue.lock().await;
        let logical_request_id = request.request_id.clone();
        self.execute_financial_plan_step_serial(
            plan_id,
            step_id,
            request,
            Some(previous_attempt_id),
            &logical_request_id,
            0,
        )
        .await
    }

    async fn execute_financial_plan_step_serial(
        &self,
        plan_id: &str,
        step_id: &str,
        request: OperationRequest,
        replacement: Option<&str>,
        logical_request_id: &str,
        refresh_count: usize,
    ) -> Result<OperationRecord> {
        let mut request = request;
        let mut replacement = replacement;
        let mut refresh_count = refresh_count;
        // Keep bounded candidate retries iterative so terminal no-dispatch
        // attempts cannot accumulate nested async frames on the broker stack.
        loop {
            if request.request_id.trim().is_empty() {
                return Err(plan_error(
                    "REQUEST_ID_REQUIRED",
                    "A stable attempt identity is required.",
                    "Use one request ID for this logical step attempt.",
                ));
            }
            let fingerprint = binding_hash(
                &json!({ "plan_id": plan_id, "step_id": step_id, "request": request_fingerprint(&request)? }),
            )?;
            let existing = {
                self.store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .by_request(&request.request_id)
                    .map_err(map_store)?
            };
            if let Some(existing) = existing {
                if existing.fingerprint != fingerprint {
                    return Err(plan_error(
                        "REQUEST_ID_CONFLICT",
                        "This attempt identity already names different plan parameters.",
                        "Inspect the original attempt and use a new ID only for a deliberate retry.",
                    ));
                }
                let operation: OperationRecord = from_value(existing.record)?;
                return if matches!(
                    operation.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                ) {
                    self.reconcile_serial(operation.id.clone()).await
                } else {
                    Ok(operation)
                };
            }
            if refresh_count == 0 {
                let prior_plan = self.load_financial_plan(plan_id)?;
                if let Some(linkage) = prior_plan.attempts.iter().rev().find(|attempt| {
                    attempt.logical_request_id.as_deref() == Some(logical_request_id)
                        && attempt.step_id == step_id
                }) {
                    if linkage.request_fingerprint != fingerprint {
                        return Err(plan_error(
                            "REQUEST_ID_CONFLICT",
                            "This logical request ID already names different plan parameters.",
                            "Inspect the original attempt and use a new ID only for a deliberate retry.",
                        ));
                    }
                    let operation = self.load_record(&linkage.attempt_id)?;
                    return if matches!(
                        operation.status,
                        Status::Submitted | Status::Pending | Status::Unknown
                    ) {
                        self.reconcile_serial(operation.id.clone()).await
                    } else {
                        Ok(operation)
                    };
                }
            }
            let mut plan = self.load_financial_plan(plan_id)?;
            validate_financial_approval_source(&plan.approval)?;
            // Keep the exact pre-preparation snapshot for the later store CAS. A
            // fresh read after async preparation could otherwise let a stale
            // in-memory plan overwrite a concurrent revoke or settlement.
            let initial_plan = plan.clone();
            let previous_operation = if let Some(previous_id) = replacement {
                if plan.active_attempt_id.as_deref() != Some(previous_id) {
                    return Err(plan_error(
                        "PLAN_CANDIDATE_NOT_ACTIVE",
                        "The candidate selected for replacement is no longer active.",
                        "Inspect the current plan status and use its active unsubmitted attempt ID.",
                    ));
                }
                let previous = self.load_record(previous_id)?;
                if previous.status != Status::AwaitingApproval || previous.submission.is_some() {
                    return Err(plan_error(
                        "PLAN_CANDIDATE_SUBMITTED",
                        "Submitted or uncertain attempts cannot be replaced.",
                        "Reconcile the original attempt before continuing.",
                    ));
                }
                if !plan
                    .attempts
                    .iter()
                    .any(|attempt| attempt.attempt_id == previous_id && attempt.step_id == step_id)
                {
                    return Err(plan_error(
                        "PLAN_CANDIDATE_SCOPE_MISMATCH",
                        "The selected candidate does not belong to this plan step.",
                        "Use the active attempt ID for this step.",
                    ));
                }
                let mut eligibility = plan.clone();
                eligibility.active_attempt_id = None;
                if self
                    .build_plan_status(&eligibility, &eligibility.revision)?
                    .next_eligible_step_id
                    .as_deref()
                    != Some(step_id)
                {
                    return Err(plan_error(
                        "PLAN_STEP_NOT_ELIGIBLE",
                        "Only the first incomplete plan step can be replaced.",
                        "Complete earlier steps before replacing this candidate.",
                    ));
                }
                if plan.revision.status != FinancialPlanAuthorizationStatus::Active
                    || parse_time(&plan.revision.expires_at)? <= self.clock.now()
                {
                    return Err(plan_error(
                        "PLAN_AUTHORIZATION_INACTIVE",
                        "The plan expired or was revoked before candidate replacement.",
                        "Obtain a fresh plan revision before starting another attempt.",
                    ));
                }
                Some(previous)
            } else {
                self.validate_plan_step(&plan, step_id, &request.operation, &request.input)?;
                None
            };
            let (adapter, kind) = self.resolve(&request.operation)?;
            if kind != OperationKind::Write && kind != OperationKind::Read {
                return Err(plan_error(
                    "PLAN_OPERATION_UNSUPPORTED",
                    "Plan steps must be agent-invocable reads or financial writes.",
                    "Use a supported operation from discovery.",
                ));
            }
            validate_plan_operation_identity(&*adapter, &plan.revision, step_id)?;
            if kind == OperationKind::Read {
                return self
                    .execute_financial_plan_read(
                        plan_id,
                        step_id,
                        request,
                        plan,
                        initial_plan,
                        previous_operation,
                        adapter,
                    )
                    .await;
            }
            let id = Uuid::new_v4().to_string();
            // The execution input is built below with the fresh preparation deadline
            // and request id, while preserving any typed recovery-parent fields.
            let mut execution_request = request.clone();
            execution_request.request_id = Uuid::new_v4().to_string();
            let preparation_stamp = PreparationStamp::start(self.clock.now());
            let action_deadline =
                self.prepare_action_deadline(&*adapter, &plan.revision, step_id, &request.input)?;
            let mut execution_input =
                self.plan_step_attempt_input(&plan, step_id, &request.input, &id)?;
            if let Some(deadline) = &action_deadline {
                deadline.apply(&mut execution_input)?;
            }
            execution_request.input = execution_input.clone();
            let mut context = self.preparation_context(None)?;
            context.logical_operation_id = Some(plan_execution_logical_operation_id(
                &plan.revision.plan_id,
                &plan.revision.revision_id,
                step_id,
                logical_request_id,
            ));
            gofin_domain::execution_progress::emit(
                gofin_domain::execution_progress::Stage::Preparing,
            );
            let preparation_result = bounded_evidence(
                &request.operation,
                "preflight",
                PREFLIGHT_TIMEOUT,
                adapter.prepare_with_context(&request.operation, &execution_input, &context),
            )
            .await;
            let mut prepared = match preparation_result {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.retry_plan_preparation_error(error, plan_id, refresh_count)
                        .await?;
                    request = request_with_child_id(request);
                    replacement = None;
                    refresh_count += 1;
                    continue;
                }
            };
            if prepared.action_deadline.is_some() {
                return Err(internal("adapter supplied broker timing"));
            }
            prepared.action_deadline = action_deadline;
            let required_origins = self.cost_origin_assets(&prepared, None)?;
            if let Err(error) = self
                .complete_cost_origin_prices(&mut prepared, &required_origins)
                .await
            {
                self.retry_plan_preparation_error(error, plan_id, refresh_count)
                    .await?;
                request = request_with_child_id(request);
                replacement = None;
                refresh_count += 1;
                continue;
            }
            if let Err(error) = validate_prepared(&prepared, &request.operation, self.clock.now()) {
                self.retry_plan_preparation_error(error, plan_id, refresh_count)
                    .await?;
                request = request_with_child_id(request);
                replacement = None;
                refresh_count += 1;
                continue;
            }
            self.validate_plan_candidate(
                &*adapter,
                &plan.revision,
                step_id,
                &request.input,
                &execution_input,
                &id,
                &prepared,
            )?;
            {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                validate_candidate_plan_caps(&plan, step_id, &prepared, &store)?;
            }
            if let Some(previous) = &previous_operation {
                plan.usage
                    .reservations
                    .retain(|reservation| reservation.attempt_id != previous.id);
                plan.active_attempt_id = None;
            }
            validate_plan_aggregate_remaining(&plan, &prepared)?;
            {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                validate_plan_funding_with_store(&plan, step_id, &prepared, &store)?;
            }

            let timestamp = self.clock.now().to_rfc3339();
            let mut operation = OperationRecord {
                id: id.clone(),
                request_id: execution_request.request_id.clone(),
                fingerprint: fingerprint.clone(),
                fingerprint_version: 1,
                operation: request.operation.clone(),
                input: request.input.clone(),
                status: Status::AwaitingApproval,
                created_at: timestamp.clone(),
                updated_at: timestamp.clone(),
                preview: Some(prepared.clone()),
                approval: Some(plan.approval.clone()),
                authorization_source: None,
                submission: None,
                dispatch_evidence: None,
                position_baseline: None,
                effects: None,
                last_reconciliation_evidence: None,
                error: None,
                run_id: None,
            };
            let execution_binding_hash =
                binding_hash(&financial_plans::financial_plan_attempt_binding(
                    &prepared,
                    plan_id,
                    &plan.revision.revision_id,
                    step_id,
                ))?;
            plan.attempts.push(FinancialPlanAttemptLinkage {
                plan_id: plan_id.into(),
                revision_id: plan.revision.revision_id.clone(),
                step_id: step_id.into(),
                attempt_id: id.clone(),
                request_fingerprint: fingerprint.clone(),
                execution_binding_hash,
                logical_request_id: Some(logical_request_id.to_owned()),
                submission_id: None,
                result: None,
            });
            if permission_for_step(&plan.revision, step_id)?
                .permission
                .as_ref()
                .and_then(|permission| permission.recovery_parent.as_ref())
                .is_none()
            {
                plan.active_attempt_id = Some(id.clone());
            }
            plan.usage
                .reservations
                .push(gofin_domain::financial_plans::FinancialPlanReservation {
                    attempt_id: id.clone(),
                    step_id: step_id.into(),
                    debits: prepared.intent.debits.clone(),
                    costs: prepared.intent.maximum_costs.clone(),
                });
            let updated_plan = to_value(&plan)?;
            let bridge_claim = {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                prepare_plan_bridge_attempt_claim(
                    &execution_request,
                    &prepared,
                    plan_id,
                    &plan.revision.revision_id,
                    step_id,
                    &id,
                    &store,
                )?
            };
            if bridge_claim.is_some() && previous_operation.is_some() {
                return Err(plan_error(
                    "PLAN_BRIDGE_CANDIDATE_REPLACEMENT_UNSUPPORTED",
                    "A claimed bridge operation cannot be replaced through the generic plan-candidate path.",
                    "Reconcile the original operation and preserve its bridge link before authorizing a new plan revision.",
                ));
            }
            let created = if let Some(mut previous) = previous_operation {
                previous.status = Status::Rejected;
                previous.updated_at = timestamp.clone();
                previous.error = Some(GoFinError::new(
                    "PLAN_CANDIDATE_SUPERSEDED",
                    "financial_plan",
                    "This unsubmitted candidate was replaced before dispatch.",
                    "Inspect the replacement attempt; submitted or uncertain attempts require reconciliation.",
                ));
                self.store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .replace_financial_plan_candidate(
                        plan_id,
                        &plan.revision.revision_id,
                        step_id,
                        &to_value(&initial_plan)?,
                        &updated_plan,
                        &previous.id,
                        &to_value(&previous)?,
                        &id,
                        &execution_request.request_id,
                        &fingerprint,
                        &request.operation,
                        &to_value(&operation)?,
                        &timestamp,
                    )
                    .map_err(map_store)?
            } else {
                let mut store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                if let Some(claim) = bridge_claim.as_ref() {
                    store
                        .create_financial_plan_bridge_attempt(
                            plan_id,
                            &plan.revision.revision_id,
                            step_id,
                            &to_value(&initial_plan)?,
                            &updated_plan,
                            &id,
                            &execution_request.request_id,
                            &fingerprint,
                            &request.operation,
                            &to_value(&operation)?,
                            &timestamp,
                            &claim.transfer_id,
                            &claim.bridge_step_id,
                            &claim.kind,
                            &claim.expected_transfer,
                            &claim.updated_transfer,
                        )
                        .map_err(map_store)?
                } else {
                    store
                        .create_financial_plan_attempt(
                            plan_id,
                            &plan.revision.revision_id,
                            step_id,
                            &to_value(&initial_plan)?,
                            &updated_plan,
                            &id,
                            &execution_request.request_id,
                            &fingerprint,
                            &request.operation,
                            &to_value(&operation)?,
                            &timestamp,
                        )
                        .map_err(map_store)?
                }
            };
            if !created.created {
                return from_value(created.record);
            }

            let result = self
                .execute_new(
                    adapter,
                    &execution_request,
                    &mut operation,
                    None,
                    Some(plan.clone()),
                    Some((prepared, preparation_stamp)),
                    super::ExecutionRoute::Financial,
                )
                .await;
            return match result {
                Ok(record) => {
                    match Box::pin(self.retry_verified_non_dispatch_expiry(
                        &record,
                        plan_id,
                        step_id,
                        logical_request_id,
                        refresh_count,
                    ))
                    .await?
                    {
                        VerifiedExpiryRetry::NotEligible => Ok(record),
                        VerifiedExpiryRetry::Stop(terminal) => Ok(*terminal),
                        VerifiedExpiryRetry::Refresh => {
                            emit_candidate_refresh(
                                record.error.as_ref(),
                                gofin_domain::execution_progress::RefreshPhase::VerifiedNonDispatch,
                                refresh_count,
                            );
                            request = request_with_child_id(request);
                            replacement = None;
                            refresh_count += 1;
                            continue;
                        }
                    }
                }
                Err(error) => {
                    let error = self.plan_candidate_recovery(error, plan_id, Some(&operation));
                    let error = if can_auto_refresh_plan_candidate(&error)
                        && refresh_count >= MAX_PLAN_CANDIDATE_REFRESHES
                    {
                        with_plan_refresh_details(error, refresh_count)
                    } else {
                        error
                    };
                    if !operation.status.terminal() {
                        operation.status = if operation.submission.is_some() {
                            Status::Unknown
                        } else {
                            status_for_error(&error)
                        };
                        operation.error = Some(error.clone());
                        operation.updated_at = self.clock.now().to_rfc3339();
                        if operation.status.terminal() {
                            self.commit_terminal(
                                operation.clone(),
                                json!(null),
                                json!([]),
                                None,
                                None,
                            )?;
                        } else {
                            self.update_record(&operation)?;
                        }
                    }
                    if operation.submission.is_some() && !operation.status.terminal() {
                        // Recover only the immutable admitted attempt. Even a
                        // proved failure must not enter candidate write retries.
                        return self.reconcile_serial(operation.id.clone()).await;
                    }
                    let retry_candidate = can_auto_refresh_plan_candidate(&error)
                        && operation.status.terminal()
                        && operation.submission.is_none()
                        && operation.dispatch_evidence.is_none()
                        && operation.effects.is_none()
                        && self
                            .store
                            .lock()
                            .map_err(|_| internal("store lock poisoned"))?
                            .submission(&operation.id)
                            .map_err(map_store)?
                            .is_none()
                        && refresh_count < MAX_PLAN_CANDIDATE_REFRESHES
                        && self.plan_authority_active(plan_id)?;
                    if retry_candidate {
                        self.verify_persisted_unsubmitted_terminal(&operation)?;
                        emit_candidate_refresh(
                            Some(&error),
                            gofin_domain::execution_progress::RefreshPhase::Revalidation,
                            refresh_count,
                        );
                        wait_for_rpc_candidate_cooldown(&error).await;
                        request = request_with_child_id(request);
                        replacement = None;
                        refresh_count += 1;
                        continue;
                    }
                    Ok(operation)
                }
            };
        }
    }

    async fn retry_verified_non_dispatch_expiry(
        &self,
        operation: &OperationRecord,
        plan_id: &str,
        step_id: &str,
        logical_request_id: &str,
        refresh_count: usize,
    ) -> Result<VerifiedExpiryRetry> {
        if operation.status != Status::Failed
            || operation.submission.is_none()
            || operation.dispatch_evidence.is_none()
            || operation
                .effects
                .as_ref()
                .is_none_or(|effects| !effects.effects.is_empty())
        {
            return Ok(VerifiedExpiryRetry::NotEligible);
        }

        let (adapter, _) = self.resolve(&operation.operation)?;
        let attestation = match adapter.verify_non_dispatch_expiry(operation).await {
            Ok(Some(attestation)) => attestation,
            Ok(None) | Err(_) => return Ok(VerifiedExpiryRetry::NotEligible),
        };
        let Some(effects) = operation.effects.as_ref() else {
            return Ok(VerifiedExpiryRetry::NotEligible);
        };
        let Some(submission) = operation.submission.as_ref() else {
            return Ok(VerifiedExpiryRetry::NotEligible);
        };
        if attestation.operation_id() != operation.id
            || attestation.submission() != submission
            || attestation.effects_digest() != effects.proof_digest
            || effects.submission != *submission
            || effects.proof_digest != gofin_domain::verified_effect_digest(effects)?
            || !effects.effects.is_empty()
        {
            return Ok(VerifiedExpiryRetry::NotEligible);
        }

        let plan = self.load_financial_plan(plan_id)?;
        let owner = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .financial_plan_attempt_owner(&operation.id)
            .map_err(map_store)?;
        if owner
            .as_ref()
            .is_none_or(|(owner_plan, revision, owner_step, _)| {
                owner_plan != plan_id
                    || revision != &plan.revision.revision_id
                    || owner_step != step_id
            })
        {
            return Ok(VerifiedExpiryRetry::NotEligible);
        }
        let durable_match = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            let stored_submission = store.submission(&operation.id).map_err(map_store)?;
            store.get_record(&operation.id).map_err(map_store)? == Some(to_value(operation)?)
                && stored_submission.as_ref().is_some_and(|stored| {
                    stored.kind == submission.kind
                        && stored.reference == submission.reference
                        && stored.data == submission.data
                })
        };
        if !durable_match {
            return Ok(VerifiedExpiryRetry::NotEligible);
        }

        let active = self.plan_authority_active(plan_id)?;
        if refresh_count >= MAX_PLAN_CANDIDATE_REFRESHES || !active {
            let exhausted = refresh_count >= MAX_PLAN_CANDIDATE_REFRESHES;
            let mut error = plan_error(
                if exhausted {
                    "PLAN_CANDIDATE_REFRESH_EXHAUSTED"
                } else {
                    "PLAN_AUTHORIZATION_INACTIVE"
                },
                if exhausted {
                    "The expired transaction was proven not dispatched, and the bounded candidate refresh limit is exhausted."
                } else {
                    "The expired transaction was proven not dispatched, but the named plan authority is no longer active."
                },
                if active && exhausted {
                    "Inspect the plan status and resolve the repeated expiry before trying again. Do not restart the exhausted refresh budget with new request IDs while conditions are unchanged. The existing plan may still authorize a new candidate after the cause is resolved."
                } else {
                    "Authorize a replacement plan revision before preparing another candidate."
                },
            );
            let mut details = error
                .details
                .as_deref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            details.extend(
                json!({
                    "refresh_count": refresh_count,
                    "refresh_limit": MAX_PLAN_CANDIDATE_REFRESHES,
                    "refresh_exhausted": exhausted,
                    "non_dispatch_proven": true,
                    "candidate_expiry_kind": match attestation.kind() {
                        gofin_domain::CandidateExpiryKind::Quote => "quote",
                        gofin_domain::CandidateExpiryKind::TransactionLifetime => "transaction_lifetime",
                    },
                    "reauthorization_required": !active,
                    "logical_request_id": logical_request_id,
                })
                .as_object()
                .expect("error details are an object")
                .clone(),
            );
            error.details = Some(Box::new(Value::Object(details)));
            let mut terminal = operation.clone();
            terminal.error = Some(error);
            terminal.updated_at = self.clock.now().to_rfc3339();
            self.update_record(&terminal)?;
            return Ok(VerifiedExpiryRetry::Stop(Box::new(terminal)));
        }
        Ok(VerifiedExpiryRetry::Refresh)
    }

    /// Explain candidate freshness/transport recovery using current authority.
    /// This guidance never retries, renews or changes financial state.
    fn plan_candidate_recovery(
        &self,
        mut error: GoFinError,
        plan_id: &str,
        attempt: Option<&OperationRecord>,
    ) -> GoFinError {
        if !matches!(
            error.code.as_str(),
            "PREVIEW_EXPIRED"
                | "PREVIEW_CHANGED"
                | "REQUEST_CHANGED"
                | "PLAN_CANDIDATE_CHANGED"
                | "BINDING_CHANGED"
                | "NONCE_CHANGED"
                | "SOLANA_BLOCKHASH_EXPIRED"
                | "SOLANA_TX_EXPIRED"
                | "JUPITER_RFQ_EXPIRED"
                | "BRIDGE_QUOTE_EXPIRED"
                | "COST_VALUATION_STALE"
                | "STALE_VALUATION"
                | "PRICE_STALE"
                | "VALUE_BOUND_EXCEEDED"
                | "SLIPPAGE_LIMIT"
                | "EVIDENCE_TIMEOUT"
                | "RPC_READ_TIMEOUT"
                | "RPC_UNAVAILABLE"
                | "RPC_HISTORICAL_STATE_UNAVAILABLE"
                | "RPC_RATE_LIMITED"
        ) {
            return error;
        }
        let Ok(plan) = self.load_financial_plan(plan_id) else {
            return error;
        };
        let at = self.clock.now();
        let active = plan.revision.status == FinancialPlanAuthorizationStatus::Active
            && parse_time(&plan.revision.expires_at).is_ok_and(|expiry| expiry > at);
        let submitted = attempt.is_some_and(|attempt| {
            attempt.submission.is_some() || attempt.dispatch_evidence.is_some()
        });
        let other_attempt = plan
            .active_attempt_id
            .as_deref()
            .filter(|id| attempt.is_none_or(|attempt| attempt.id != *id));
        let reconcile = submitted || other_attempt.is_some();
        error.next_action = if reconcile {
            "Inspect this plan's status and reconcile the original attempt. Do not create another financial submission or renew consent merely to verify its outcome."
        } else if !active {
            "Nothing was submitted. Review the remaining work and authorize a replacement using --replaces with the current revision ID before starting a new attempt."
        } else if attempt.is_some() {
            "Nothing was submitted. Prepare a fresh candidate under the same plan using a new attempt ID. Fresh Touch ID is unnecessary while the plan remains active and the candidate stays within its approved scope."
        } else {
            "Nothing was submitted or recorded as an attempt. Retry preparation under the same plan; the request ID can be reused. Fresh Touch ID is unnecessary while the plan remains active and the candidate stays within its approved scope."
        }
        .into();
        let mut details = error
            .details
            .as_deref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        details.extend(
            json!({
                "plan_id": plan_id,
                "revision_id": plan.revision.revision_id,
                "observed_at": at.to_rfc3339(),
                "attempt_id": other_attempt.or_else(|| attempt.map(|attempt| attempt.id.as_str())),
                "reconciliation_required": reconcile,
                "reauthorization_required": !reconcile && !active,
                "new_attempt_required": !reconcile && attempt.is_some(),
                "submission_recorded": submitted,
            })
            .as_object()
            .expect("recovery details are an object")
            .clone(),
        );
        if !reconcile
            && active
            && rpc_candidate_cooldown(&error).is_some_and(|delay| delay > MAX_PLAN_RPC_RETRY_WAIT)
        {
            error.next_action = "Nothing was submitted. The reported RPC cooldown exceeds the automatic wait limit. Wait for that cooldown before retrying preparation under the same active plan; do not retry early or change the financial scope.".into();
            details.insert(
                "automatic_retry_wait_limit_ms".into(),
                json!(MAX_PLAN_RPC_RETRY_WAIT.as_millis()),
            );
        }
        if error.code == "COST_VALUATION_STALE" && !reconcile && active {
            error.next_action = "Nothing was submitted. Inspect the reported cost valuation timestamp and its origin before retrying. A current quote can be refreshed; immutable historical producer evidence cannot be repaired by fetching new quotes. Preserve this active plan and its saved transfer IDs; do not reset the candidate retry budget or request new consent solely for this error.".into();
            details.insert("automatic_retry_supported".into(), json!(false));
            details.insert(
                "recovery_class".into(),
                json!("cost_evidence_requires_inspection"),
            );
        }
        error.details = Some(Box::new(Value::Object(details)));
        error
    }

    fn plan_authority_active(&self, plan_id: &str) -> Result<bool> {
        let plan = self.load_financial_plan(plan_id)?;
        Ok(
            plan.revision.status == FinancialPlanAuthorizationStatus::Active
                && parse_time(&plan.revision.expires_at)? > self.clock.now(),
        )
    }

    async fn retry_plan_preparation_error(
        &self,
        error: GoFinError,
        plan_id: &str,
        refresh_count: usize,
    ) -> Result<()> {
        let error = self.plan_candidate_recovery(error, plan_id, None);
        if can_auto_refresh_plan_candidate(&error)
            && refresh_count < MAX_PLAN_CANDIDATE_REFRESHES
            && self.plan_authority_active(plan_id)?
        {
            emit_candidate_refresh(
                Some(&error),
                gofin_domain::execution_progress::RefreshPhase::Preparation,
                refresh_count,
            );
            wait_for_rpc_candidate_cooldown(&error).await;
            // The caller continues its existing loop. Boxing a recursive
            // executor future still nests its poll frames on each refresh.
            return Ok(());
        }
        Err(with_plan_refresh_details(error, refresh_count))
    }

    fn verify_persisted_unsubmitted_terminal(&self, operation: &OperationRecord) -> Result<()> {
        if !operation.status.terminal()
            || operation.submission.is_some()
            || operation.dispatch_evidence.is_some()
            || operation.effects.is_some()
        {
            return Err(plan_error(
                "PLAN_CANDIDATE_NOT_RETRYABLE",
                "The failed candidate has evidence that prevents automatic refresh.",
                "Reconcile the original attempt before continuing.",
            ));
        }
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        if store
            .submission(&operation.id)
            .map_err(map_store)?
            .is_some()
        {
            return Err(plan_error(
                "PLAN_CANDIDATE_NOT_RETRYABLE",
                "A durable submission identity exists for this candidate.",
                "Reconcile the original attempt before continuing.",
            ));
        }
        let persisted = store
            .get_record(&operation.id)
            .map_err(map_store)?
            .ok_or_else(|| internal("terminal candidate record disappeared"))?;
        if persisted != to_value(operation)?
            || persisted
                .get("submission")
                .is_some_and(|value| !value.is_null())
            || persisted
                .get("dispatch_evidence")
                .is_some_and(|value| !value.is_null())
            || persisted
                .get("effects")
                .is_some_and(|value| !value.is_null())
        {
            return Err(plan_error(
                "PLAN_CANDIDATE_NOT_RETRYABLE",
                "The durable candidate record does not prove a clean pre-submission failure.",
                "Reconcile the original attempt before continuing.",
            ));
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    async fn execute_financial_plan_read(
        &self,
        plan_id: &str,
        step_id: &str,
        request: OperationRequest,
        mut plan: FinancialPlanAuthorizationRecord,
        initial_plan: FinancialPlanAuthorizationRecord,
        previous_operation: Option<OperationRecord>,
        adapter: std::sync::Arc<dyn gofin_domain::Adapter>,
    ) -> Result<OperationRecord> {
        let permission = permission_for_step(&plan.revision, step_id)?;
        let Some(exact) = &permission.exact else {
            return Err(plan_error(
                "PLAN_PERMISSION_INVALID",
                "Read steps require a generic exact scope.",
                "Preview the plan with an exact read scope.",
            ));
        };
        if !exact.scope.maximum_debits.is_empty()
            || !exact.scope.maximum_costs.is_empty()
            || !exact.scope.possible_credits.is_empty()
        {
            return Err(plan_error(
                "PLAN_READ_SCOPE_INVALID",
                "A read step cannot declare financial debits, costs, or credits.",
                "Remove financial bounds and credit identities from the read scope.",
            ));
        }
        let definition = adapter
            .definitions()
            .into_iter()
            .find(|definition| definition.name == request.operation)
            .ok_or_else(|| internal("registry definition disappeared"))?;
        validate_exact_plan_identity(adapter.as_ref(), &request.operation, exact)?;
        if !plan.revision.spec.steps.iter().any(|step| {
            step.step_id == step_id
                && step.operation == request.operation
                && step.input == request.input
        }) {
            return Err(plan_error(
                "PLAN_STEP_SCOPE_MISMATCH",
                "Read input differs from the exact approved plan input.",
                "Submit the exact input in the active plan revision.",
            ));
        }
        let fingerprint = binding_hash(&json!({
            "plan_id": plan_id,
            "step_id": step_id,
            "request": request_fingerprint(&request)?,
        }))?;
        let id = Uuid::new_v4().to_string();
        let timestamp = self.clock.now().to_rfc3339();
        let mut operation = OperationRecord {
            id: id.clone(),
            request_id: request.request_id.clone(),
            fingerprint: fingerprint.clone(),
            fingerprint_version: 1,
            operation: request.operation.clone(),
            input: request.input.clone(),
            status: Status::AwaitingApproval,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
            preview: None,
            approval: Some(plan.approval.clone()),
            authorization_source: None,
            submission: None,
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        plan.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: plan_id.into(),
            revision_id: plan.revision.revision_id.clone(),
            step_id: step_id.into(),
            attempt_id: id.clone(),
            request_fingerprint: fingerprint.clone(),
            execution_binding_hash: binding_hash(&json!({
                "kind":"financial_plan_read_step",
                "plan_id":plan_id,
                "revision_id":plan.revision.revision_id,
                "step_id":step_id,
                "operation":request.operation,
                "input":request.input,
                "definition_hash":exact.definition_hash,
                "connections_hash":exact.connections_hash,
            }))?,
            logical_request_id: None,
            submission_id: None,
            result: None,
        });
        plan.active_attempt_id = Some(id.clone());
        let updated_plan = to_value(&plan)?;
        let created = if let Some(mut previous) = previous_operation {
            previous.status = Status::Rejected;
            previous.updated_at = timestamp.clone();
            previous.error = Some(GoFinError::new(
                "PLAN_CANDIDATE_SUPERSEDED",
                "financial_plan",
                "This unsubmitted read candidate was replaced before execution.",
                "Inspect the replacement attempt.",
            ));
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .replace_financial_plan_candidate(
                    plan_id,
                    &plan.revision.revision_id,
                    step_id,
                    &to_value(&initial_plan)?,
                    &updated_plan,
                    &previous.id,
                    &to_value(&previous)?,
                    &id,
                    &request.request_id,
                    &fingerprint,
                    &request.operation,
                    &to_value(&operation)?,
                    &timestamp,
                )
                .map_err(map_store)?
        } else {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .create_financial_plan_attempt(
                    plan_id,
                    &plan.revision.revision_id,
                    step_id,
                    &to_value(&initial_plan)?,
                    &updated_plan,
                    &id,
                    &request.request_id,
                    &fingerprint,
                    &request.operation,
                    &to_value(&operation)?,
                    &timestamp,
                )
                .map_err(map_store)?
        };
        if !created.created {
            return from_value(created.record);
        }
        let result = match adapter.read(&request.operation, &request.input).await {
            Ok(output) => {
                validate_plan_json_schema(&definition.output_schema, &output).map(|()| output)
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(output) => {
                operation.status = Status::Completed;
                // The operation record is the durable response-loss recovery
                // record. Adapter read output never creates financial effects.
                operation.last_reconciliation_evidence = Some(json!({"read_result": output}));
                if let Some(linkage) = plan
                    .attempts
                    .iter_mut()
                    .find(|linkage| linkage.attempt_id == id)
                {
                    linkage.result = Some(output);
                }
            }
            Err(error) => {
                operation.status = Status::Failed;
                operation.error = Some(error);
            }
        }
        operation.updated_at = self.clock.now().to_rfc3339();
        loop {
            // Hold the process-local store mutex across the final CAS so a
            // concurrent revoke on this Engine cannot slip between reading
            // the authorization state and persisting the read result.
            let mut store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            let current_value = store
                .financial_plan(plan_id)
                .map_err(map_store)?
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_NOT_FOUND",
                        "The authorized plan no longer exists.",
                        "Inspect plan status and recover the original read attempt.",
                    )
                })?;
            let current: FinancialPlanAuthorizationRecord = from_value(current_value)?;
            if current.active_attempt_id.as_deref() != Some(id.as_str()) {
                drop(store);
                let persisted = self.load_record(&id)?;
                if persisted.status.terminal() {
                    return Ok(persisted);
                }
                return Err(plan_error(
                    "PLAN_ATTEMPT_OWNERSHIP_LOST",
                    "The read attempt no longer owns the plan's active slot.",
                    "Inspect plan status and recover the original attempt.",
                ));
            }
            let mut settled_plan = current.clone();
            if let Some(updated_linkage) = settled_plan
                .attempts
                .iter_mut()
                .find(|linkage| linkage.attempt_id == id)
            {
                updated_linkage.result = operation
                    .last_reconciliation_evidence
                    .as_ref()
                    .and_then(|evidence| evidence.get("read_result"))
                    .cloned();
            }
            settled_plan.active_attempt_id = None;
            let persisted = store.finish_financial_plan_read(
                plan_id,
                &to_value(&current)?,
                &to_value(&settled_plan)?,
                &id,
                &to_value(&operation)?,
                operation.status == Status::Completed,
                &operation.updated_at,
            );
            drop(store);
            match persisted {
                Ok(()) => break,
                // Revocation or expiry may update authorization state after
                // the read result is produced but before this transaction.
                // Reload and retry while this attempt still owns the slot so
                // the read result settles without restoring stale authority.
                Err(StoreError::ConcurrentReservation) => continue,
                Err(error) => return Err(map_store(error)),
            }
        }
        Ok(operation)
    }

    fn validate_plan_request(&self, request: &FinancialPlanAuthorizationRequest) -> Result<()> {
        request.validate()?;
        if request.request_id.trim().is_empty() || request.valid_for_seconds == Some(0) {
            return Err(plan_error(
                "PLAN_REQUEST_INVALID",
                "A request ID and positive plan duration are required.",
                "Supply a stable request ID and a positive representable duration.",
            ));
        }
        if let Some(seconds) = request.valid_for_seconds {
            let duration = plan_duration(seconds)?;
            self.clock
                .now()
                .checked_add_signed(duration)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_DURATION_INVALID",
                        "Plan expiry cannot be represented from the current time.",
                        "Choose a shorter consent duration.",
                    )
                })?;
        }
        Ok(())
    }

    fn resolve_plan_duration(
        &self,
        request: &FinancialPlanAuthorizationRequest,
        prior: Option<&FinancialPlanAuthorizationRecord>,
    ) -> Result<(u64, Vec<GoFinError>)> {
        let prior_status = prior
            .map(|record| self.build_plan_status(record, &record.revision))
            .transpose()?;
        let mut allowances = Vec::new();
        let mut unclassified = Vec::new();
        for step in &request.plan.steps {
            if prior_status.as_ref().is_some_and(|status| {
                status.steps.iter().any(|progress| {
                    progress.step_id == step.step_id
                        && progress.state == FinancialPlanStepProgressState::Completed
                })
            }) {
                continue;
            }
            let (adapter, _) = self.resolve(&step.operation)?;
            match adapter.plan_step_allowance(&step.operation, &step.input)? {
                Some(seconds) => allowances.push(seconds),
                None => unclassified.push(step.step_id.clone()),
            }
        }
        if !unclassified.is_empty() && request.valid_for_seconds.is_none() {
            return Err(plan_error("PLAN_DURATION_REQUIRED", "This plan includes a resting, delayed, or unclassified stage whose lifetime must be selected explicitly.",
                "Supply --valid-for with a horizon covering the entire remaining sequence, including dependency settlement.")
                .with_details(json!({"step_ids":unclassified,"explicit_duration_required":true})));
        }
        let budget = gofin_domain::plan_timing::sequential_budget(allowances)?;
        let seconds = request.valid_for_seconds.unwrap_or(budget);
        for step in &request.plan.steps {
            let completed = prior_status.as_ref().is_some_and(|status| {
                status.steps.iter().any(|progress| {
                    progress.step_id == step.step_id
                        && progress.state == FinancialPlanStepProgressState::Completed
                })
            });
            if !completed
                && step
                    .action_deadline_seconds
                    .is_some_and(|window| window >= seconds)
            {
                return Err(plan_error("PLAN_DURATION_CANNOT_FIT_ACTION_WINDOW", "The plan lifetime leaves no orchestration time around a full approved action window.",
                    "Select an explicit longer --valid-for or review a shorter relative action window before requesting native approval.")
                    .with_details(json!({"step_id":step.step_id,"authorization_seconds":seconds,"required_action_window_seconds":step.action_deadline_seconds,"automatic_duration":request.valid_for_seconds.is_none()})));
            }
        }
        plan_duration(seconds)?;
        let warnings = if seconds < budget {
            vec![plan_error("PLAN_DURATION_BELOW_BUDGET", "The explicit duration is shorter than the known sequential planning allowance.",
                "Review the full workflow horizon; keep the explicit constraint or choose a longer duration before approval.")
                .with_details(json!({"requested_seconds":seconds,"known_budget_seconds":budget,"unclassified_step_ids":unclassified}))]
        } else {
            Vec::new()
        };
        Ok((seconds, warnings))
    }

    fn prepare_action_deadline(
        &self,
        adapter: &dyn Adapter,
        revision: &FinancialPlanRevision,
        step_id: &str,
        input: &Value,
    ) -> Result<Option<gofin_domain::plan_timing::ResolvedActionDeadline>> {
        let step = revision
            .spec
            .steps
            .iter()
            .find(|step| step.step_id == step_id)
            .ok_or_else(|| internal("plan step missing"))?;
        let definition = adapter
            .definitions()
            .into_iter()
            .find(|d| d.name == step.operation)
            .ok_or_else(|| internal("plan operation missing"))?;
        gofin_domain::plan_timing::plan_input_schema(
            &definition,
            input,
            step.action_deadline_seconds,
        )?;
        let Some(policy) = gofin_domain::plan_timing::relative_deadline_policy(
            &definition,
            step.action_deadline_seconds,
        )?
        else {
            gofin_domain::authority_expiry::validate_plan_input_deadlines(
                &definition,
                input,
                &revision.expires_at,
                self.clock.now(),
            )?;
            return Ok(None);
        };
        gofin_domain::plan_timing::resolve_action_deadline(
            &policy,
            step.action_deadline_seconds.expect("relative policy"),
            self.clock.now(),
            &revision.expires_at,
        )
        .map(Some)
        .map_err(|mut error| {
            if let Some(details) = error
                .details
                .as_mut()
                .and_then(|details| details.as_object_mut())
            {
                details.insert("plan_id".into(), json!(revision.plan_id));
                details.insert("revision_id".into(), json!(revision.revision_id));
                details.insert("step_id".into(), json!(step_id));
            }
            error
        })
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

    /// Admission is independent of future balances and precedes native approval.
    /// A bounded batch overlaps independent venue reads without changing step order.
    async fn validate_plan_feasibility(
        &self,
        spec: &financial_plans::FinancialPlanSpec,
        prior: Option<&FinancialPlanAuthorizationRecord>,
        completed_steps: &[String],
    ) -> Result<Vec<String>> {
        let mut pending = Vec::new();
        for step in &spec.steps {
            if completed_steps
                .iter()
                .any(|completed| completed == &step.step_id)
                && prior.is_some_and(|p| p.revision.spec.steps.iter().any(|old| old == step))
            {
                continue;
            }
            let (adapter, kind) = self.resolve(&step.operation)?;
            // Adapter permission preparation already validates its own scope.
            if kind != OperationKind::Write || step.permitted_variations["mode"] != "exact" {
                continue;
            }
            pending.push((
                step.step_id.clone(),
                step.operation.clone(),
                step.input.clone(),
                from_value::<FinancialPlanExactScope>(step.permitted_variations.clone())?,
                adapter,
            ));
        }
        let mut warnings = Vec::new();
        for chunk in pending.chunks(4) {
            let mut tasks = tokio::task::JoinSet::new();
            for (index, (step_id, operation, input, exact_scope, adapter)) in
                chunk.iter().cloned().enumerate()
            {
                tasks.spawn(async move {
                    let result = bounded_evidence(
                        &operation,
                        "plan_feasibility",
                        PREFLIGHT_TIMEOUT,
                        adapter.validate_plan_feasibility(&operation, &input, Some(&exact_scope)),
                    )
                    .await
                    .map_err(|mut error| {
                        let mut details = error
                            .details
                            .as_deref()
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        details.insert("step_id".into(), json!(step_id));
                        details.insert("operation".into(), json!(operation));
                        details.insert("before_plan_approval".into(), json!(true));
                        error.details = Some(Box::new(Value::Object(details)));
                        error
                    })?;
                    Ok::<_, GoFinError>((
                        index,
                        result
                            .into_iter()
                            .map(|warning| format!("{step_id}: {warning}"))
                            .collect::<Vec<_>>(),
                    ))
                });
            }
            let mut rows = Vec::new();
            while let Some(result) = tasks.join_next().await {
                rows.push(result.map_err(|_| internal("plan feasibility task failed"))??);
            }
            rows.sort_by_key(|(index, _)| *index);
            warnings.extend(rows.into_iter().flat_map(|(_, warnings)| warnings));
        }
        Ok(warnings)
    }

    async fn prepare_plan_permissions(
        &self,
        spec: &financial_plans::FinancialPlanSpec,
        prior: Option<&FinancialPlanAuthorizationRecord>,
        completed_steps: &[String],
    ) -> Result<Vec<FinancialPlanStepPermission>> {
        let mut permissions = Vec::new();
        for step in &spec.steps {
            // Completed steps are archival history. Reusing their approved
            // permission keeps an adapter metadata update from invalidating a
            // valid suffix-only revision.
            if let Some(prior) = prior
                && prior.revision.spec.steps.iter().any(|old| {
                    old == step
                        && completed_steps
                            .iter()
                            .any(|completed| completed == &step.step_id)
                })
                && let Some(permission) = prior
                    .revision
                    .permissions
                    .iter()
                    .find(|permission| permission.step_id == step.step_id)
            {
                permissions.push(permission.clone());
                continue;
            }
            let (adapter, kind) = self.resolve(&step.operation)?;
            let definition = adapter
                .definitions()
                .into_iter()
                .find(|definition| definition.name == step.operation)
                .ok_or_else(|| internal("registry definition disappeared"))?;
            let is_read = kind == OperationKind::Read;
            if !is_read && kind != OperationKind::Write {
                return Err(plan_error(
                    "PLAN_OPERATION_UNSUPPORTED",
                    "Plans may include only agent-invocable reads and financial writes.",
                    "Remove configuration, lifecycle, human-interactive, or unsupported operations from this plan.",
                ));
            }
            if !plan_step_actor_allowed(kind, &definition.execution_actor) {
                return Err(plan_error(
                    "PLAN_OPERATION_UNSUPPORTED",
                    "This operation is not available to an agent as a plan step.",
                    "Choose an agent-readable read or agent-invocable financial write.",
                ));
            }
            let template_schema = gofin_domain::plan_timing::plan_input_schema(
                &definition,
                &step.input,
                step.action_deadline_seconds,
            )?;
            validate_plan_json_schema(&template_schema, &step.input)?;
            if !step.funding_dependencies.is_empty() {
                adapter
                    .validate_plan_funding_dependencies(
                        &step.operation,
                        &step.input,
                        &step.funding_dependencies,
                    )
                    .map_err(|mut error| {
                        let mut details = error
                            .details
                            .as_deref()
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        details.insert("step_id".into(), json!(step.step_id));
                        details.insert("operation".into(), json!(step.operation));
                        details.insert("before_plan_approval".into(), json!(true));
                        error.details = Some(Box::new(Value::Object(details)));
                        error
                    })?;
            }
            let generated_input_fields =
                generated_attempt_input_fields(&definition, &step.input, is_read)?;
            validate_generated_attempt_input_schema(
                &template_schema,
                &step.input,
                &generated_input_fields,
                &generated_attempt_input_value(
                    "plan-review",
                    "revision-review",
                    &step.step_id,
                    "attempt-review",
                )?,
            )?;
            let exact_requested =
                step.permitted_variations.get("mode") == Some(&Value::String("exact".into()));
            if exact_requested {
                if definition.semantics["plan_permission"]["requires_adapter_scope"] == true {
                    return Err(plan_error(
                        "PLAN_APPLICATION_SCOPE_REQUIRED",
                        "This operation requires its application-reviewed plan bounds, including risk and cumulative authority.",
                        "Use the operation's discovered plan_permission bounds instead of generic exact mode.",
                    ));
                }
                validate_plan_json_schema(&template_schema, &step.input)?;
                let scope: FinancialPlanExactScope = serde_json::from_value(
                    step.permitted_variations.clone(),
                )
                .map_err(|_| {
                    plan_error(
                        "PLAN_EXACT_SCOPE_INVALID",
                        "The exact plan scope is malformed.",
                        "Declare mode, maximum_debits, maximum_costs, and possible_credits with exact identities.",
                    )
                })?;
                validate_exact_scope(&scope)?;
                validate_exact_coinbase_venue_fee_policy(
                    &step.operation,
                    &step.input,
                    &scope,
                    &spec.aggregate_bounds,
                    &step.funding_dependencies,
                )?;
                if is_read
                    && (!scope.maximum_debits.is_empty()
                        || !scope.maximum_costs.is_empty()
                        || !scope.possible_credits.is_empty()
                        || !step.completion.operation_succeeded
                        || step.completion.principal_spend.is_some()
                        || step.completion.minimum_net_receipt.is_some()
                        || !step.funding_dependencies.is_empty())
                {
                    return Err(plan_error(
                        "PLAN_READ_SCOPE_INVALID",
                        "Read steps cannot carry financial caps, funding, or output identities and must require verified success.",
                        "Use operation_succeeded completion with empty financial scope and no funding dependencies.",
                    ));
                }
                let mut definition_hash = financial_plans::exact_plan_definition_hash(&definition)?;
                let connections_hash = stable_connection_hash(&adapter.authorization_identities())?;
                // Preserve a started revision's historical hash spelling only
                // after proving the current effective contract and connection
                // are identical. Revision scope checks still compare all bounds.
                if let Some(prior) = prior
                    && prior
                        .revision
                        .spec
                        .steps
                        .iter()
                        .any(|old| old.step_id == step.step_id && old.operation == step.operation)
                    && let Some(old) = prior
                        .revision
                        .permissions
                        .iter()
                        .find(|old| old.step_id == step.step_id)
                        .and_then(|old| old.exact.as_ref())
                    && old.connections_hash == connections_hash
                    && financial_plans::exact_plan_definition_hash_matches(
                        &definition,
                        &old.definition_hash,
                    )?
                {
                    definition_hash = old.definition_hash.clone();
                }
                permissions.push(FinancialPlanStepPermission {
                    step_id: step.step_id.clone(),
                    generated_input_fields,
                    exact: Some(FinancialPlanExactPermission {
                        scope,
                        definition_hash,
                        connections_hash,
                    }),
                    permission: None,
                });
            } else {
                if is_read {
                    return Err(plan_error(
                        "PLAN_PERMISSION_UNSUPPORTED",
                        "Read steps require an exact operation scope.",
                        "Set permitted_variations.mode to exact and declare step caps.",
                    ));
                }
                let mut permission = bounded_evidence(
                    &step.operation,
                    "plan_review",
                    PREFLIGHT_TIMEOUT,
                    adapter.prepare_plan_permission(
                        &step.operation,
                        &step.input,
                        &step.permitted_variations,
                    ),
                )
                .await?;
                if let Some(window) = step.action_deadline_seconds
                    && permission
                        .spec
                        .parameters
                        .get("max_deadline_seconds")
                        .and_then(Value::as_u64)
                        .is_some_and(|limit| window > limit)
                {
                    return Err(plan_error(
                        "PLAN_ACTION_DEADLINE_INVALID",
                        "Relative action window exceeds the approved adapter deadline bound.",
                        "Use an action window within max_deadline_seconds.",
                    ));
                }
                if permission.spec.summary.trim().is_empty()
                    || permission.spec.fixed.is_null()
                    || permission.spec.parameters.is_null()
                {
                    return Err(plan_error(
                        "PLAN_PERMISSION_INVALID",
                        "The adapter returned incomplete plan permission scope.",
                        "Use an operation with complete adapter-reviewed permission metadata.",
                    ));
                }
                if permission.spec.maximum_costs.iter().any(|cost| {
                    cost.treatment == gofin_domain::CostTreatment::VenueCalculatedAdditionalDebit
                }) {
                    return Err(plan_error(
                        "PLAN_VENUE_CALCULATED_FEE_UNSUPPORTED",
                        "Venue-calculated uncapped fees require the exact Coinbase withdrawal scope.",
                        "Use exact mode with explicit Coinbase fee consent and a finite reserve.",
                    ));
                }
                if step.operation == "bridge.execute" {
                    let Some(scope) = permission.cumulative_scope.as_mut() else {
                        return Err(plan_error(
                            "PLAN_BRIDGE_CUMULATIVE_SCOPE_REQUIRED",
                            "A bridge intent must declare cumulative debit and cost limits for all route children.",
                            "Use an adapter-reviewed bridge.execute scope that covers the complete allowance, deposit and settlement workflow.",
                        ));
                    };
                    scope.scope_id = step.step_id.clone();
                }
                permissions.push(FinancialPlanStepPermission {
                    step_id: step.step_id.clone(),
                    exact: None,
                    generated_input_fields,
                    permission: Some(permission),
                });
            }
        }
        validate_plan_recovery_permissions(spec, &permissions)?;
        Ok(permissions)
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

    fn load_financial_plan_revision(
        &self,
        plan_id: &str,
        revision_id: &str,
    ) -> Result<FinancialPlanRevision> {
        let value = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .financial_plan_revision(plan_id, revision_id)
            .map_err(map_store)?
            .ok_or_else(|| {
                plan_error(
                    "PLAN_REVISION_NOT_FOUND",
                    "Plan revision does not exist.",
                    "Use the revision linked to the original request.",
                )
            })?;
        from_value(value)
    }

    fn plan_replacement(
        &self,
        request: &FinancialPlanAuthorizationRequest,
    ) -> Result<Option<FinancialPlanAuthorizationRecord>> {
        let Some(revision_id) = request.replaces_revision_id.as_deref() else {
            return Ok(None);
        };
        let owner = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .financial_plan_revision_owner(revision_id)
            .map_err(map_store)?
            .ok_or_else(|| {
                plan_error(
                    "PLAN_REVISION_NOT_FOUND",
                    "The revision selected for replacement does not exist.",
                    "Use the current revision ID shown by plan status.",
                )
            })?;
        let record = self.load_financial_plan(&owner.0)?;
        if record.revision.revision_id != revision_id
            || record.revision.status == FinancialPlanAuthorizationStatus::Superseded
            || record.active_attempt_id.is_some()
        {
            return Err(plan_error(
                "PLAN_REVISION_NOT_REPLACEABLE",
                "Only the current plan revision without an unresolved attempt can be renewed or revised.",
                "Reconcile the current attempt and use the active revision ID.",
            ));
        }
        Ok(Some(record))
    }

    fn validate_plan_step(
        &self,
        record: &FinancialPlanAuthorizationRecord,
        step_id: &str,
        operation: &str,
        input: &Value,
    ) -> Result<()> {
        if record.revision.status != FinancialPlanAuthorizationStatus::Active
            || parse_time(&record.revision.expires_at)? <= self.clock.now()
        {
            return Err(plan_error(
                "PLAN_AUTHORIZATION_INACTIVE",
                "Plan authorization is expired or revoked.",
                "Reconcile unresolved attempts, preview a replacement revision with --replaces REVISION_ID, and obtain fresh native approval before starting another step.",
            ).with_details(json!({"plan_id":record.revision.plan_id,"revision_id":record.revision.revision_id,"expires_at":record.revision.expires_at,
                "observed_at":self.clock.now().to_rfc3339(),"authorization_status":record.revision.status,
                "reauthorization_required":true,"reconciliation_required":record.active_attempt_id.is_some()})));
        }
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
                    "Use a step ID from the active plan revision.",
                )
            })?;
        if step.operation != operation {
            return Err(plan_error(
                "PLAN_STEP_SCOPE_MISMATCH",
                "Operation differs from the approved step.",
                "Use the operation named by the active plan revision.",
            ));
        }
        let permission = permission_for_step(&record.revision, step_id)?;
        if permission.exact.is_some() && step.input != *input {
            return Err(plan_error(
                "PLAN_STEP_SCOPE_MISMATCH",
                "Input differs from the exact operation authorized in this plan.",
                "Submit the original exact input or obtain fresh plan approval.",
            ));
        }
        if permission
            .generated_input_fields
            .iter()
            .any(|pointer| input.pointer(pointer) != step.input.pointer(pointer))
        {
            return Err(plan_error(
                "PLAN_STEP_SCOPE_MISMATCH",
                "A broker-generated identity field differs from the approved input template.",
                "Keep the identity field from the plan template; GoFin assigns its concrete value per attempt.",
            ));
        }
        if let Some(recovery) = permission
            .permission
            .as_ref()
            .and_then(|permission| permission.recovery_parent.as_ref())
            && (input.pointer(&recovery.operation_id_pointer)
                != step.input.pointer(&recovery.operation_id_pointer)
                || input.pointer(&recovery.submission_pointer)
                    != step.input.pointer(&recovery.submission_pointer)
                || recovery.parent_binding_fields.iter().any(|field| {
                    input.pointer(&field.child_input_pointer)
                        != step.input.pointer(&field.child_input_pointer)
                }))
        {
            return Err(plan_error(
                "PLAN_RECOVERY_PARENT_TAMPERED",
                "Recovery parent fields are fixed by the approved plan template.",
                "Use the approved cleanup step input; GoFin supplies the exact durable parent identity.",
            ));
        }
        let recovery_parent = permission
            .permission
            .as_ref()
            .and_then(|permission| permission.recovery_parent.as_ref());
        if record.active_attempt_id.is_some()
            && !recovery_parent.is_some_and(|parent| {
                self.recovery_parent_attempt(record, &parent.step_id)
                    .is_ok_and(|(id, _)| record.active_attempt_id.as_deref() == Some(id.as_str()))
            })
        {
            return Err(plan_error(
                "PLAN_ATTEMPT_UNRESOLVED",
                "A previous plan attempt is unresolved.",
                "Reconcile that attempt before executing another step.",
            ));
        }
        let status = self.build_plan_status(record, &record.revision)?;
        if status.next_eligible_step_id.as_deref() != Some(step_id) {
            return Err(plan_error(
                "PLAN_STEP_NOT_ELIGIBLE",
                "Only the first incomplete plan step can execute.",
                "Complete or reconcile earlier steps before continuing.",
            ));
        }
        Ok(())
    }

    fn plan_step_attempt_input(
        &self,
        record: &FinancialPlanAuthorizationRecord,
        step_id: &str,
        raw_input: &Value,
        attempt_id: &str,
    ) -> Result<Value> {
        let mut input = plan_attempt_input(&record.revision, step_id, raw_input, attempt_id)?;
        let Some(parent_scope) = permission_for_step(&record.revision, step_id)?
            .permission
            .as_ref()
            .and_then(|permission| permission.recovery_parent.as_ref())
        else {
            return Ok(input);
        };
        let (parent_id, parent_operation) =
            self.recovery_parent_attempt(record, &parent_scope.step_id)?;
        set_json_pointer(
            &mut input,
            &parent_scope.operation_id_pointer,
            Value::String(parent_id),
        )?;
        set_json_pointer(
            &mut input,
            &parent_scope.submission_pointer,
            to_value(parent_operation.submission.as_ref().ok_or_else(|| {
                recovery_parent_invalid("The recovery parent has no durable submission identity.")
            })?)?,
        )?;
        let binding = parent_operation
            .preview
            .as_ref()
            .map(|preview| &preview.binding)
            .ok_or_else(|| {
                recovery_parent_invalid("The recovery parent has no immutable prepared binding.")
            })?;
        for field in &parent_scope.parent_binding_fields {
            let value = binding
                .pointer(&field.parent_binding_pointer)
                .ok_or_else(|| {
                    recovery_parent_invalid(
                        "The recovery parent binding is missing an approved field.",
                    )
                })?;
            set_json_pointer(&mut input, &field.child_input_pointer, value.clone())?;
        }
        Ok(input)
    }

    fn recovery_parent_attempt(
        &self,
        record: &FinancialPlanAuthorizationRecord,
        parent_step_id: &str,
    ) -> Result<(String, OperationRecord)> {
        let child_index = record
            .revision
            .spec
            .steps
            .iter()
            .position(|step| {
                permission_for_step(&record.revision, step.step_id.as_str()).is_ok_and(
                    |permission| {
                        permission
                            .permission
                            .as_ref()
                            .and_then(|permission| permission.recovery_parent.as_ref())
                            .is_some_and(|parent| parent.step_id == parent_step_id)
                    },
                )
            })
            .ok_or_else(|| {
                recovery_parent_invalid("No approved recovery step references this parent.")
            })?;
        let parent_index = record
            .revision
            .spec
            .steps
            .iter()
            .position(|step| step.step_id == parent_step_id)
            .ok_or_else(|| {
                recovery_parent_invalid("The approved recovery parent step is missing.")
            })?;
        let matching = record
            .attempts
            .iter()
            .filter(|attempt| {
                attempt.step_id == parent_step_id
                    && attempt.revision_id == record.revision.revision_id
            })
            .collect::<Vec<_>>();
        let mut unresolved = Vec::new();
        for attempt in matching {
            let operation = self.load_record(&attempt.attempt_id)?;
            if matches!(
                operation.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                operation.submission.as_ref().ok_or_else(|| {
                    recovery_parent_invalid(
                        "The recovery parent has no durable submission identity.",
                    )
                })?;
                operation.preview.as_ref().ok_or_else(|| {
                    recovery_parent_invalid(
                        "The recovery parent has no immutable prepared binding.",
                    )
                })?;
                unresolved.push((operation.id.clone(), operation));
            }
        }
        if unresolved.len() != 1 {
            return Err(recovery_parent_invalid(
                "The recovery parent must resolve to exactly one submitted unresolved attempt.",
            ));
        }
        if parent_index >= child_index {
            return Err(recovery_parent_invalid(
                "The recovery parent must be an earlier step in the same plan revision.",
            ));
        }
        Ok(unresolved.remove(0))
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

fn validate_plan_operation_identity(
    adapter: &dyn gofin_domain::Adapter,
    revision: &FinancialPlanRevision,
    step_id: &str,
) -> Result<()> {
    let permission = permission_for_step(revision, step_id)?;
    let Some(exact) = &permission.exact else {
        return Ok(());
    };
    let step = revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| internal("plan step disappeared"))?;
    validate_exact_plan_identity(adapter, &step.operation, exact)
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

fn emit_candidate_refresh(
    error: Option<&GoFinError>,
    phase: gofin_domain::execution_progress::RefreshPhase,
    completed_refreshes: usize,
) {
    use gofin_domain::execution_progress::{self, CandidateRefresh, RefreshReason};
    let reason = error.and_then(|error| RefreshReason::from_code(&error.code));
    let reason = reason.or_else(|| {
        (phase == execution_progress::RefreshPhase::VerifiedNonDispatch)
            .then_some(RefreshReason::VerifiedNonDispatchExpiry)
    });
    if let Some(reason_code) = reason {
        execution_progress::emit_refresh(CandidateRefresh {
            reason_code,
            phase,
            refresh_count: completed_refreshes + 1,
            refresh_limit: MAX_PLAN_CANDIDATE_REFRESHES,
        });
    } else {
        // Future retry classifications must not leak arbitrary adapter text.
        execution_progress::emit(execution_progress::Stage::RefreshingCandidate);
    }
}

fn can_auto_refresh_plan_candidate(error: &GoFinError) -> bool {
    let cooldown = rpc_candidate_cooldown(error);
    if cooldown.is_some_and(|delay| delay > MAX_PLAN_RPC_RETRY_WAIT)
        || (error.code == "RPC_RATE_LIMITED" && cooldown.is_none())
    {
        return false;
    }
    // A new candidate may reprice within the existing plan and policy bounds.
    // The shared code also reports identity and malformed-bound failures; those
    // must not be retried as ordinary USD price movement.
    if error.code == "VALUE_BOUND_EXCEEDED" {
        return error
            .details
            .as_deref()
            .is_some_and(|details| details["reason"].as_str() == Some("usd_ceiling_exceeded"));
    }
    matches!(
        error.code.as_str(),
        "PREVIEW_EXPIRED"
            | "SOLANA_BLOCKHASH_EXPIRED"
            | "SOLANA_TX_EXPIRED"
            | "JUPITER_RFQ_EXPIRED"
            | "BRIDGE_QUOTE_EXPIRED"
            | "STALE_VALUATION"
            | "PRICE_STALE"
            | "NONCE_CHANGED"
            | "EVIDENCE_TIMEOUT"
            | "RPC_READ_TIMEOUT"
            | "RPC_UNAVAILABLE"
            | "RPC_HISTORICAL_STATE_UNAVAILABLE"
            | "RPC_RATE_LIMITED"
    )
}

fn rpc_candidate_cooldown(error: &GoFinError) -> Option<std::time::Duration> {
    if !matches!(
        error.code.as_str(),
        "RPC_UNAVAILABLE"
            | "RPC_READ_TIMEOUT"
            | "RPC_HISTORICAL_STATE_UNAVAILABLE"
            | "RPC_RATE_LIMITED"
    ) {
        return None;
    }
    let details = error.details.as_deref()?;
    let top = details["retry_after_ms"]
        .as_u64()
        .filter(|delay| *delay > 0);
    let endpoints = details["rpc_health"]["endpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|endpoint| endpoint["status"] == "cooling_down")
        .filter_map(|endpoint| endpoint["retry_after_ms"].as_u64())
        .filter(|delay| *delay > 0);
    // Respect both an overall Retry-After and the earliest endpoint cooldown.
    // A shorter value in either source must not bypass the other restriction.
    match (top, endpoints.min()) {
        (Some(top), Some(endpoint)) => Some(top.max(endpoint)),
        (top, endpoint) => top.or(endpoint),
    }
    .map(std::time::Duration::from_millis)
}

async fn wait_for_rpc_candidate_cooldown(error: &GoFinError) {
    if let Some(delay) = rpc_candidate_cooldown(error) {
        // Admission has already checked this bound. Never shorten a provider's
        // cooldown, and re-enter normal authority/identity checks after waiting.
        debug_assert!(delay <= MAX_PLAN_RPC_RETRY_WAIT);
        tokio::time::sleep(delay).await;
    }
}

fn with_plan_refresh_details(mut error: GoFinError, refresh_count: usize) -> GoFinError {
    let mut details = error
        .details
        .as_deref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    details.insert("refresh_count".into(), json!(refresh_count));
    details.insert("refresh_limit".into(), json!(MAX_PLAN_CANDIDATE_REFRESHES));
    let exhausted = refresh_count >= MAX_PLAN_CANDIDATE_REFRESHES;
    details.insert("refresh_exhausted".into(), json!(exhausted));
    if exhausted
        && details.get("reconciliation_required") == Some(&json!(false))
        && details.get("reauthorization_required") == Some(&json!(false))
    {
        error.next_action = "Nothing was submitted. Automatic candidate refresh is exhausted. Inspect the failure and resolve its cause before trying again; do not repeat unchanged attempts with new IDs. Fresh Touch ID is unnecessary while the plan remains active and the corrected candidate stays within its approved scope.".into();
    }
    error.details = Some(Box::new(Value::Object(details)));
    error
}

fn request_with_child_id(mut request: OperationRequest) -> OperationRequest {
    request.request_id = Uuid::new_v4().to_string();
    request
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

fn validate_plan_recovery_permissions(
    spec: &financial_plans::FinancialPlanSpec,
    permissions: &[FinancialPlanStepPermission],
) -> Result<()> {
    for permission in permissions {
        let Some(recovery) = permission
            .permission
            .as_ref()
            .and_then(|permission| permission.recovery_parent.as_ref())
        else {
            continue;
        };
        let child_index = spec
            .steps
            .iter()
            .position(|step| step.step_id == permission.step_id)
            .ok_or_else(|| recovery_parent_invalid("Recovery child step is not in this plan."))?;
        let parent_index = spec
            .steps
            .iter()
            .position(|step| step.step_id == recovery.step_id)
            .ok_or_else(|| recovery_parent_invalid("Recovery parent step is not in this plan."))?;
        let recovery_chain_is_contiguous = parent_index < child_index
            && spec.steps[parent_index + 1..=child_index]
                .iter()
                .all(|step| {
                    permissions
                        .iter()
                        .find(|candidate| candidate.step_id == step.step_id)
                        .and_then(|candidate| candidate.permission.as_ref())
                        .and_then(|candidate| candidate.recovery_parent.as_ref())
                        .is_some_and(|candidate| candidate.step_id == recovery.step_id)
                });
        if !recovery_chain_is_contiguous
            || recovery.step_id == permission.step_id
            || permission.exact.is_some()
            || !valid_recovery_pointer(&recovery.operation_id_pointer)
            || !valid_recovery_pointer(&recovery.submission_pointer)
            || recovery.operation_id_pointer == recovery.submission_pointer
            || recovery
                .operation_id_pointer
                .starts_with(&format!("{}/", recovery.submission_pointer))
            || recovery
                .submission_pointer
                .starts_with(&format!("{}/", recovery.operation_id_pointer))
        {
            return Err(recovery_parent_invalid(
                "A recovery step must follow an earlier plan step through a contiguous chain of recovery children, with distinct parent-binding fields.",
            ));
        }
        let parent_permission = permissions
            .iter()
            .find(|candidate| candidate.step_id == recovery.step_id)
            .ok_or_else(|| recovery_parent_invalid("Recovery parent permission is missing."))?;
        if parent_permission
            .permission
            .as_ref()
            .and_then(|parent| parent.recovery_parent.as_ref())
            .is_some()
            || parent_permission.exact.is_some()
        {
            return Err(recovery_parent_invalid(
                "Recovery parents must be adapter-reviewed write steps, not reads or nested recovery actions.",
            ));
        }
        for pointer in [&recovery.operation_id_pointer, &recovery.submission_pointer] {
            let step = &spec.steps[child_index];
            if step
                .input
                .pointer(pointer)
                .is_some_and(|value| !value.is_null())
            {
                return Err(recovery_parent_invalid(
                    "Recovery identity fields in the approved input must be null placeholders.",
                ));
            }
        }
        let mut destinations = std::collections::BTreeSet::from([
            recovery.operation_id_pointer.clone(),
            recovery.submission_pointer.clone(),
        ]);
        for field in &recovery.parent_binding_fields {
            if !valid_recovery_pointer(&field.parent_binding_pointer)
                || !valid_recovery_pointer(&field.child_input_pointer)
                || !destinations.insert(field.child_input_pointer.clone())
            {
                return Err(recovery_parent_invalid(
                    "Parent binding fields must use valid, distinct pointers.",
                ));
            }
            if spec.steps[child_index]
                .input
                .pointer(&field.child_input_pointer)
                .is_some_and(|value| !value.is_null())
            {
                return Err(recovery_parent_invalid(
                    "Parent binding fields in the approved child input must be null placeholders.",
                ));
            }
        }
    }
    Ok(())
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

fn validate_plan_permissions(
    spec: &financial_plans::FinancialPlanSpec,
    permissions: &[FinancialPlanStepPermission],
) -> Result<()> {
    if permissions.len() != spec.steps.len() {
        return Err(plan_error(
            "PLAN_PERMISSION_INVALID",
            "An adapter permission is missing for a plan step.",
            "Use integrations that support every step in the plan.",
        ));
    }
    for step in &spec.steps {
        if step.operation == "bridge.execute" && step.completion.minimum_net_receipt.is_none() {
            return Err(plan_error(
                "PLAN_BRIDGE_COMPLETION_INVALID",
                "A deferred bridge intent must complete on a verified destination receipt.",
                "Declare the exact destination resource and asset with a minimum receipt target.",
            ));
        }
        let permission = permissions
            .iter()
            .find(|p| p.step_id == step.step_id)
            .ok_or_else(|| {
                plan_error(
                    "PLAN_PERMISSION_INVALID",
                    "An adapter permission is missing for a plan step.",
                    "Use integrations that support every step in the plan.",
                )
            })?;
        if permission.exact.is_some() == permission.permission.is_some() {
            return Err(plan_error(
                "PLAN_PERMISSION_INVALID",
                "A plan step must have exactly one permission mode.",
                "Regenerate the plan review with one exact or adapter-reviewed scope per step.",
            ));
        }
        validate_permission_completion(step, permission)?;
        for dependency in &step.funding_dependencies {
            let repeated_identity = step
                .funding_dependencies
                .iter()
                .filter(|other| {
                    other.resource == dependency.resource && other.asset == dependency.asset
                })
                .count()
                > 1;
            if repeated_identity {
                return Err(plan_error(
                    "PLAN_FUNDING_AMBIGUOUS",
                    "A step cannot draw the same resource and asset from multiple producers.",
                    "Declare one producing step for each funded input identity.",
                ));
            }
            let producing = permissions
                .iter()
                .find(|p| p.step_id == dependency.producing_step_id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_FUNDING_INVALID",
                        "A funding dependency has no producing step permission.",
                        "Declare funding from an earlier supported plan step.",
                    )
                })?;
            let identity = financial_plans::FinancialPlanFundingIdentity {
                resource: dependency.resource.clone(),
                asset: dependency.asset.clone(),
            };
            if !permission_credits(producing).contains(&identity) {
                return Err(plan_error(
                    "PLAN_FUNDING_IDENTITY_MISMATCH",
                    "A producing step cannot credit the declared funding resource and asset.",
                    "Use the exact resource and asset identities returned by the producing adapter permission.",
                ));
            }
            let declared_consumption = permission_debits(permission).iter().any(|debit| {
                debit.resource == dependency.resource
                    && debit.amount.asset == dependency.asset
                    && !debit.amount.atoms.is_zero()
            }) || (dependency.includes_additional_costs()
                && permission_costs(permission).iter().any(|cost| {
                    matches!(
                        cost.treatment,
                        gofin_domain::CostTreatment::AdditionalDebit
                            | gofin_domain::CostTreatment::VenueCalculatedAdditionalDebit
                    ) && cost.location.resource() == &dependency.resource
                        && cost.amount.asset == dependency.asset
                        && !cost.amount.atoms.is_zero()
                }));
            if !declared_consumption {
                return Err(plan_error(
                    "PLAN_FUNDING_UNUSED",
                    "A funding dependency does not match any permitted input debit or additional fee.",
                    "Declare an exact resource and asset that this step may consume.",
                ));
            }
        }
    }
    // Report independent accounting problems together so repairing one cap does
    // not require another live review just to discover the next missing identity.
    finish_plan_validation(
        [
            validate_cumulative_plan_scopes(spec, permissions),
            validate_aggregate_permission_bounds(spec, permissions),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect(),
    )
}

fn finish_plan_validation(mut errors: Vec<GoFinError>) -> Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    let diagnostics: Vec<Value> = errors
        .iter()
        .flat_map(|error| {
            error
                .details
                .as_deref()
                .and_then(|details| details["validation_errors"].as_array())
                .cloned()
                .unwrap_or_else(|| vec![json!(error)])
        })
        .collect();
    let mut first = errors.remove(0);
    let mut details = first
        .details
        .as_deref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    details.insert("validation_errors".into(), json!(diagnostics));
    first.details = Some(Box::new(Value::Object(details)));
    if !first.next_action.contains("--verbose") {
        first
            .next_action
            .push_str(" Use --verbose or --json to inspect all validation details.");
    }
    Err(first)
}

fn plan_diagnostic_steps(spec: &financial_plans::FinancialPlanSpec, step_ids: &[&str]) -> Value {
    json!(
        spec.steps
            .iter()
            .filter(|step| step_ids.contains(&step.step_id.as_str()))
            .map(|step| json!({"step_id": step.step_id, "operation": step.operation}))
            .collect::<Vec<_>>()
    )
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

fn validate_plan_authorization_duration(
    requested_seconds: u64,
    permissions: &[FinancialPlanStepPermission],
) -> Result<()> {
    for permission in permissions {
        let Some(maximum) = permission
            .permission
            .as_ref()
            .and_then(|permission| permission.authorization_seconds)
        else {
            continue;
        };
        if maximum == 0 || requested_seconds > maximum {
            return Err(plan_error(
                "PLAN_AUTHORIZATION_DURATION_EXCEEDED",
                "The requested plan authorization lasts longer than an adapter-reviewed step allows.",
                "Review the workflow duration and the adapter's intentional authorization limit; revise the approved scope or workflow rather than truncating consent.",
            ).with_details(json!({"step_id":permission.step_id,"requested_seconds":requested_seconds,"maximum_authorization_seconds":maximum})));
        }
    }
    Ok(())
}

fn validate_cumulative_plan_scopes(
    spec: &financial_plans::FinancialPlanSpec,
    permissions: &[FinancialPlanStepPermission],
) -> Result<()> {
    let mut scopes = std::collections::BTreeMap::new();
    for permission in permissions {
        let Some(scope) = permission
            .permission
            .as_ref()
            .and_then(|permission| permission.cumulative_scope.as_ref())
        else {
            continue;
        };
        if scope.scope_id.trim().is_empty() || scope.scope_id.len() > 256 {
            return Err(plan_error(
                "PLAN_CUMULATIVE_SCOPE_INVALID",
                "An adapter cumulative scope must have a stable nonempty identity.",
                "Regenerate the plan review with a valid shared parent scope.",
            ));
        }
        let mut debit_ids = std::collections::BTreeSet::new();
        if scope.maximum_debits.is_empty() && scope.maximum_costs.is_empty()
            || scope.maximum_debits.iter().any(|debit| {
                debit.amount.atoms.is_zero()
                    || !debit_ids.insert((debit.resource.clone(), debit.amount.asset.clone()))
            })
        {
            return Err(plan_error(
                "PLAN_CUMULATIVE_SCOPE_INVALID",
                "An adapter cumulative scope must contain valid positive unique debit and cost caps.",
                "Regenerate the plan review with exact parent limits.",
            ));
        }
        let mut obligation_ids = std::collections::BTreeSet::new();
        if scope.maximum_obligations.iter().any(|cap| {
            cap.amount.atoms.is_zero()
                || !obligation_ids.insert((cap.resource.clone(), cap.amount.asset.clone()))
        }) {
            return Err(plan_error(
                "PLAN_CUMULATIVE_SCOPE_INVALID",
                "Debt obligation caps must be positive and unique by resource and asset.",
                "Regenerate the plan with exact borrowing limits.",
            ));
        }
        let mut cost_ids = Vec::new();
        if scope.maximum_costs.iter().any(|cost| {
            let identity = (
                cost.location.clone(),
                cost.amount.asset.clone(),
                cost.treatment.clone(),
            );
            if cost.amount.atoms.is_zero() || cost_ids.contains(&identity) {
                true
            } else {
                cost_ids.push(identity);
                false
            }
        }) {
            return Err(plan_error(
                "PLAN_CUMULATIVE_SCOPE_INVALID",
                "An adapter cumulative scope contains an invalid or duplicate fee cap.",
                "Regenerate the plan review with exact parent limits.",
            ));
        }
        if scopes
            .insert(scope.scope_id.clone(), scope.clone())
            .is_some_and(|previous| previous != *scope)
        {
            return Err(plan_error(
                "PLAN_CUMULATIVE_SCOPE_CONFLICT",
                "Steps sharing a cumulative scope disagree about the parent cap.",
                "Use one identical cumulative parent scope across every linked step.",
            ));
        }
    }
    let mut errors = Vec::new();
    for scope in scopes.values() {
        let step_ids: Vec<&str> = permissions
            .iter()
            .filter(|permission| {
                permission
                    .permission
                    .as_ref()
                    .and_then(|p| p.cumulative_scope.as_ref())
                    .is_some_and(|other| other.scope_id == scope.scope_id)
            })
            .map(|permission| permission.step_id.as_str())
            .collect();
        let steps = plan_diagnostic_steps(spec, &step_ids);
        for debit in &scope.maximum_debits {
            let cap = spec
                .aggregate_bounds
                .maximum_debits
                .iter()
                .find(|aggregate| {
                    aggregate.resource == debit.resource
                        && aggregate.amount.asset == debit.amount.asset
                });
            if cap.is_none_or(|aggregate| aggregate.amount.atoms < debit.amount.atoms) {
                errors.push(plan_error(
                    "PLAN_CUMULATIVE_SCOPE_OUTSIDE_AGGREGATE",
                    &format!("Shared scope {} for steps {} exceeds or lacks an aggregate debit cap.",
                        scope.scope_id, step_ids.join(", ")),
                    "Compare the required and declared amounts in details.validation_errors; correct the step scope or aggregate cap within the intended budget, then preview again.",
                ).with_details(json!({
                    "scope_id": scope.scope_id, "steps": steps,
                    "resource": debit.resource, "asset": debit.amount.asset,
                    "required_atoms": debit.amount.atoms,
                    "maximum_atoms": cap.map(|cap| &cap.amount.atoms),
                    "aggregate_path": "/aggregate_bounds/maximum_debits",
                })));
            }
        }
        for cost in &scope.maximum_costs {
            let cap = spec
                .aggregate_bounds
                .maximum_costs
                .iter()
                .filter(|aggregate| {
                    aggregate.location == cost.location
                        && aggregate.amount.asset == cost.amount.asset
                        && aggregate.treatment == cost.treatment
                })
                .max_by(|left, right| left.amount.atoms.cmp(&right.amount.atoms));
            if cap.is_none_or(|aggregate| aggregate.amount.atoms < cost.amount.atoms) {
                errors.push(plan_error(
                    "PLAN_CUMULATIVE_SCOPE_OUTSIDE_AGGREGATE",
                    &format!("Shared scope {} for steps {} exceeds or lacks an aggregate fee cap.",
                        scope.scope_id, step_ids.join(", ")),
                    "Compare the exact fee identity, treatment and amounts in details.validation_errors; correct the step scope or aggregate cap within the intended budget, then preview again.",
                ).with_details(json!({
                    "scope_id": scope.scope_id, "steps": steps,
                    "location": cost.location, "asset": cost.amount.asset,
                    "treatment": cost.treatment, "required_atoms": cost.amount.atoms,
                    "maximum_atoms": cap.map(|cap| &cap.amount.atoms),
                    "aggregate_path": "/aggregate_bounds/maximum_costs",
                    "declared_cost_bounds": spec.aggregate_bounds.maximum_costs.iter()
                        .filter(|other| other.location == cost.location && other.amount.asset == cost.amount.asset)
                        .collect::<Vec<_>>(),
                })));
            }
        }
    }
    finish_plan_validation(errors)
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

fn validate_exact_scope(scope: &FinancialPlanExactScope) -> Result<()> {
    let mut debits = BTreeSet::new();
    for debit in &scope.maximum_debits {
        if debit.amount.atoms.is_zero()
            || !debits.insert((debit.resource.clone(), debit.amount.asset.clone()))
        {
            return Err(plan_error(
                "PLAN_EXACT_SCOPE_INVALID",
                "Exact debit caps must be positive and unique by resource and asset.",
                "Use one positive maximum debit for each resource and asset.",
            ));
        }
    }
    let mut costs = Vec::new();
    for cost in &scope.maximum_costs {
        let valid_text = |value: &str| !value.is_empty() && value.trim() == value;
        let valid_location = match &cost.location {
            AccountingLocation::Cash { resource } => valid_text(&resource.0),
            AccountingLocation::Position {
                resource, position, ..
            } => valid_text(&resource.0) && valid_text(position),
        };
        let valid_treatment = matches!(cost.location, AccountingLocation::Cash { .. })
            || matches!(cost.treatment, CostTreatment::IncludedInDebit);
        if !valid_location
            || !valid_treatment
            || !valid_text(&cost.amount.asset.network)
            || !valid_text(&cost.amount.asset.reference)
            || costs.iter().any(|identity| {
                identity
                    == &(
                        cost.location.clone(),
                        cost.amount.asset.clone(),
                        cost.treatment.clone(),
                    )
            })
        {
            return Err(plan_error(
                "PLAN_EXACT_SCOPE_INVALID",
                "Exact cost caps must have valid unique location, asset, and treatment identities; zero preserves exact zero fee authority.",
                "Use one nonnegative maximum cost for each exact fee identity.",
            ));
        }
        costs.push((
            cost.location.clone(),
            cost.amount.asset.clone(),
            cost.treatment.clone(),
        ));
    }
    let mut credits = BTreeSet::new();
    if scope
        .possible_credits
        .iter()
        .any(|identity| !credits.insert(identity.clone()))
    {
        return Err(plan_error(
            "PLAN_EXACT_SCOPE_INVALID",
            "Possible credit identities must be unique.",
            "Declare each possible output resource and asset once.",
        ));
    }
    Ok(())
}

fn validate_exact_coinbase_venue_fee_policy(
    operation: &str,
    input: &Value,
    scope: &FinancialPlanExactScope,
    aggregate: &financial_plans::FinancialPlanBounds,
    funding_dependencies: &[gofin_domain::financial_plans::FinancialPlanFundingDependency],
) -> Result<()> {
    let has_venue_fee = scope
        .maximum_costs
        .iter()
        .any(|cost| cost.treatment == gofin_domain::CostTreatment::VenueCalculatedAdditionalDebit);
    let requests_venue_fee = operation == "coinbase.withdraw"
        && input.get("feeMode").and_then(Value::as_str) == Some("venue_calculated");
    if !has_venue_fee && !requests_venue_fee {
        return Ok(());
    }

    let invalid = || {
        plan_error(
            "PLAN_VENUE_CALCULATED_FEE_UNSUPPORTED",
            "Uncapped venue-calculated fees are supported only for an exact Coinbase withdrawal to an observed network with explicit consent and a finite upfront reserve.",
            "Use the exact Coinbase withdrawal input, reserve and matching currency scope, or obtain fresh standalone approval.",
        )
    };
    let Some(currency) = input.get("currency").map_or(Some("USDC"), Value::as_str) else {
        return Err(invalid());
    };
    let Some(amount) = input.get("amount").and_then(Value::as_str) else {
        return Err(invalid());
    };
    let Some(reserve) = super::coinbase_withdrawal_fee_reserve(input) else {
        return Err(invalid());
    };
    let Some(address) = input.get("address").and_then(Value::as_str) else {
        return Err(invalid());
    };
    if operation != "coinbase.withdraw"
        || input
            .get("network")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || input.get("feeMode").and_then(Value::as_str) != Some("venue_calculated")
        || input
            .get("account")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || input
            .get("accountId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || input
            .get("destination")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || address.is_empty()
    {
        return Err(invalid());
    }
    let principal_atoms =
        gofin_domain::Quantity::from_decimal(amount, 36).map_err(|_| invalid())?;
    let reserve_atoms = gofin_domain::Quantity::from_decimal(reserve, 36).map_err(|_| invalid())?;
    if principal_atoms.is_zero()
        || reserve_atoms.is_zero()
        || scope.maximum_debits.len() != 1
        || scope.maximum_costs.len() != 1
        || scope.possible_credits.len() != 1
    {
        return Err(invalid());
    }
    let debit = &scope.maximum_debits[0];
    let cost = &scope.maximum_costs[0];
    if funding_dependencies.iter().any(|dependency| {
        dependency.resource == debit.resource
            && dependency.asset == debit.amount.asset
            && dependency.includes_additional_costs()
    }) {
        return Err(invalid());
    }
    let expected_asset = gofin_domain::AssetId {
        network: "coinbase".into(),
        reference: currency.into(),
        decimals: 36,
    };
    let credit = &scope.possible_credits[0];
    let evm = credit.asset.network.starts_with("ethereum:");
    let same = |left: &str, right: &str| {
        if evm {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };
    let credit_matches = super::coinbase_withdrawal_credit_network(&credit.asset.network)
        && credit
            .resource
            .0
            .strip_prefix(&format!("{}:", credit.asset.network))
            .is_some_and(|bound| same(bound, address))
        && input
            .get("token")
            .and_then(Value::as_str)
            .is_none_or(|token| same(&credit.asset.reference, token))
        && gofin_domain::Quantity::from_decimal(amount, credit.asset.decimals).is_ok();
    if debit.resource.0.is_empty()
        || !debit.resource.0.starts_with("coinbase:portfolio:")
        || debit.amount.asset != expected_asset
        || debit.amount.atoms != principal_atoms
        || cost.location
            != (gofin_domain::AccountingLocation::Cash {
                resource: debit.resource.clone(),
            })
        || cost.amount.asset != expected_asset
        || cost.amount.atoms != reserve_atoms
        || cost.treatment != gofin_domain::CostTreatment::VenueCalculatedAdditionalDebit
        || !credit_matches
        || !aggregate.maximum_debits.iter().any(|bound| {
            bound.resource == debit.resource
                && bound.amount.asset == debit.amount.asset
                && bound.amount.atoms >= debit.amount.atoms
        })
        || !aggregate.maximum_costs.iter().any(|bound| {
            bound.location == cost.location
                && bound.amount.asset == cost.amount.asset
                && bound.treatment == cost.treatment
                && bound.amount.atoms >= cost.amount.atoms
        })
    {
        return Err(invalid());
    }
    Ok(())
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

fn validate_generated_attempt_input_schema(
    schema: &Value,
    input: &Value,
    fields: &[String],
    generated_value: &str,
) -> Result<()> {
    let mut generated_input = input.clone();
    for pointer in fields {
        let slot = generated_input.pointer_mut(pointer).ok_or_else(|| {
            plan_error(
                "PLAN_GENERATED_INPUT_METADATA_INVALID",
                "A broker-generated attempt field is missing from the plan input template.",
                "Fix the operation definition or supply its required string identity field.",
            )
        })?;
        *slot = Value::String(generated_value.to_owned());
    }
    validate_plan_json_schema(schema, &generated_input)
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

fn plan_step_actor_allowed(kind: OperationKind, actor: &gofin_domain::ExecutionActor) -> bool {
    match kind {
        OperationKind::Read => *actor == gofin_domain::ExecutionActor::AgentReadable,
        OperationKind::Write => *actor == gofin_domain::ExecutionActor::AgentInvocable,
        _ => false,
    }
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

fn validate_permission_completion(
    step: &financial_plans::FinancialPlanStepSpec,
    permission: &FinancialPlanStepPermission,
) -> Result<()> {
    let debits = permission_debits(permission);
    let credits = permission_credits(permission);
    if let Some(target) = &step.completion.principal_spend
        && !debits.iter().any(|cap| {
            cap.resource == target.resource
                && cap.amount.asset == target.amount.asset
                && cap.amount.atoms >= target.amount.atoms
        })
    {
        return Err(plan_error(
            "PLAN_COMPLETION_OUTSIDE_SCOPE",
            "The principal-spend completion target exceeds its adapter permission scope.",
            "Lower the target or choose a supported step scope.",
        ));
    }
    if let Some(target) = &step.completion.minimum_net_receipt {
        let identity = financial_plans::FinancialPlanFundingIdentity {
            resource: target.resource.clone(),
            asset: target.amount.asset.clone(),
        };
        if !credits.contains(&identity) {
            return Err(plan_error(
                "PLAN_COMPLETION_OUTSIDE_SCOPE",
                "The minimum receipt target is outside the adapter's possible credit identities.",
                "Use an exact resource and asset the adapter can prove as output.",
            ));
        }
    }
    Ok(())
}

fn validate_aggregate_permission_bounds(
    spec: &financial_plans::FinancialPlanSpec,
    permissions: &[FinancialPlanStepPermission],
) -> Result<()> {
    // Completion targets are required gross spending, unlike optional step
    // ceilings that may deliberately share a smaller aggregate allowance.
    // Catch impossible plans with exact arithmetic before asking for approval.
    let mut errors = Vec::new();
    let mut required = BTreeMap::<(ResourceId, gofin_domain::AssetId), BigUint>::new();
    for target in spec
        .steps
        .iter()
        .filter_map(|step| step.completion.principal_spend.as_ref())
    {
        *required
            .entry((target.resource.clone(), target.amount.asset.clone()))
            .or_default() += target.amount.atoms.integer();
    }
    for ((resource, asset), amount) in required {
        let cap = spec
            .aggregate_bounds
            .maximum_debits
            .iter()
            .find(|cap| cap.resource == resource && cap.amount.asset == asset);
        if cap.is_none_or(|cap| amount > cap.amount.atoms.integer()) {
            errors.push(plan_error(
                "PLAN_COMPLETION_OUTSIDE_AGGREGATE",
                "Required principal-spend targets exceed the plan's cumulative debit cap.",
                "Reduce the completion targets or review a sufficient aggregate cap before authorization.",
            )
            .with_details(json!({
                "steps": spec.steps.iter().filter(|step| step.completion.principal_spend.as_ref()
                    .is_some_and(|target| target.resource == resource && target.amount.asset == asset))
                    .map(|step| json!({"step_id": step.step_id, "operation": step.operation})).collect::<Vec<_>>(),
                "aggregate_path": "/aggregate_bounds/maximum_debits",
                "resource": resource,
                "asset": asset,
                "required_atoms": amount.to_string(),
                "maximum_atoms": cap.map(|cap| &cap.amount.atoms),
            })));
        }
    }
    for permission in permissions {
        for item in permission_debits(permission) {
            if !spec
                .aggregate_bounds
                .maximum_debits
                .iter()
                .any(|cap| cap.resource == item.resource && cap.amount.asset == item.amount.asset)
            {
                errors.push(plan_error(
                    "PLAN_AGGREGATE_IDENTITY_MISMATCH",
                    "Aggregate debit bounds omit a resource and asset used by a step permission.",
                    "Declare a cumulative cap for each exact debit identity in details.validation_errors, then preview again.",
                ).with_details(json!({
                    "step_id": permission.step_id,
                    "steps": plan_diagnostic_steps(spec, &[&permission.step_id]),
                    "resource": item.resource, "asset": item.amount.asset,
                    "step_maximum_atoms": item.amount.atoms,
                    "maximum_atoms": Value::Null,
                    "aggregate_path": "/aggregate_bounds/maximum_debits",
                })));
            }
        }
        for item in permission_costs(permission) {
            if !spec.aggregate_bounds.maximum_costs.iter().any(|cap| {
                cap.location == item.location
                    && cap.amount.asset == item.amount.asset
                    && cap.treatment == item.treatment
            }) {
                errors.push(plan_error(
                    "PLAN_AGGREGATE_IDENTITY_MISMATCH",
                    "Aggregate cost bounds omit a fee identity used by a step permission.",
                    "Declare a cumulative cap for each exact fee identity and treatment in details.validation_errors, then preview again.",
                ).with_details(json!({
                    "step_id": permission.step_id,
                    "steps": plan_diagnostic_steps(spec, &[&permission.step_id]),
                    "location": item.location, "asset": item.amount.asset,
                    "treatment": item.treatment, "step_maximum_atoms": item.amount.atoms,
                    "maximum_atoms": Value::Null,
                    "aggregate_path": "/aggregate_bounds/maximum_costs",
                    "declared_cost_bounds": spec.aggregate_bounds.maximum_costs.iter()
                        .filter(|other| other.location == item.location && other.amount.asset == item.amount.asset)
                        .collect::<Vec<_>>(),
                })));
            }
        }
    }
    finish_plan_validation(errors)
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
    transfer_id: String,
    bridge_step_id: String,
    kind: gofin_domain::BridgeStepKind,
    expected_transfer: Value,
    updated_transfer: gofin_domain::BridgeTransferRecord,
}

fn prepare_plan_bridge_attempt_claim(
    request: &OperationRequest,
    prepared: &PreparedOperation,
    plan_id: &str,
    revision_id: &str,
    plan_step_id: &str,
    operation_id: &str,
    store: &Store,
) -> Result<Option<PlanBridgeAttemptClaim>> {
    let Some(mut claim) = validate_plan_bridge_attempt_scope(
        request,
        prepared,
        plan_id,
        revision_id,
        plan_step_id,
        None,
        store,
    )?
    else {
        return Ok(None);
    };
    let link = claim
        .updated_transfer
        .steps
        .iter_mut()
        .find(|link| link.step_id == claim.bridge_step_id)
        .ok_or_else(|| internal("validated bridge step disappeared"))?;
    link.operation_id = Some(operation_id.to_owned());
    link.plan_authority = Some(gofin_domain::BridgePlanAuthority {
        plan_id: plan_id.to_owned(),
        revision_id: revision_id.to_owned(),
        plan_step_id: plan_step_id.to_owned(),
    });
    link.state = gofin_domain::BridgeStepState::Submitted;
    claim.updated_transfer.state = claim.kind.pending_state();
    claim.updated_transfer.version = claim
        .updated_transfer
        .version
        .checked_add(1)
        .ok_or_else(|| internal("bridge transfer version overflow"))?;
    Ok(Some(claim))
}

// Pure validation shared by initial admission and exact destination receipt
// candidates. An existing claim is accepted only for its already-bound attempt.
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
        transfer_id,
        bridge_step_id,
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

fn validate_plan_aggregate_remaining(
    record: &FinancialPlanAuthorizationRecord,
    prepared: &PreparedOperation,
) -> Result<()> {
    let mut debits = record.usage.consumed_debits.clone();
    let mut costs = record.usage.consumed_costs.clone();
    for reservation in &record.usage.reservations {
        debits = add_amounts(&debits, &reservation.debits)?;
        costs = add_cost_bounds(&costs, &reservation.costs)?;
    }
    let remaining_debits = remaining_debits(
        &record.revision.spec.aggregate_bounds.maximum_debits,
        Some(&debits),
    )?;
    let remaining_costs = remaining_costs(
        &record.revision.spec.aggregate_bounds.maximum_costs,
        Some(&costs),
    )?;
    let aggregate_scope = BoundarySpec {
        fixed: Value::Null,
        parameters: Value::Null,
        maximum_debits: record.revision.spec.aggregate_bounds.maximum_debits.clone(),
        maximum_costs: record.revision.spec.aggregate_bounds.maximum_costs.clone(),
        summary: "aggregate plan bound".into(),
    };
    let candidate = BoundaryAuthorization {
        authorization_source: None,
        yolo_provenance: None,
        id: record.revision.plan_id.clone(),
        intent_id: record.revision.plan_id.clone(),
        operation: prepared.operation.clone(),
        bounds: Value::Null,
        spec: aggregate_scope,
        approved_at: record.revision.approved_at.clone(),
        expires_at: record.revision.expires_at.clone(),
        valid_for_seconds: record.revision.valid_for_seconds,
        status: BoundaryAuthorizationStatus::Active,
        remaining_debits,
        remaining_costs,
        spent_debits: debits,
        spent_costs: costs,
        replaces: None,
    };
    validate_candidate_caps(&candidate, prepared)
}

#[cfg(test)]
fn validate_plan_funding(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
) -> Result<()> {
    validate_plan_funding_inner(record, step_id, prepared, None, None)
}

fn validate_plan_funding_with_store(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
    store: &Store,
) -> Result<()> {
    let prerequisite_principal =
        bridge_allowance_funding_principal(record, step_id, prepared, store)?;
    let has_fractional_dependency = record
        .revision
        .spec
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .is_some_and(|step| {
            step.funding_dependencies
                .iter()
                .any(|dependency| dependency.maximum_fraction_bps.is_some())
        });
    if !has_fractional_dependency {
        return validate_plan_funding_inner(
            record,
            step_id,
            prepared,
            None,
            prerequisite_principal.as_ref(),
        );
    }
    let consumed_by_step = replay_plan_funding_consumption(record, store)?;
    validate_plan_funding_inner(
        record,
        step_id,
        prepared,
        Some(&consumed_by_step),
        prerequisite_principal.as_ref(),
    )
}

// Allowance changes are prerequisites within one logical bridge step. They
// require its exact selected principal to be funded, but must not reserve or
// consume that principal before the later deposit actually debits it.
fn bridge_allowance_funding_principal(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
    store: &Store,
) -> Result<Option<ResourceAmount>> {
    if prepared.operation != "bridge.execute"
        || !record
            .revision
            .spec
            .steps
            .iter()
            .any(|step| step.step_id == step_id && step.operation == "bridge.execute")
        || prepared
            .intent
            .debits
            .iter()
            .any(|debit| !debit.amount.atoms.is_zero())
    {
        return Ok(None);
    }
    let Some(marker) = prepared
        .binding
        .product_get(gofin_domain::BRIDGE_STEP_METADATA_KEY)?
    else {
        return Ok(None);
    };
    let kind: Option<gofin_domain::BridgeStepKind> = marker
        .get("kind")
        .cloned()
        .and_then(|kind| serde_json::from_value(kind).ok());
    if !matches!(
        kind,
        Some(gofin_domain::BridgeStepKind::Approval | gofin_domain::BridgeStepKind::ResetAllowance)
    ) {
        return Ok(None);
    }
    // Old prepared/persisted bindings are still valid, but cannot manufacture
    // a deferred funding observation they never contained.
    if marker.get("source_principal").is_none() {
        return Ok(None);
    }
    let invalid = || {
        plan_error(
            "PLAN_BRIDGE_FUNDING_INVALID",
            "The allowance prerequisite does not identify the saved bridge's exact source principal.",
            "Prepare the original saved bridge child again; keep its plan, route and funding dependency.",
        )
    };
    let transfer_id = marker
        .get("transfer_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let transfer = store
        .bridge_transfer(transfer_id)
        .map_err(map_store)?
        .ok_or_else(invalid)?;
    bridge_allowance_principal_from_transfer(record, step_id, marker, &transfer).map(Some)
}

fn bridge_allowance_principal_from_transfer(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    marker: &Value,
    transfer: &gofin_domain::BridgeTransferRecord,
) -> Result<ResourceAmount> {
    let invalid = || {
        plan_error(
            "PLAN_BRIDGE_FUNDING_INVALID",
            "The allowance prerequisite does not identify the saved bridge's exact source principal.",
            "Prepare the original saved bridge child again; keep its plan, route and funding dependency.",
        )
    };
    let principal: ResourceAmount = serde_json::from_value(
        marker
            .get("source_principal")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|_| invalid())?;
    let kind: gofin_domain::BridgeStepKind =
        serde_json::from_value(marker.get("kind").cloned().unwrap_or(Value::Null))
            .map_err(|_| invalid())?;
    if !matches!(
        kind,
        gofin_domain::BridgeStepKind::Approval | gofin_domain::BridgeStepKind::ResetAllowance
    ) || marker.get("transfer_id").and_then(Value::as_str) != Some(transfer.id.as_str())
    {
        return Err(invalid());
    }
    let link_id = marker
        .get("step_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let prospective = transfer.prospective_debit.as_ref().ok_or_else(invalid)?;
    let marker_prospective: gofin_domain::BridgeProspectiveDebit = serde_json::from_value(
        marker
            .get("prospective_debit")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|_| invalid())?;
    let own_authority = |authority: &gofin_domain::BridgePlanAuthority| {
        authority.plan_id == record.revision.plan_id && authority.plan_step_id == step_id
    };
    let valid_link = transfer.steps.iter().any(|link| {
        link.step_id == link_id
            && link.kind == kind
            && link.grant_id.is_none()
            && link.operation_id.is_none()
            && link.state == gofin_domain::BridgeStepState::Unsubmitted
            && link.plan_authority.as_ref().is_none_or(own_authority)
    });
    let valid_deposit = transfer.steps.iter().any(|link| {
        link.step_id == prospective.deposit_step_id
            && link.kind == gofin_domain::BridgeStepKind::Deposit
            && link.state == gofin_domain::BridgeStepState::Unsubmitted
            && link.grant_id.is_none()
            && link.operation_id.is_none()
            && link.plan_authority.as_ref().is_none_or(own_authority)
    });
    let permission = permission_for_step(&record.revision, step_id)?;
    if transfer.grant_id.is_some()
        || transfer.transit.is_some()
        || transfer
            .plan_scope
            .as_ref()
            .is_some_and(|scope| scope.plan_id != record.revision.plan_id)
        || !matches!(
            transfer.state,
            gofin_domain::BridgeTransferState::Prepared
                | gofin_domain::BridgeTransferState::ApprovalPending
        )
        || marker.get("grant_id").is_some_and(|grant| !grant.is_null())
        || !valid_link
        || !valid_deposit
        || &marker_prospective != prospective
        || principal.resource != prospective.resource
        || principal.amount.asset != prospective.maximum.asset
        || principal.amount.atoms.is_zero()
        || principal.amount.atoms > prospective.maximum.atoms
        || !permission_debits(permission).iter().any(|debit| {
            debit.resource == principal.resource
                && debit.amount.asset == principal.amount.asset
                && debit.amount.atoms >= principal.amount.atoms
        })
    {
        return Err(invalid());
    }
    Ok(principal)
}

type FundingConsumptionKey = (String, ResourceId, gofin_domain::AssetId);

fn replay_plan_funding_consumption(
    record: &FinancialPlanAuthorizationRecord,
    store: &Store,
) -> Result<BTreeMap<(String, FundingConsumptionKey), BigUint>> {
    let mut by_consumer = BTreeMap::new();
    for attempt in &record.attempts {
        let Some(raw) = store.get_record(&attempt.attempt_id).map_err(map_store)? else {
            return Err(plan_error(
                "PLAN_ATTEMPT_MISSING",
                "A plan attempt record is missing while checking funding limits.",
                "Restore the durable operation record before continuing.",
            ));
        };
        let operation: OperationRecord = from_value(raw)?;
        let human = store
            .operation_resolution(&operation.id)
            .map_err(map_store)?;
        if operation.effects.is_none() && human.is_none() {
            continue;
        }
        // The durable plan usage already includes ordinary operation credits
        // and externally verified bridge destination receipts. Attribute each
        // attempt independently against that final availability, with prior
        // consumption cleared; summing these per-attempt debits yields the
        // exact per-consumer total without inventing a receipt from immutable
        // source-operation effects or double-counting ordinary producer writes.
        let mut projected = record.clone();
        projected.usage = FinancialPlanUsage::default();
        projected.usage.verified_proceeds = record
            .usage
            .funding_proceeds()
            .map(|item| FinancialPlanProceeds {
                producing_step_id: item.producing_step_id.clone(),
                resource: item.resource.clone(),
                asset: item.asset.clone(),
                available: item.available.clone(),
                consumed: gofin_domain::Quantity::zero(),
            })
            .collect();
        if let Some(human) = human {
            let (debits, costs) = super::operation_resolutions::human_charged_totals(&human)?;
            for debit in debits {
                super::operation_resolutions::consume_reported_funding(
                    &mut projected,
                    &attempt.step_id,
                    &debit.resource,
                    &debit.amount.asset,
                    &debit.amount.atoms.integer(),
                    false,
                );
            }
            for cost in costs {
                if matches!(
                    cost.treatment,
                    CostTreatment::AdditionalDebit | CostTreatment::VenueCalculatedAdditionalDebit
                ) {
                    super::operation_resolutions::consume_reported_funding(
                        &mut projected,
                        &attempt.step_id,
                        cost.location.resource(),
                        &cost.amount.asset,
                        &cost.amount.atoms.integer(),
                        true,
                    );
                }
            }
        } else {
            account_financial_plan_effects(&mut projected, &operation)?;
        }
        for item in &projected.usage.verified_proceeds {
            if !item.consumed.is_zero() {
                *by_consumer
                    .entry((
                        attempt.step_id.clone(),
                        (
                            item.producing_step_id.clone(),
                            item.resource.clone(),
                            item.asset.clone(),
                        ),
                    ))
                    .or_insert_with(BigUint::zero) += item.consumed.integer();
            }
        }
    }
    Ok(by_consumer)
}

fn validate_plan_funding_inner(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
    consumption_by_step: Option<&BTreeMap<(String, FundingConsumptionKey), BigUint>>,
    prerequisite_principal: Option<&ResourceAmount>,
) -> Result<()> {
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
    for dependency in &step.funding_dependencies {
        let mut debit_required = prepared
            .intent
            .debits
            .iter()
            .filter(|debit| {
                debit.resource == dependency.resource && debit.amount.asset == dependency.asset
            })
            .fold(BigUint::zero(), |sum, debit| {
                sum + debit.amount.atoms.integer()
            });
        if debit_required.is_zero()
            && let Some(principal) = prerequisite_principal.filter(|principal| {
                principal.resource == dependency.resource
                    && principal.amount.asset == dependency.asset
            })
        {
            debit_required = principal.amount.atoms.integer();
        }
        let fee_required = prepared
            .intent
            .maximum_costs
            .iter()
            .filter(|cost| {
                matches!(
                    cost.treatment,
                    gofin_domain::CostTreatment::AdditionalDebit
                        | gofin_domain::CostTreatment::VenueCalculatedAdditionalDebit
                ) && cost.location.resource() == &dependency.resource
                    && cost.amount.asset == dependency.asset
            })
            .fold(BigUint::zero(), |sum, cost| {
                sum + cost.amount.atoms.integer()
            });
        let required = if dependency.includes_additional_costs() {
            debit_required + fee_required
        } else {
            debit_required
        };
        if required.is_zero() {
            return Err(plan_error(
                "PLAN_FUNDING_UNUSED",
                "A declared funding identity is not consumed by this step.",
                "Remove the unused dependency or declare the exact input identity it funds.",
            ));
        }
        let matching_proceeds = record
            .usage
            .funding_proceeds()
            .filter(|item| {
                item.producing_step_id == dependency.producing_step_id
                    && item.resource == dependency.resource
                    && item.asset == dependency.asset
            })
            .collect::<Vec<_>>();
        let proceeds = matching_proceeds.iter().try_fold(
            BigUint::zero(),
            |sum, item| -> Result<BigUint> {
                let available = item.available.integer();
                let consumed = item.consumed.integer();
                if consumed > available {
                    return Err(plan_error(
                        "PLAN_FUNDING_USAGE_INVALID",
                        "Persisted producer consumption exceeds its verified proceeds.",
                        "Preserve the plan history and repair the inconsistent funding ledger before continuing.",
                    ));
                }
                Ok(sum + available - consumed)
            },
        )?;
        if proceeds < required {
            return Err(plan_error(
                "PLAN_FUNDING_NOT_SETTLED",
                "Verified proceeds from the declared producing step do not cover this attempt.",
                "Reconcile the producing step or reduce the dependent input; unrelated balances cannot satisfy this dependency.",
            ));
        }
        if let Some(maximum_fraction_bps) = dependency.maximum_fraction_bps {
            let total_verified = matching_proceeds
                .iter()
                .fold(BigUint::zero(), |sum, item| sum + item.available.integer());
            let proceeds_key = (
                dependency.producing_step_id.clone(),
                dependency.resource.clone(),
                dependency.asset.clone(),
            );
            let already_consumed = match consumption_by_step {
                Some(usage) => usage
                    .get(&(step_id.to_owned(), proceeds_key))
                    .cloned()
                    .unwrap_or_default(),
                None => matching_proceeds
                    .iter()
                    .fold(BigUint::zero(), |sum, item| sum + item.consumed.integer()),
            };
            let reserved = record
                .usage
                .reservations
                .iter()
                .filter(|reservation| reservation.step_id == step_id)
                .fold(BigUint::zero(), |sum, reservation| {
                    let debit = reservation
                        .debits
                        .iter()
                        .filter(|item| {
                            item.resource == dependency.resource
                                && item.amount.asset == dependency.asset
                        })
                        .fold(BigUint::zero(), |total, item| {
                            total + item.amount.atoms.integer()
                        });
                    let fee = if dependency.includes_additional_costs() {
                        reservation
                            .costs
                            .iter()
                            .filter(|cost| {
                                matches!(
                                    cost.treatment,
                                    gofin_domain::CostTreatment::AdditionalDebit
                                        | gofin_domain::CostTreatment::VenueCalculatedAdditionalDebit
                                )
                                    && cost.location.resource() == &dependency.resource
                                    && cost.amount.asset == dependency.asset
                            })
                            .fold(BigUint::zero(), |total, cost| {
                                total + cost.amount.atoms.integer()
                            })
                    } else {
                        BigUint::zero()
                    };
                    sum + debit + fee
                });
            let maximum =
                (total_verified * BigUint::from(maximum_fraction_bps)) / BigUint::from(10_000u32);
            if already_consumed + reserved + required > maximum {
                return Err(plan_error(
                    "PLAN_FUNDING_FRACTION_EXCEEDED",
                    "The dependent input exceeds its approved share of verified producer proceeds.",
                    "Reduce the nominated input to the remaining approved fraction or obtain fresh plan authority.",
                ));
            }
        }
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

#[cfg(test)]
fn validate_revision_transition(
    prior: Option<&FinancialPlanAuthorizationRecord>,
    next: &financial_plans::FinancialPlanSpec,
    next_permissions: &[FinancialPlanStepPermission],
    usage: &FinancialPlanUsage,
    store: &Store,
) -> Result<()> {
    validate_revision_transition_with_completed(prior, next, next_permissions, usage, store, &[])
}

fn validate_revision_transition_with_completed(
    prior: Option<&FinancialPlanAuthorizationRecord>,
    next: &financial_plans::FinancialPlanSpec,
    next_permissions: &[FinancialPlanStepPermission],
    usage: &FinancialPlanUsage,
    store: &Store,
    completed_steps: &[String],
) -> Result<()> {
    let Some(prior) = prior else {
        return Ok(());
    };
    validate_cumulative_revision_scopes(prior, next, next_permissions, store)?;
    validate_active_bridge_revision_scopes(prior, next, next_permissions, store)?;
    if prior.active_attempt_id.is_some() {
        return Err(plan_error(
            "PLAN_ATTEMPT_UNRESOLVED",
            "A plan revision cannot replace an unresolved attempt.",
            "Reconcile the existing attempt before revising the plan.",
        ));
    }
    let started_indices = prior
        .revision
        .spec
        .steps
        .iter()
        .enumerate()
        .filter_map(|(index, step)| {
            prior
                .attempts
                .iter()
                .any(|attempt| {
                    attempt.step_id == step.step_id
                        && !repairable_zero_effect_attempt(prior, attempt, store)
                })
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if let Some(last_started) = started_indices.last().copied() {
        if next.steps.len() <= last_started {
            return Err(plan_error(
                "PLAN_REVISION_HISTORY_CONFLICT",
                "A revision cannot remove a started plan step.",
                "Preserve the started prefix and revise only the unstarted suffix.",
            ));
        }
        for index in 0..=last_started {
            let old = &prior.revision.spec.steps[index];
            let new = &next.steps[index];
            let step_attempts = prior
                .attempts
                .iter()
                .filter(|attempt| attempt.step_id == old.step_id)
                .collect::<Vec<_>>();
            if !step_attempts.is_empty()
                && step_attempts
                    .iter()
                    .all(|attempt| repairable_zero_effect_attempt(prior, attempt, store))
            {
                if old.step_id != new.step_id
                    || old.operation != new.operation
                    || old.completion != new.completion
                    || old.funding_dependencies != new.funding_dependencies
                {
                    return Err(plan_error(
                        "PLAN_REVISION_HISTORY_CONFLICT",
                        "A failed zero-effect attempt can change its input or action window while preserving its financial identity and accounting declarations.",
                        "Keep the step operation, completion conditions, and funding dependencies unchanged.",
                    ));
                }
                continue;
            }
            if completed_steps
                .iter()
                .any(|step_id| step_id == &old.step_id)
            {
                let old_permission = prior
                    .revision
                    .permissions
                    .iter()
                    .find(|permission| permission.step_id == old.step_id)
                    .ok_or_else(|| {
                        plan_error(
                            "PLAN_PERMISSION_MISSING",
                            "The prior revision has incomplete permission history.",
                            "Restore the complete prior plan revision before replacing it.",
                        )
                    })?;
                let new_permission = next_permissions
                    .iter()
                    .find(|permission| permission.step_id == old.step_id)
                    .ok_or_else(|| {
                        plan_error(
                            "PLAN_PERMISSION_MISSING",
                            "A completed step has no replacement permission.",
                            "Keep every completed step unchanged in the replacement revision.",
                        )
                    })?;
                if old != new || old_permission != new_permission {
                    return Err(plan_error(
                        "PLAN_REVISION_HISTORY_CONFLICT",
                        "A revision must preserve the recorded specification and permission for each completed step.",
                        "Keep completed plan steps unchanged and revise only the remaining suffix.",
                    ));
                }
                continue;
            }
            if old.step_id != new.step_id
                || old.operation != new.operation
                || old.completion != new.completion
                || old.funding_dependencies != new.funding_dependencies
                || old.action_deadline_seconds != new.action_deadline_seconds
            {
                return Err(plan_error(
                    "PLAN_REVISION_HISTORY_CONFLICT",
                    "A revision cannot reorder or change a started step's identity, completion conditions, or funding declaration.",
                    "Preserve the started prefix and revise only the unstarted suffix.",
                ));
            }
            let old_permission = prior
                .revision
                .permissions
                .iter()
                .find(|permission| permission.step_id == old.step_id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_PERMISSION_MISSING",
                        "The prior revision has incomplete permission history.",
                        "Restore the complete prior plan revision before replacing it.",
                    )
                })?;
            let new_permission = next_permissions
                .iter()
                .find(|permission| permission.step_id == old.step_id)
                .ok_or_else(|| {
                    plan_error(
                        "PLAN_PERMISSION_MISSING",
                        "A started step has no replacement permission.",
                        "Keep every started step in the replacement revision.",
                    )
                })?;
            if old_permission.generated_input_fields != new_permission.generated_input_fields {
                return Err(plan_error(
                    "PLAN_REVISION_SCOPE_CHANGED",
                    "A revision cannot change broker-generated attempt identity fields for a started step.",
                    "Preserve each started step's generated identity fields; revise only the unstarted suffix.",
                ));
            }
            match (&old_permission.exact, &new_permission.exact) {
                (Some(old_exact), Some(new_exact)) => {
                    if old.input != new.input
                        || old_exact.definition_hash != new_exact.definition_hash
                        || old_exact.connections_hash != new_exact.connections_hash
                        || old_exact.scope.possible_credits != new_exact.scope.possible_credits
                    {
                        return Err(plan_error(
                            "PLAN_REVISION_SCOPE_CHANGED",
                            "A revision cannot change a started exact operation, connection identity, or possible credit identity.",
                            "Preserve the started step's exact scope; revise only limits above verified use.",
                        ));
                    }
                }
                (None, None) => {
                    let old_fixed = &old_permission
                        .permission
                        .as_ref()
                        .ok_or_else(|| internal("prior adapter scope missing"))?
                        .spec
                        .fixed;
                    let new_fixed = &new_permission
                        .permission
                        .as_ref()
                        .ok_or_else(|| internal("replacement adapter scope missing"))?
                        .spec
                        .fixed;
                    if old_fixed != new_fixed {
                        return Err(plan_error(
                            "PLAN_REVISION_SCOPE_CHANGED",
                            "A revision cannot change the fixed adapter scope of a started step.",
                            "Preserve the started step's adapter-reviewed scope.",
                        ));
                    }
                }
                _ => {
                    return Err(plan_error(
                        "PLAN_REVISION_SCOPE_CHANGED",
                        "A started step cannot switch between exact and adapter permission modes.",
                        "Preserve the started step's permission mode.",
                    ));
                }
            }
            let mut step_debits = Vec::new();
            let mut step_costs = Vec::new();
            for attempt in prior
                .attempts
                .iter()
                .filter(|attempt| attempt.step_id == old.step_id)
            {
                if let Some(raw) = store.get_record(&attempt.attempt_id).map_err(map_store)? {
                    let operation: OperationRecord = from_value(raw)?;
                    if let Some(verified) = operation.effects {
                        let (debits, costs, _) =
                            summarize_plan_effects(&verified.effects, &old.step_id, usage)?;
                        step_debits = add_amounts(&step_debits, &debits)?;
                        step_costs = add_cost_bounds(&step_costs, &costs)?;
                    }
                }
            }
            ensure_bounds_cover_usage(
                permission_debits(new_permission),
                &step_debits,
                permission_costs(new_permission),
                &step_costs,
            )?;
        }
    }
    for used in &usage.consumed_debits {
        if !next.aggregate_bounds.maximum_debits.iter().any(|cap| {
            cap.resource == used.resource
                && cap.amount.asset == used.amount.asset
                && cap.amount.atoms >= used.amount.atoms
        }) {
            return Err(plan_error(
                "PLAN_REVISION_BELOW_USAGE",
                "A revised debit bound is below already consumed authority.",
                "Keep revised limits at or above verified consumption.",
            ));
        }
    }
    for used in &usage.consumed_costs {
        if !next.aggregate_bounds.maximum_costs.iter().any(|cap| {
            cap.location == used.location
                && cap.amount.asset == used.amount.asset
                && cap.treatment == used.treatment
                && cap.amount.atoms >= used.amount.atoms
        }) {
            return Err(plan_error(
                "PLAN_REVISION_BELOW_USAGE",
                "A revised fee bound is below already consumed authority.",
                "Keep revised limits at or above verified consumption.",
            ));
        }
    }
    Ok(())
}

fn completed_plan_step_ids(status: FinancialPlanStatus) -> Vec<String> {
    status
        .steps
        .into_iter()
        .filter(|step| step.state == FinancialPlanStepProgressState::Completed)
        .map(|step| step.step_id)
        .collect()
}

fn repairable_zero_effect_attempt(
    prior: &FinancialPlanAuthorizationRecord,
    attempt: &FinancialPlanAttemptLinkage,
    store: &Store,
) -> bool {
    if attempt.submission_id.is_some() || !matches!(store.submission(&attempt.attempt_id), Ok(None))
    {
        return false;
    }
    let Some(operation) = store
        .get_record(&attempt.attempt_id)
        .ok()
        .flatten()
        .and_then(|value| from_value::<OperationRecord>(value).ok())
    else {
        return false;
    };
    repairable_zero_effect_record(prior, attempt, &operation, false)
}

fn repairable_zero_effect_record(
    prior: &FinancialPlanAuthorizationRecord,
    attempt: &FinancialPlanAttemptLinkage,
    operation: &OperationRecord,
    durable_submission_exists: bool,
) -> bool {
    !durable_submission_exists
        && attempt.submission_id.is_none()
        && matches!(
            operation.status,
            Status::Failed | Status::Expired | Status::Cancelled | Status::Rejected
        )
        && operation.submission.is_none()
        && operation.dispatch_evidence.is_none()
        && operation
            .effects
            .as_ref()
            .is_none_or(|effects| effects.effects.is_empty())
        && prior.active_attempt_id.as_deref() != Some(attempt.attempt_id.as_str())
}

fn validate_cumulative_revision_scopes(
    prior: &FinancialPlanAuthorizationRecord,
    next_spec: &financial_plans::FinancialPlanSpec,
    next_permissions: &[FinancialPlanStepPermission],
    store: &Store,
) -> Result<()> {
    let mut historical = std::collections::BTreeMap::<
        String,
        gofin_domain::financial_plans::FinancialPlanCumulativeScope,
    >::new();
    let mut revisions = store
        .financial_plan_revisions(&prior.revision.plan_id)
        .map_err(map_store)?
        .into_iter()
        .map(from_value::<FinancialPlanRevision>)
        .collect::<Result<Vec<_>>>()?;
    if !revisions
        .iter()
        .any(|revision| revision.revision_id == prior.revision.revision_id)
    {
        revisions.push(prior.revision.clone());
    }
    for (revision, permission) in revisions.iter().flat_map(|revision| {
        revision
            .permissions
            .iter()
            .map(move |permission| (revision, permission))
    }) {
        let Some(scope) = permission
            .permission
            .as_ref()
            .and_then(|permission| permission.cumulative_scope.as_ref())
        else {
            continue;
        };
        if revision
            .spec
            .steps
            .iter()
            .any(|step| step.step_id == permission.step_id && step.operation == "bridge.execute")
        {
            // Nominated bridge route and fee/refund constraints are amendable
            // until execution begins. Their old snapshots are historical, not
            // shared parent caps that constrain a still-unstarted intent.
            continue;
        }
        if historical
            .insert(scope.scope_id.clone(), scope.clone())
            .is_some_and(|previous| previous != *scope)
        {
            return Err(plan_error(
                "PLAN_CUMULATIVE_SCOPE_CONFLICT",
                "Prior plan revisions disagree about a shared adapter parent cap.",
                "Keep the original cumulative scope definition across revisions.",
            ));
        }
    }
    for permission in next_permissions {
        let Some(scope) = permission
            .permission
            .as_ref()
            .and_then(|value| value.cumulative_scope.as_ref())
        else {
            continue;
        };
        let is_bridge_execute =
            next_spec.steps.iter().any(|step| {
                step.step_id == permission.step_id && step.operation == "bridge.execute"
            }) && prior.revision.spec.steps.iter().any(|step| {
                step.step_id == permission.step_id && step.operation == "bridge.execute"
            });
        if is_bridge_execute {
            let previous_scope = prior
                .revision
                .permissions
                .iter()
                .find(|old| old.step_id == permission.step_id)
                .and_then(|old| old.permission.as_ref())
                .and_then(|old| old.cumulative_scope.as_ref());
            if bridge_execute_scope_is_unstarted(prior, &permission.step_id, store)? {
                continue;
            }
            if previous_scope != Some(scope) {
                return Err(plan_error(
                    "PLAN_CUMULATIVE_SCOPE_CHANGED",
                    "A started bridge intent cannot reset or widen its reviewed cumulative limits.",
                    "Preserve the original bridge route and cumulative limits after an attempt or claim exists.",
                ));
            }
            continue;
        }
        if historical
            .get(&scope.scope_id)
            .is_some_and(|old| old != scope)
        {
            return Err(plan_error(
                "PLAN_CUMULATIVE_SCOPE_CHANGED",
                "A plan revision cannot reset or widen a previously reviewed cumulative parent cap.",
                "Preserve the original cumulative scope identity and exact limits.",
            ));
        }
    }
    Ok(())
}

fn bridge_execute_scope_is_unstarted(
    prior: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    store: &Store,
) -> Result<bool> {
    if prior
        .attempts
        .iter()
        .any(|attempt| attempt.step_id == step_id)
        || prior
            .usage
            .reservations
            .iter()
            .any(|reservation| reservation.step_id == step_id)
        || prior
            .usage
            .verified_proceeds
            .iter()
            .any(|proceeds| proceeds.producing_step_id == step_id)
    {
        return Ok(false);
    }
    for transfer in store.bridge_transfers().map_err(map_store)? {
        if transfer.steps.iter().any(|link| {
            link.plan_authority.as_ref().is_some_and(|authority| {
                authority.plan_id == prior.revision.plan_id && authority.plan_step_id == step_id
            })
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_active_bridge_revision_scopes(
    prior: &FinancialPlanAuthorizationRecord,
    next: &financial_plans::FinancialPlanSpec,
    next_permissions: &[FinancialPlanStepPermission],
    store: &Store,
) -> Result<()> {
    let transfers = store.bridge_transfers().map_err(map_store)?;
    validate_bridge_revision_scope_records(prior, next, next_permissions, &transfers)
}

fn validate_bridge_revision_scope_records(
    prior: &FinancialPlanAuthorizationRecord,
    next: &financial_plans::FinancialPlanSpec,
    next_permissions: &[FinancialPlanStepPermission],
    transfers: &[gofin_domain::BridgeTransferRecord],
) -> Result<()> {
    for transfer in transfers.iter().filter(|transfer| {
        transfer
            .plan_scope
            .as_ref()
            .is_some_and(|scope| scope.plan_id == prior.revision.plan_id)
            && !matches!(
                transfer.state,
                gofin_domain::BridgeTransferState::Completed
                    | gofin_domain::BridgeTransferState::Refunded
                    | gofin_domain::BridgeTransferState::Failed
            )
    }) {
        let bridge_execute_steps = transfer
            .steps
            .iter()
            .filter_map(|link| {
                let authority = link.plan_authority.as_ref()?;
                (authority.plan_id == prior.revision.plan_id
                    && link.state != gofin_domain::BridgeStepState::Unsubmitted)
                    .then_some(authority.plan_step_id.clone())
            })
            .filter(|step_id| {
                prior
                    .revision
                    .spec
                    .steps
                    .iter()
                    .any(|step| step.step_id == *step_id && step.operation == "bridge.execute")
            })
            .collect::<std::collections::BTreeSet<_>>();
        let has_unsubmitted_bridge_execute = transfer.steps.iter().any(|link| {
            link.state == gofin_domain::BridgeStepState::Unsubmitted
                && link.plan_authority.as_ref().is_some_and(|authority| {
                    authority.plan_id == prior.revision.plan_id
                        && prior.revision.spec.steps.iter().any(|step| {
                            step.step_id == authority.plan_step_id
                                && step.operation == "bridge.execute"
                        })
                })
        });
        if bridge_execute_steps.is_empty() && has_unsubmitted_bridge_execute {
            continue;
        }
        if !bridge_execute_steps.is_empty() {
            for step_id in bridge_execute_steps {
                let old = prior.revision.permissions.iter().find(|permission| {
                    permission.step_id == step_id
                        && prior.revision.spec.steps.iter().any(|step| {
                            step.step_id == step_id && step.operation == "bridge.execute"
                        })
                });
                let Some(old) = old else {
                    return Err(plan_error(
                        "PLAN_BRIDGE_PARENT_SCOPE_MISSING",
                        "A submitted bridge intent is absent from the prior authorized revision.",
                        "Preserve the bridge intent permission while reconciling its saved transfer.",
                    ));
                };
                if next_permissions
                    .iter()
                    .find(|permission| permission.step_id == step_id)
                    != Some(old)
                {
                    return Err(plan_error(
                        "PLAN_BRIDGE_PARENT_SCOPE_CHANGED",
                        "A submitted bridge intent cannot lose or change its route and cumulative limits during plan renewal.",
                        "Preserve the original bridge intent permission while reconciling its saved transfer.",
                    ));
                }
            }
            continue;
        }
        let exact_step = |permission: &FinancialPlanStepPermission| {
            permission.exact.as_ref()?;
            prior.revision.spec.steps.iter().find(|step| {
                step.step_id == permission.step_id
                    && step.operation == "bridge.transfer"
                    && step.input["transferId"].as_str() == Some(transfer.id.as_str())
            })
        };
        let prior_permissions = prior.revision.permissions.iter().filter(|permission| {
            permission
                .permission
                .as_ref()
                .and_then(|permission| permission.cumulative_scope.as_ref())
                .is_some_and(|scope| scope.scope_id == transfer.id)
                || exact_step(permission).is_some()
        });
        let mut found = false;
        for old in prior_permissions {
            found = true;
            let replacement = next_permissions
                .iter()
                .find(|permission| permission.step_id == old.step_id);
            if let Some(old_step) = exact_step(old) {
                // Exact mode has no adapter cumulative_scope. Its reviewed
                // transfer/child input is the durable parent association.
                let link = transfer.steps.iter().find(|link| {
                    old_step.input["stepId"].as_str() == Some(link.step_id.as_str())
                        && old_step.input["stepKind"] == json!(link.kind)
                });
                let Some(link) = link else {
                    return Err(plan_error(
                        "PLAN_BRIDGE_PARENT_SCOPE_MISSING",
                        "An exact bridge permission does not identify a child of its saved parent.",
                        "Preserve the original saved transfer, child identity and kind.",
                    ));
                };
                if replacement.is_none()
                    && link.state == gofin_domain::BridgeStepState::Superseded
                    && link.operation_id.is_none()
                    && link.kind.is_destination_stage()
                {
                    continue;
                }
                let replacement_step = next.steps.iter().find(|step| step.step_id == old.step_id);
                if replacement != Some(old) || replacement_step != Some(old_step) {
                    return Err(plan_error(
                        "PLAN_BRIDGE_PARENT_SCOPE_CHANGED",
                        "A live exact bridge parent cannot lose or change its reviewed child input, bounds or permission during renewal.",
                        "Keep the saved transfer and child IDs, route, bounds and accounting declarations unchanged while renewing authority.",
                    ));
                }
                continue;
            }
            let superseded_unsubmitted = old
                .permission
                .as_ref()
                .and_then(|permission| {
                    permission.spec.fixed["plan_step_identity"]["step_id"].as_str()
                })
                .is_some_and(|id| {
                    transfer.steps.iter().any(|link| {
                        link.step_id == id
                            && link.state == gofin_domain::BridgeStepState::Superseded
                            && link.operation_id.is_none()
                            && link.kind.is_destination_stage()
                    })
                });
            if replacement.is_none() && superseded_unsubmitted {
                // Only the store-validated deferred recovery recipe can retire
                // this never-attempted stage. Started writes retain every
                // original permission; recovery adds explicit new identities.
                continue;
            }
            if replacement != Some(old) {
                return Err(plan_error(
                    "PLAN_BRIDGE_PARENT_SCOPE_CHANGED",
                    "A live plan-linked bridge parent cannot lose or change any declared step permission during plan renewal.",
                    "Preserve the complete bridge parent topology and selected bounds while renewing the remaining plan authorization.",
                ));
            }
        }
        if !found {
            return Err(plan_error(
                "PLAN_BRIDGE_PARENT_SCOPE_MISSING",
                "A live plan-linked bridge parent is absent from the current authorized revision.",
                "Restore its exact bridge steps and cumulative parent scope before authorizing further plan work.",
            ));
        }
    }
    Ok(())
}

fn ensure_bounds_cover_usage(
    maximum_debits: &[ResourceAmount],
    consumed_debits: &[ResourceAmount],
    maximum_costs: &[CostBound],
    consumed_costs: &[CostBound],
) -> Result<()> {
    for consumed in consumed_debits {
        if !maximum_debits.iter().any(|cap| {
            cap.resource == consumed.resource
                && cap.amount.asset == consumed.amount.asset
                && cap.amount.atoms >= consumed.amount.atoms
        }) {
            return Err(plan_error(
                "PLAN_REVISION_BELOW_USAGE",
                "A revised step debit bound is below verified consumption.",
                "Keep started-step limits at or above verified consumption.",
            ));
        }
    }
    for consumed in consumed_costs {
        if !maximum_costs.iter().any(|cap| {
            cap.location == consumed.location
                && cap.amount.asset == consumed.amount.asset
                && cap.treatment == consumed.treatment
                && cap.amount.atoms >= consumed.amount.atoms
        }) {
            return Err(plan_error(
                "PLAN_REVISION_BELOW_USAGE",
                "A revised step fee bound is below verified consumption.",
                "Keep started-step limits at or above verified consumption.",
            ));
        }
    }
    Ok(())
}

fn expire_plan_if_needed(
    engine: &Engine,
    record: &mut FinancialPlanAuthorizationRecord,
) -> Result<()> {
    if record.revision.status == FinancialPlanAuthorizationStatus::Active
        && parse_time(&record.revision.expires_at)? <= engine.clock.now()
    {
        let expected = record.clone();
        record.revision.status = FinancialPlanAuthorizationStatus::Expired;
        engine
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .compare_update_financial_plan(
                &record.revision.plan_id,
                &to_value(&expected)?,
                &to_value(record)?,
                &engine.clock.now().to_rfc3339(),
            )
            .map_err(map_store)?;
    }
    Ok(())
}

fn plan_duration(seconds: u64) -> Result<chrono::Duration> {
    let seconds = i64::try_from(seconds).map_err(|_| {
        plan_error(
            "PLAN_DURATION_INVALID",
            "Plan duration cannot be represented.",
            "Choose a shorter positive duration.",
        )
    })?;
    let duration = chrono::Duration::try_seconds(seconds).ok_or_else(|| {
        plan_error(
            "PLAN_DURATION_INVALID",
            "Plan duration cannot be represented.",
            "Choose a shorter positive duration.",
        )
    })?;
    Utc::now().checked_add_signed(duration).ok_or_else(|| {
        plan_error(
            "PLAN_DURATION_INVALID",
            "Plan expiry cannot be represented.",
            "Choose a shorter positive duration.",
        )
    })?;
    Ok(duration)
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

#[cfg(test)]
pub(super) mod tests {
    #[test]
    fn quote_expiry_can_refresh_but_unclassified_cost_evidence_cannot_loop() {
        let error = |code| super::plan_error(code, "fixture", "Inspect evidence.");
        assert!(super::can_auto_refresh_plan_candidate(&error(
            "BRIDGE_QUOTE_EXPIRED"
        )));
        assert!(!super::can_auto_refresh_plan_candidate(&error(
            "COST_VALUATION_STALE"
        )));
        assert!(!super::can_auto_refresh_plan_candidate(&error(
            "BRIDGE_REFRESH_UNSAFE"
        )));
        assert!(!super::can_auto_refresh_plan_candidate(&error(
            "BRIDGE_REFRESH_SCOPE_CHANGED"
        )));
    }

    mod exact_bridge_revision {
        include!("financial_plans_exact_bridge_revision_tests.rs");
    }
    mod bridge_allowance_funding {
        include!("financial_plans_bridge_funding_tests.rs");
    }
    mod early_reservations {
        include!("financial_plan_reservation_tests.rs");
    }
    use super::*;
    use gofin_domain::financial_plans::PlanPermission;
    use gofin_domain::{
        AccountingLocation, Amount, AssetId, CostTreatment, FinancialEffect, Quantity, ResourceId,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[test]
    fn recovery_children_may_form_only_a_contiguous_parent_bound_chain() {
        let spec: financial_plans::FinancialPlanSpec = serde_json::from_value(json!({
            "version":1,"name":"recovery chain","steps":[
                {"step_id":"create","operation":"order.create","input":{"wallet":"w"},"permitted_variations":{},"completion":{"operation_succeeded":true},"funding_dependencies":[]},
                {"step_id":"update","operation":"order.update","input":{"original_operation_id":null,"original_submission":null},"permitted_variations":{},"completion":{"operation_succeeded":true},"funding_dependencies":[]},
                {"step_id":"cancel","operation":"order.cancel","input":{"original_operation_id":null,"original_submission":null},"permitted_variations":{},"completion":{"operation_succeeded":true},"funding_dependencies":[]},
                {"step_id":"next-write","operation":"wallet.transfer","input":{},"permitted_variations":{},"completion":{"operation_succeeded":true},"funding_dependencies":[]}
            ],"aggregate_bounds":{"maximum_debits":[],"maximum_costs":[]}
        })).unwrap();
        let permission = |step_id: &str, parent: Option<&str>| {
            let recovery_parent =
                parent.map(|step_id| financial_plans::FinancialPlanRecoveryParent {
                    step_id: step_id.to_owned(),
                    operation_id_pointer: "/original_operation_id".into(),
                    submission_pointer: "/original_submission".into(),
                    parent_binding_fields: Vec::new(),
                });
            FinancialPlanStepPermission {
                step_id: step_id.to_owned(),
                generated_input_fields: Vec::new(),
                exact: None,
                permission: Some(PlanPermission {
                    spec: gofin_domain::boundaries::BoundarySpec {
                        fixed: json!({}),
                        parameters: json!({}),
                        maximum_debits: Vec::new(),
                        maximum_costs: Vec::new(),
                        summary: "synthetic recovery test".into(),
                    },
                    recovery_parent,
                    possible_credits: Vec::new(),
                    fee_assets: Vec::new(),
                    deferred_receipt: None,
                    authorization_seconds: None,
                    cumulative_scope: None,
                }),
            }
        };
        let permissions = vec![
            permission("create", None),
            permission("update", Some("create")),
            permission("cancel", Some("create")),
            permission("next-write", None),
        ];
        assert!(validate_plan_recovery_permissions(&spec, &permissions).is_ok());
        let mut gapped = permissions.clone();
        gapped[1] = permission("update", None);
        assert_eq!(
            validate_plan_recovery_permissions(&spec, &gapped)
                .unwrap_err()
                .code,
            "PLAN_RECOVERY_PARENT_INVALID"
        );
        let mut wrong_parent = permissions;
        wrong_parent[2] = permission("cancel", Some("update"));
        assert_eq!(
            validate_plan_recovery_permissions(&spec, &wrong_parent)
                .unwrap_err()
                .code,
            "PLAN_RECOVERY_PARENT_INVALID"
        );
    }
    include!("plan_revision_repair_tests.rs");
    include!("non_cash_plan_source_tests.rs");
    include!("exact_zero_cost_tests.rs");

    fn asset(reference: &str) -> AssetId {
        AssetId {
            network: "synthetic".into(),
            reference: reference.into(),
            decimals: 2,
        }
    }

    struct GenericReadAdapter {
        definition_override: Mutex<Option<gofin_domain::Definition>>,
        reads: AtomicUsize,
        prepares: AtomicUsize,
        changed_connection: AtomicBool,
        changed_authorization_identity: AtomicBool,
        fail_read: AtomicBool,
        read_gate: Option<(Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>)>,
    }

    impl GenericReadAdapter {
        fn definition() -> gofin_domain::Definition {
            gofin_domain::Definition {
                name: "future.read".into(),
                description: "Synthetic future read".into(),
                kind: gofin_domain::OperationKind::Read,
                execution_actor: gofin_domain::ExecutionActor::AgentReadable,
                input_schema: json!({
                    "$schema":"https://json-schema.org/draft/2020-12/schema",
                    "type":"object",
                    "properties":{"account":{"type":"string"}},
                    "required":["account"],
                    "additionalProperties":false
                }),
                output_schema: json!({
                    "$schema":"https://json-schema.org/draft/2020-12/schema",
                    "type":"object",
                    "properties":{"value":{"type":"integer"}},
                    "required":["value"],
                    "additionalProperties":false
                }),
                error_schema: json!({"type":"object"}),
                examples: vec![],
                coverage: "synthetic".into(),
                completion: "read only".into(),
                semantics: json!({"contract":"v1"}),
            }
        }

        fn connection(&self) -> gofin_domain::ConnectionMetadata {
            gofin_domain::ConnectionMetadata {
                name: "synthetic account".into(),
                provider: "synthetic-provider".into(),
                resource: ResourceId("synthetic:account:one".into()),
                role: Some("read-only".into()),
                parent: None,
                network: Some("synthetic".into()),
                address: Some("account-1".into()),
                credential_ref: Some("synthetic:credential".into()),
                signing_enabled: false,
                details: json!({
                    "display_summary": if self.changed_connection.load(Ordering::SeqCst) {
                        "Primary synthetic account"
                    } else {
                        "Synthetic account"
                    }
                }),
            }
        }

        fn authorization_identity(&self) -> gofin_domain::ConnectionMetadata {
            gofin_domain::ConnectionMetadata {
                name: "protected account UID".into(),
                provider: "synthetic-provider".into(),
                resource: ResourceId("synthetic:account:one".into()),
                role: Some("protected-account-identity".into()),
                parent: None,
                network: Some("synthetic".into()),
                address: Some("account-1".into()),
                credential_ref: Some("synthetic:credential".into()),
                signing_enabled: false,
                details: json!({
                    "account_uid": if self.changed_authorization_identity.load(Ordering::SeqCst) {
                        "hidden-uid-2"
                    } else {
                        "hidden-uid-1"
                    }
                }),
            }
        }
    }

    #[async_trait::async_trait]
    impl gofin_domain::Adapter for GenericReadAdapter {
        fn definitions(&self) -> Vec<gofin_domain::Definition> {
            vec![
                self.definition_override
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(Self::definition),
            ]
        }

        fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            vec![self.connection()]
        }

        fn authorization_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            vec![self.authorization_identity()]
        }

        async fn read(&self, _operation: &str, _input: &Value) -> Result<Value> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if let Some((started, release)) = &self.read_gate {
                started.add_permits(1);
                release
                    .acquire()
                    .await
                    .map_err(|_| internal("read test gate closed"))?
                    .forget();
            }
            if self.fail_read.load(Ordering::SeqCst) {
                return Err(GoFinError::validation("synthetic read failure"));
            }
            Ok(json!({"value":7}))
        }

        async fn prepare(&self, _operation: &str, _input: &Value) -> Result<PreparedOperation> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Err(GoFinError::validation(
                "generic exact read must not prepare",
            ))
        }

        async fn revalidate(&self, _prepared: &PreparedOperation) -> Result<PreparedOperation> {
            Err(GoFinError::validation("read must not revalidate"))
        }

        async fn submit(
            &self,
            _prepared: &PreparedOperation,
            _journal: Arc<dyn gofin_domain::SubmissionJournal>,
        ) -> Result<Submission> {
            Err(GoFinError::validation("read must not submit"))
        }

        async fn reconcile(&self, _record: &OperationRecord) -> Result<Reconciliation> {
            Err(GoFinError::validation("read must not reconcile"))
        }
    }

    #[derive(Clone, Copy)]
    enum PostPersistMutation {
        None,
        Revoke,
        Expire,
        TamperBinding,
    }

    struct PostPersistGuardAdapter {
        store: Arc<Mutex<Store>>,
        plan_id: Mutex<Option<String>>,
        mutation: PostPersistMutation,
        submissions: AtomicUsize,
        complete_on_reconcile: bool,
    }

    impl PostPersistGuardAdapter {
        fn definition() -> gofin_domain::Definition {
            gofin_domain::Definition {
                name: "synthetic.postpersist".into(),
                description: "synthetic post-persist dispatch guard test".into(),
                kind: OperationKind::Write,
                execution_actor: gofin_domain::ExecutionActor::AgentInvocable,
                input_schema: json!({"type":"object","additionalProperties":true}),
                output_schema: json!({"type":"object"}),
                error_schema: json!({"type":"object"}),
                examples: vec![],
                coverage: "synthetic guard test".into(),
                completion: "synthetic".into(),
                semantics: json!({}),
            }
        }

        fn mutate_persisted_plan(&self) -> Result<()> {
            if matches!(self.mutation, PostPersistMutation::None) {
                return Ok(());
            }
            let plan_id = self
                .plan_id
                .lock()
                .map_err(|_| internal("test plan ID lock poisoned"))?
                .clone()
                .ok_or_else(|| internal("test plan ID missing"))?;
            let mut store = self
                .store
                .lock()
                .map_err(|_| internal("test store lock poisoned"))?;
            let current = store
                .financial_plan(&plan_id)
                .map_err(map_store)?
                .ok_or_else(|| internal("test plan missing"))?;
            let mut updated = current.clone();
            match self.mutation {
                PostPersistMutation::None => {}
                PostPersistMutation::Revoke => {
                    updated["revision"]["status"] = json!("revoked");
                }
                PostPersistMutation::Expire => {
                    updated["revision"]["expires_at"] = json!("2000-01-01T00:00:00Z");
                }
                PostPersistMutation::TamperBinding => {
                    updated["attempts"][0]["execution_binding_hash"] = json!("tampered");
                }
            }
            store
                .compare_update_financial_plan(&plan_id, &current, &updated, &now())
                .map_err(map_store)
        }
    }

    #[async_trait::async_trait]
    impl gofin_domain::Adapter for PostPersistGuardAdapter {
        fn definitions(&self) -> Vec<gofin_domain::Definition> {
            vec![Self::definition()]
        }

        async fn read(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(json!({}))
        }

        async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
            Ok(PreparedOperation {
                operation: operation.into(),
                input: input.clone(),
                binding: json!({"message":"fixed candidate"}),
                summary: "synthetic exact candidate".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: now(),
                expires_at: (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
                valuation: Some(gofin_domain::Valuation {
                    movement_usd_micros: Quantity::zero(),
                    maximum_fee_usd_micros: Quantity::zero(),
                    additional_allocation_usd_micros: Quantity::zero(),
                    observed_at: now(),
                    source: "synthetic no-cost plan test".into(),
                    prices: vec![],
                }),
                intent: FinancialIntent::default(),
                operation_version: 1,
                action_deadline: None,
            })
        }

        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            Ok(prepared.clone())
        }

        async fn submit(
            &self,
            prepared: &PreparedOperation,
            journal: Arc<dyn gofin_domain::SubmissionJournal>,
        ) -> Result<Submission> {
            journal.assert_candidate(prepared)?;
            let sequence = self.submissions.load(Ordering::SeqCst) + 1;
            let submission = Submission {
                kind: "synthetic-chain".into(),
                reference: format!("synthetic-message-hash-{sequence}"),
                data: json!({"message":"fixed"}),
            };
            journal.persist_submission(&submission)?;
            self.submissions.fetch_add(1, Ordering::SeqCst);
            self.mutate_persisted_plan()?;
            journal.assert_candidate(prepared)?;
            Ok(submission)
        }

        async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
            if self.complete_on_reconcile {
                let mut verified = VerifiedEffects {
                    evidence_version: 1,
                    submission: record
                        .submission
                        .clone()
                        .ok_or_else(|| internal("synthetic submission missing"))?,
                    effects: vec![],
                    evidence: json!({"synthetic":"completed plan step"}),
                    observed_at: now(),
                    proof_digest: String::new(),
                };
                verified.proof_digest = verified_effect_digest(&verified)?;
                Ok(Reconciliation {
                    status: Status::Completed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified),
                })
            } else {
                Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"uncertain":true}),
                    verified: None,
                })
            }
        }
    }

    fn post_persist_plan_request(request_id: &str) -> FinancialPlanAuthorizationRequest {
        FinancialPlanAuthorizationRequest {
            request_id: request_id.into(),
            plan: gofin_domain::financial_plans::FinancialPlanSpec {
                version: gofin_domain::financial_plans::FINANCIAL_PLAN_VERSION,
                name: "Post-persist guard".into(),
                summary: Some("Synthetic post-persist authorization test".into()),
                steps: vec![gofin_domain::financial_plans::FinancialPlanStepSpec {
                    step_id: "write".into(),
                    operation: "synthetic.postpersist".into(),
                    input: json!({"value":"fixed"}),
                    permitted_variations: json!({
                        "mode":"exact",
                        "maximum_debits":[],
                        "maximum_costs":[],
                        "possible_credits":[]
                    }),
                    completion: gofin_domain::financial_plans::FinancialStepCompletion {
                        principal_spend: None,
                        minimum_net_receipt: None,
                        operation_succeeded: true,
                    },
                    funding_dependencies: vec![],
                    action_deadline_seconds: None,
                }],
                aggregate_bounds: gofin_domain::financial_plans::FinancialPlanBounds {
                    maximum_debits: vec![],
                    maximum_costs: vec![],
                },
            },
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        }
    }

    struct PlanTestAuthorizer;

    struct YoloPlanAuthorizer {
        enabled: Arc<AtomicBool>,
        disable_after_authorize: bool,
    }

    struct PlanYoloLease(gofin_domain::YoloAuthorizationProvenance);

    impl gofin_domain::SubmissionAuthorityLease for PlanYoloLease {
        fn yolo_provenance(&self) -> Option<gofin_domain::YoloAuthorizationProvenance> {
            Some(self.0.clone())
        }
    }

    #[async_trait::async_trait]
    impl gofin_domain::Authorizer for YoloPlanAuthorizer {
        async fn authorize(
            &self,
            challenge: &AuthorizationChallenge,
        ) -> Result<gofin_domain::ApprovalAudit> {
            let audit = gofin_domain::ApprovalAudit {
                source: Some(gofin_domain::AuthorizationSource::Yolo),
                yolo_provenance: Some(gofin_domain::YoloAuthorizationProvenance {
                    revision: 12,
                    generation: "SYNTHETIC-plan-yolo-generation".into(),
                    checked_at: now(),
                }),
                mechanism: "protected-yolo-plan-test".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            };
            if self.disable_after_authorize {
                self.enabled.store(false, Ordering::SeqCst);
            }
            Ok(audit)
        }

        fn acquire_submission_authority(
            &self,
            audit: &gofin_domain::ApprovalAudit,
        ) -> Result<Box<dyn gofin_domain::SubmissionAuthorityLease>> {
            if !self.enabled.load(Ordering::SeqCst) {
                return Err(GoFinError::new(
                    "YOLO_DISABLED",
                    "authorization",
                    "Protected Yolo authority is disabled.",
                    "Use the standard approval flow.",
                ));
            }
            let original = audit
                .yolo_provenance
                .as_ref()
                .ok_or_else(|| GoFinError::validation("Yolo plan audit provenance is missing."))?;
            Ok(Box::new(PlanYoloLease(
                gofin_domain::YoloAuthorizationProvenance {
                    revision: original.revision,
                    generation: original.generation.clone(),
                    checked_at: now(),
                },
            )))
        }
    }

    #[async_trait::async_trait]
    impl gofin_domain::Authorizer for PlanTestAuthorizer {
        async fn authorize(
            &self,
            challenge: &AuthorizationChallenge,
        ) -> Result<gofin_domain::ApprovalAudit> {
            Ok(gofin_domain::ApprovalAudit {
                source: None,
                yolo_provenance: None,
                mechanism: "synthetic-plan-test".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: gofin_domain::now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[tokio::test]
    async fn yolo_plan_revision_and_step_keep_protected_source_and_generation() {
        let enabled = Arc::new(AtomicBool::new(true));
        let store = Arc::new(Mutex::new(Store::in_memory().unwrap()));
        let adapter = Arc::new(PostPersistGuardAdapter {
            store: store.clone(),
            plan_id: Mutex::new(None),
            mutation: PostPersistMutation::None,
            submissions: AtomicUsize::new(0),
            complete_on_reconcile: true,
        });
        let engine = Engine::new_with_shared_store(
            store,
            vec![adapter.clone()],
            Arc::new(YoloPlanAuthorizer {
                enabled: enabled.clone(),
                disable_after_authorize: false,
            }),
            EngineOptions::default(),
        )
        .unwrap();

        let mut initial = post_persist_plan_request("protected-plan-create");
        let mut second_step = initial.plan.steps[0].clone();
        second_step.step_id = "write-second".into();
        second_step.input = json!({"value":"second"});
        initial.plan.steps.push(second_step.clone());
        let review = engine
            .preview_financial_plan(initial.clone())
            .await
            .unwrap();
        initial.expected_review_hash = Some(review.review_hash);
        let first = engine.authorize_financial_plan(initial).await.unwrap();
        assert_eq!(
            first.revision.authorization_source,
            Some(gofin_domain::AuthorizationSource::Yolo)
        );
        assert_eq!(
            first.revision.yolo_provenance.as_ref().unwrap().revision,
            12
        );

        let mut replacement = post_persist_plan_request("protected-plan-revision");
        replacement.plan.steps.push(second_step);
        replacement.replaces_revision_id = Some(first.revision.revision_id.clone());
        let review = engine
            .preview_financial_plan(replacement.clone())
            .await
            .unwrap();
        replacement.expected_review_hash = Some(review.review_hash);
        let revised = engine.authorize_financial_plan(replacement).await.unwrap();
        assert_eq!(revised.plan_id, first.plan_id);
        assert_ne!(revised.revision.revision_id, first.revision.revision_id);
        assert_eq!(
            revised.revision.authorization_source,
            Some(gofin_domain::AuthorizationSource::Yolo)
        );
        assert_eq!(
            revised
                .revision
                .yolo_provenance
                .as_ref()
                .unwrap()
                .generation,
            "SYNTHETIC-plan-yolo-generation"
        );
        *adapter.plan_id.lock().unwrap() = Some(revised.plan_id.clone());

        assert_eq!(
            engine
                .execute_financial_plan_step(
                    &revised.plan_id,
                    "write-second",
                    OperationRequest {
                        request_id: "protected-plan-step-out-of-order".into(),
                        operation: "synthetic.postpersist".into(),
                        input: json!({"value":"second"}),
                        run_id: None,
                    },
                )
                .await
                .unwrap_err()
                .code,
            "PLAN_STEP_NOT_ELIGIBLE"
        );
        assert_eq!(
            engine
                .execute_financial_plan_step(
                    &revised.plan_id,
                    "write",
                    OperationRequest {
                        request_id: "protected-plan-step-out-of-scope".into(),
                        operation: "synthetic.postpersist".into(),
                        input: json!({"value":"outside-exact-scope"}),
                        run_id: None,
                    },
                )
                .await
                .unwrap_err()
                .code,
            "PLAN_STEP_SCOPE_MISMATCH"
        );

        let attempt = engine
            .execute_financial_plan_step(
                &revised.plan_id,
                "write",
                OperationRequest {
                    request_id: "protected-plan-step".into(),
                    operation: "synthetic.postpersist".into(),
                    input: json!({"value":"fixed"}),
                    run_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(attempt.status, Status::Completed, "{attempt:?}");
        assert_eq!(
            attempt.approval.as_ref().unwrap().source,
            Some(gofin_domain::AuthorizationSource::Yolo)
        );
        assert_eq!(
            attempt
                .approval
                .as_ref()
                .unwrap()
                .yolo_provenance
                .as_ref()
                .unwrap()
                .revision,
            12
        );
        assert!(
            attempt
                .approval
                .as_ref()
                .unwrap()
                .yolo_provenance
                .as_ref()
                .unwrap()
                .checked_at
                > revised
                    .revision
                    .yolo_provenance
                    .as_ref()
                    .unwrap()
                    .checked_at
        );
        assert_eq!(
            engine
                .financial_plan_status(&revised.plan_id)
                .await
                .unwrap()
                .next_eligible_step_id
                .as_deref(),
            Some("write-second")
        );
        let second = engine
            .execute_financial_plan_step(
                &revised.plan_id,
                "write-second",
                OperationRequest {
                    request_id: "protected-plan-step-second".into(),
                    operation: "synthetic.postpersist".into(),
                    input: json!({"value":"second"}),
                    run_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(second.status, Status::Completed, "{second:?}");
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn sudo_cannot_create_or_reuse_named_plan_authority() {
        struct SudoPlanAuthorizer;
        #[async_trait::async_trait]
        impl gofin_domain::Authorizer for SudoPlanAuthorizer {
            async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
                let mut audit =
                    gofin_domain::Authorizer::authorize(&PlanTestAuthorizer, challenge).await?;
                audit.source = Some(gofin_domain::AuthorizationSource::Sudo);
                audit.mechanism = "sudo-terminal-review".into();
                Ok(audit)
            }
        }
        for malicious_creation in [true, false] {
            let store = Arc::new(Mutex::new(Store::in_memory().unwrap()));
            let adapter = Arc::new(PostPersistGuardAdapter {
                store: store.clone(),
                plan_id: Mutex::new(None),
                mutation: PostPersistMutation::None,
                submissions: AtomicUsize::new(0),
                complete_on_reconcile: true,
            });
            let authorizer: Arc<dyn gofin_domain::Authorizer> = if malicious_creation {
                Arc::new(SudoPlanAuthorizer)
            } else {
                Arc::new(PlanTestAuthorizer)
            };
            let engine = Engine::new_with_shared_store(
                store.clone(),
                vec![adapter.clone()],
                authorizer,
                EngineOptions::default(),
            )
            .unwrap();
            let mut request = post_persist_plan_request("sudo-plan-authorization");
            request.expected_review_hash = Some(
                engine
                    .preview_financial_plan(request.clone())
                    .await
                    .unwrap()
                    .review_hash,
            );
            if malicious_creation {
                assert_eq!(
                    engine
                        .authorize_financial_plan(request)
                        .await
                        .unwrap_err()
                        .code,
                    "SUDO_CONFIGURATION_ONLY"
                );
            } else {
                let authorized = engine.authorize_financial_plan(request).await.unwrap();
                let original = engine.load_financial_plan(&authorized.plan_id).unwrap();
                let mut changed = original.clone();
                changed.approval.source = Some(gofin_domain::AuthorizationSource::Sudo);
                store
                    .lock()
                    .unwrap()
                    .compare_update_financial_plan(
                        &authorized.plan_id,
                        &to_value(&original).unwrap(),
                        &to_value(&changed).unwrap(),
                        &now(),
                    )
                    .unwrap();
                let error = engine
                    .execute_financial_plan_step(
                        &authorized.plan_id,
                        "write",
                        OperationRequest {
                            request_id: "sudo-plan-attempt".into(),
                            operation: "synthetic.postpersist".into(),
                            input: json!({"value":"fixed"}),
                            run_id: None,
                        },
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.code, "SUDO_CONFIGURATION_ONLY");
                assert_eq!(
                    engine
                        .load_financial_plan(&authorized.plan_id)
                        .unwrap()
                        .approval
                        .source,
                    Some(gofin_domain::AuthorizationSource::Sudo)
                );
            }
            assert_eq!(adapter.submissions.load(Ordering::SeqCst), 0);
            assert!(engine.operations().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn yolo_disable_after_plan_approval_prevents_authority_commit() {
        let enabled = Arc::new(AtomicBool::new(true));
        let store = Arc::new(Mutex::new(Store::in_memory().unwrap()));
        let adapter = Arc::new(PostPersistGuardAdapter {
            store: store.clone(),
            plan_id: Mutex::new(None),
            mutation: PostPersistMutation::None,
            submissions: AtomicUsize::new(0),
            complete_on_reconcile: false,
        });
        let engine = Engine::new_with_shared_store(
            store,
            vec![adapter],
            Arc::new(YoloPlanAuthorizer {
                enabled: enabled.clone(),
                disable_after_authorize: true,
            }),
            EngineOptions::default(),
        )
        .unwrap();
        let mut request = post_persist_plan_request("protected-plan-stale-commit");
        let review = engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap();
        request.expected_review_hash = Some(review.review_hash);
        let error = engine.authorize_financial_plan(request).await.unwrap_err();
        assert_eq!(error.code, "YOLO_DISABLED");
        assert!(!enabled.load(Ordering::SeqCst));
    }

    fn generic_read_request(request_id: &str) -> FinancialPlanAuthorizationRequest {
        FinancialPlanAuthorizationRequest {
            request_id: request_id.into(),
            plan: financial_plans::FinancialPlanSpec {
                version: financial_plans::FINANCIAL_PLAN_VERSION,
                name: "Generic exact read".into(),
                summary: None,
                steps: vec![financial_plans::FinancialPlanStepSpec {
                    action_deadline_seconds: None,
                    step_id: "read-account".into(),
                    operation: "future.read".into(),
                    input: json!({"account":"account-1"}),
                    permitted_variations: json!({
                        "mode":"exact",
                        "maximum_debits":[],
                        "maximum_costs":[],
                        "possible_credits":[]
                    }),
                    completion: financial_plans::FinancialStepCompletion {
                        principal_spend: None,
                        minimum_net_receipt: None,
                        operation_succeeded: true,
                    },
                    funding_dependencies: vec![],
                }],
                aggregate_bounds: financial_plans::FinancialPlanBounds {
                    maximum_debits: vec![],
                    maximum_costs: vec![],
                },
            },
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        }
    }

    #[tokio::test]
    async fn post_persist_dispatch_guard_does_not_rewrite_plan_and_still_checks_authority() {
        for (index, mutation, expected_error) in [
            (0, PostPersistMutation::None, None),
            (
                1,
                PostPersistMutation::Revoke,
                Some("PLAN_AUTHORIZATION_INACTIVE"),
            ),
            (
                2,
                PostPersistMutation::Expire,
                Some("PLAN_AUTHORIZATION_INACTIVE"),
            ),
            (
                3,
                PostPersistMutation::TamperBinding,
                Some("PLAN_CANDIDATE_CHANGED"),
            ),
        ] {
            let store = Arc::new(Mutex::new(Store::in_memory().unwrap()));
            let adapter = Arc::new(PostPersistGuardAdapter {
                store: store.clone(),
                plan_id: Mutex::new(None),
                mutation,
                submissions: AtomicUsize::new(0),
                complete_on_reconcile: false,
            });
            let engine = Engine::new_with_shared_store(
                store,
                vec![adapter.clone()],
                Arc::new(PlanTestAuthorizer),
                EngineOptions::default(),
            )
            .unwrap();
            let mut request = post_persist_plan_request(&format!("post-persist-{index}"));
            let review = engine
                .preview_financial_plan(request.clone())
                .await
                .unwrap();
            request.expected_review_hash = Some(review.review_hash);
            let authorized = engine.authorize_financial_plan(request).await.unwrap();
            *adapter.plan_id.lock().unwrap() = Some(authorized.plan_id.clone());
            let result = engine
                .execute_financial_plan_step(
                    &authorized.plan_id,
                    "write",
                    OperationRequest {
                        request_id: format!("post-persist-attempt-{index}"),
                        operation: "synthetic.postpersist".into(),
                        input: json!({"value":"fixed"}),
                        run_id: None,
                    },
                )
                .await
                .unwrap();
            if let Some(code) = expected_error {
                assert_eq!(
                    result.error.as_ref().map(|error| error.code.as_str()),
                    Some(code)
                );
                assert_eq!(result.status, Status::Pending);
            } else {
                assert_eq!(result.status, Status::Pending, "{result:?}");
                assert!(result.error.is_none());
                assert_eq!(
                    result
                        .submission
                        .as_ref()
                        .map(|submission| submission.reference.as_str()),
                    Some("synthetic-message-hash-1")
                );
            }
        }
    }

    #[tokio::test]
    async fn bridge_description_upgrade_accepts_saved_hash_and_explains_material_mismatch() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let mut current = GenericReadAdapter::definition();
        current.name = "bridge.transfer".into();
        current.semantics["request_identity"] = json!(
            "each step has its own operation ID; uncertain submissions reconcile under the original ID"
        );
        let mut historical = current.clone();
        historical.semantics["request_identity"] = json!(
            "each step has its own operation ID; uncertain submissions reconcile under the original ID; quote refresh stays on the persisted provider and route"
        );
        // Independent historical algorithm: do not generate old authority using
        // the new canonicalization helper.
        let historical_hash = binding_hash(&json!({"operation":historical.name,"kind":historical.kind,"execution_actor":historical.execution_actor,"input_schema":historical.input_schema,"semantics":historical.semantics})).unwrap();
        *adapter.definition_override.lock().unwrap() = Some(current.clone());
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let mut spec = generic_read_request("bridge-upgrade").plan;
        spec.steps[0].operation = current.name.clone();
        let permissions = engine
            .prepare_plan_permissions(&spec, None, &[])
            .await
            .unwrap();
        let mut exact = permissions[0].exact.clone().unwrap();
        assert_ne!(exact.definition_hash, historical_hash);
        exact.definition_hash = historical_hash;
        validate_exact_plan_identity(&*adapter, &current.name, &exact).unwrap();
        for (changed_contract, changed_identity) in [(true, false), (false, true), (true, true)] {
            let mut definition = current.clone();
            if changed_contract {
                definition.input_schema["required"] = json!(["different-authority"]);
            }
            *adapter.definition_override.lock().unwrap() = Some(definition);
            adapter
                .changed_authorization_identity
                .store(changed_identity, Ordering::SeqCst);
            let error = validate_exact_plan_identity(&*adapter, &current.name, &exact).unwrap_err();
            assert_eq!(error.code, "PLAN_OPERATION_CONTRACT_CHANGED");
            assert_eq!(
                error.details.as_deref().unwrap(),
                &json!({"definition_matches":!changed_contract,"connection_identity_matches":!changed_identity})
            );
        }
    }

    #[tokio::test]
    async fn withdrawal_discovery_upgrade_preserves_started_hash_and_contract_checks() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let mut current = GenericReadAdapter::definition();
        current.name = "kraken.get-withdrawal-addresses".into();
        current.semantics["withdrawal_discovery"] = json!({
            "preferred_api":"funding-v1",
            "venue_api_status":"Funding Beta in the pinned official schema; supported does not imply general availability",
            "methods_operation":"kraken.list-funding-methods-v1",
            "methods_direction":"withdraw",
            "addresses_operation":"kraken.list-funding-addresses-v1",
            "new_withdrawal_operation":"kraken.create-funding-withdrawal-v1",
            "address_selection":"Match the exact verified address, tag/memo, account and resolved method/network; network-group addresses require current membership proof.",
            "pagination":"Traverse next_cursor using cursor before concluding that an address or method is absent; a single page is not exhaustive.",
            "legacy_absence_is_authoritative":false,
            "automatic_api_translation":false,
            "recovery":"Preserve the original operation, API, submission identity and reviewed limits; never switch APIs or create a replacement to recover an uncertain withdrawal."
        });
        current.semantics["withdrawal_discovery"]["preferred_operation_for_new_workflow"] =
            json!("kraken.list-funding-addresses-v1");
        let transient = binding_hash(&json!({"operation":current.name,"kind":current.kind,
            "execution_actor":current.execution_actor,"input_schema":current.input_schema,"semantics":current.semantics})).unwrap();
        *adapter.definition_override.lock().unwrap() = Some(current.clone());
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let mut spec = generic_read_request("upgrade").plan;
        spec.steps[0].operation = current.name.clone();
        let canonical = engine
            .prepare_plan_permissions(&spec, None, &[])
            .await
            .unwrap();
        let original = canonical[0].exact.as_ref().unwrap().definition_hash.clone();
        assert_ne!(original, transient);
        for approved in [original, transient] {
            let mut prior = prior_record();
            prior.revision.spec = spec.clone();
            prior.revision.permissions = canonical.clone();
            prior.revision.permissions[0]
                .exact
                .as_mut()
                .unwrap()
                .definition_hash = approved.clone();
            prior.usage = FinancialPlanUsage::default();
            prior.attempts[0].step_id = "read-account".into();
            let exact = prior.revision.permissions[0].exact.as_ref().unwrap();
            validate_exact_plan_identity(&*adapter, &current.name, exact).unwrap();
            let next = engine
                .prepare_plan_permissions(&spec, Some(&prior), &[])
                .await
                .unwrap();
            assert_eq!(next[0].exact.as_ref().unwrap().definition_hash, approved);
            validate_revision_transition(
                Some(&prior),
                &spec,
                &next,
                &prior.usage,
                &Store::in_memory().unwrap(),
            )
            .unwrap();
            let mut changed = current.clone();
            changed.semantics["native_endpoint"] = json!("different endpoint");
            *adapter.definition_override.lock().unwrap() = Some(changed);
            assert_eq!(
                validate_exact_plan_identity(&*adapter, &current.name, exact)
                    .unwrap_err()
                    .code,
                "PLAN_OPERATION_CONTRACT_CHANGED"
            );
            let next = engine
                .prepare_plan_permissions(&spec, Some(&prior), &[])
                .await
                .unwrap();
            assert_eq!(
                validate_revision_transition(
                    Some(&prior),
                    &spec,
                    &next,
                    &prior.usage,
                    &Store::in_memory().unwrap()
                )
                .unwrap_err()
                .code,
                "PLAN_REVISION_SCOPE_CHANGED"
            );
            // Completed archival steps retain their prior permission, while an
            // uncompleted step may not execute against the changed endpoint.
            let completed = engine
                .prepare_plan_permissions(&spec, Some(&prior), &["read-account".into()])
                .await
                .unwrap();
            assert_eq!(completed, prior.revision.permissions);
            *adapter.definition_override.lock().unwrap() = Some(current.clone());
            adapter
                .changed_authorization_identity
                .store(true, Ordering::SeqCst);
            assert_eq!(
                validate_exact_plan_identity(&*adapter, &current.name, exact)
                    .unwrap_err()
                    .code,
                "PLAN_OPERATION_CONTRACT_CHANGED"
            );
            adapter
                .changed_authorization_identity
                .store(false, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn pre_gofin_exact_authority_survives_only_audited_branding_changes() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let mut legacy = GenericReadAdapter::definition();
        legacy.name = "bridge.quotes".into();
        legacy.input_schema["$defs"] = json!({"BridgeBounds":{
            "type":"object",
            "description":"All caps are explicit and asset-denominated. USD caps use integer microdollars;\na caller must supply independent Fin pricing evidence before policy can enforce them."
        }});
        legacy.input_schema["properties"]["account"]["enum"] = json!(["account-1", "Fin"]);
        legacy.input_schema["properties"]["account"]["maxLength"] = json!(80);
        legacy.semantics = json!({
            "command_line_example":"fin read bridge.quotes --input JSON --json",
            "provider_usd_fields":"ignored; fee USD values are independently priced by Fin",
            "authority":{"maximum":10},
        });
        // Reproduce the old exact-scope algorithm independently, so the test
        // cannot accidentally authorize a new, incompatible hash on both sides.
        let legacy_hash = binding_hash(&json!({
            "operation":legacy.name,
            "kind":legacy.kind,
            "execution_actor":legacy.execution_actor,
            "input_schema":legacy.input_schema,
            "semantics":legacy.semantics,
        }))
        .unwrap();
        *adapter.definition_override.lock().unwrap() = Some(legacy.clone());
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let mut request = generic_read_request("legacy-brand-auth");
        request.plan.steps[0].operation = legacy.name.clone();
        let review = engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap();
        let exact = review.permissions[0].exact.as_ref().unwrap().clone();
        assert_eq!(exact.definition_hash, legacy_hash);
        request.expected_review_hash = Some(review.review_hash);
        let authorized = engine.authorize_financial_plan(request).await.unwrap();

        let mut renamed = legacy.clone();
        renamed.input_schema["$defs"]["BridgeBounds"]["description"] = json!(
            "All caps are explicit and asset-denominated. USD caps use integer microdollars;\na caller must supply independent GoFin pricing evidence before policy can enforce them."
        );
        renamed.semantics["command_line_example"] =
            json!("gofin read bridge.quotes --input JSON --json");
        renamed.semantics["provider_usd_fields"] =
            json!("ignored; fee USD values are independently priced by GoFin");
        *adapter.definition_override.lock().unwrap() = Some(renamed.clone());
        validate_exact_plan_identity(&*adapter, &renamed.name, &exact).unwrap();

        // Real schema/actor/authority changes still require a fresh approval,
        // including material values containing either spelling of the brand.
        for (pointer, value) in [
            ("/input_schema/properties/account/enum/1", json!("GoFin")),
            ("/input_schema/properties/account/maxLength", json!(81)),
            ("/kind", json!("write")),
            ("/execution_actor", json!("human_interactive")),
            ("/semantics/authority/maximum", json!(11)),
            (
                "/semantics/provider_usd_fields",
                json!(
                    "ignored; fee USD values are independently priced by GoFin; maximum fee is 100"
                ),
            ),
        ] {
            let mut changed = serde_json::to_value(&renamed).unwrap();
            *changed.pointer_mut(pointer).unwrap() = value;
            *adapter.definition_override.lock().unwrap() =
                Some(serde_json::from_value(changed).unwrap());
            let error = validate_exact_plan_identity(&*adapter, &renamed.name, &exact).unwrap_err();
            assert_eq!(error.code, "PLAN_OPERATION_CONTRACT_CHANGED", "{pointer}");
        }
        *adapter.definition_override.lock().unwrap() = Some(renamed.clone());
        engine
            .preview_financial_plan_step(
                &authorized.plan_id,
                "read-account",
                &renamed.name,
                &json!({"account":"account-1"}),
            )
            .await
            .unwrap();
        let result = engine
            .execute_financial_plan_step(
                &authorized.plan_id,
                "read-account",
                OperationRequest {
                    request_id: "legacy-brand-attempt".into(),
                    operation: renamed.name,
                    input: json!({"account":"account-1"}),
                    run_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Completed);
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 2);
        assert_eq!(adapter.prepares.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn generic_registered_read_uses_schema_identity_and_persists_result_without_prepare() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("generic-read.sqlite");
        let engine = Engine::new(
            Store::open(&database).unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let mut request = generic_read_request("generic-read-auth");
        let review = engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap();
        assert!(review.permissions[0].exact.is_some());
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.prepares.load(Ordering::SeqCst), 0);
        request.expected_review_hash = Some(review.review_hash);
        let authorized = engine.authorize_financial_plan(request).await.unwrap();
        let plan_id = authorized.plan_id.clone();
        let result = engine
            .execute_financial_plan_step(
                &plan_id,
                "read-account",
                OperationRequest {
                    request_id: "generic-read-attempt".into(),
                    operation: "future.read".into(),
                    input: json!({"account":"account-1"}),
                    run_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Completed);
        assert_eq!(
            result.last_reconciliation_evidence,
            Some(json!({"read_result":{"value":7}}))
        );
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.prepares.load(Ordering::SeqCst), 0);
        let status = engine.financial_plan_status(&plan_id).await.unwrap();
        assert_eq!(
            status.steps[0].state,
            FinancialPlanStepProgressState::Completed
        );
        assert_eq!(status.steps[0].attempts[0].result, Some(json!({"value":7})));
        assert!(status.usage.verified_proceeds.is_empty());
        assert!(status.next_eligible_step_id.is_none());
        drop(engine);

        let restarted_adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let restarted = Engine::new(
            Store::open(&database).unwrap(),
            vec![restarted_adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let recovered = restarted.financial_plan_status(&plan_id).await.unwrap();
        assert_eq!(
            recovered.steps[0].attempts[0].result,
            Some(json!({"value":7}))
        );
        assert_eq!(restarted_adapter.reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn generic_exact_dispatch_rejects_changed_protected_connection_identity() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let request = generic_read_request("generic-read-metadata-auth");
        let authorized = engine.authorize_financial_plan(request).await.unwrap();
        adapter
            .changed_authorization_identity
            .store(true, Ordering::SeqCst);
        let error = engine
            .execute_financial_plan_step(
                &authorized.plan_id,
                "read-account",
                OperationRequest {
                    request_id: "generic-read-metadata-attempt".into(),
                    operation: "future.read".into(),
                    input: json!({"account":"account-1"}),
                    run_id: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "PLAN_OPERATION_CONTRACT_CHANGED");
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn hidden_authorization_identity_change_blocks_exact_step_before_read() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let authorized = engine
            .authorize_financial_plan(generic_read_request("hidden-identity-auth"))
            .await
            .unwrap();
        let public_connections = adapter.connections();
        let approved_authorization_identities = adapter.authorization_identities();
        adapter
            .changed_authorization_identity
            .store(true, Ordering::SeqCst);
        assert_eq!(adapter.connections(), public_connections);
        assert_ne!(
            adapter.authorization_identities(),
            approved_authorization_identities
        );

        let error = engine
            .execute_financial_plan_step(
                &authorized.plan_id,
                "read-account",
                OperationRequest {
                    request_id: "hidden-identity-attempt".into(),
                    operation: "future.read".into(),
                    input: json!({"account":"account-1"}),
                    run_id: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "PLAN_OPERATION_CONTRACT_CHANGED");
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.prepares.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn public_summary_change_does_not_invalidate_unchanged_authorization_identity() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        });
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let authorized = engine
            .authorize_financial_plan(generic_read_request("public-summary-auth"))
            .await
            .unwrap();
        let original_public = adapter.connections();
        let approved_identities = adapter.authorization_identities();
        adapter.changed_connection.store(true, Ordering::SeqCst);
        assert_ne!(adapter.connections(), original_public);
        assert_eq!(adapter.authorization_identities(), approved_identities);

        let result = engine
            .execute_financial_plan_step(
                &authorized.plan_id,
                "read-account",
                OperationRequest {
                    request_id: "public-summary-attempt".into(),
                    operation: "future.read".into(),
                    input: json!({"account":"account-1"}),
                    run_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::Completed);
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.prepares.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn exact_connection_hash_binds_stable_details_and_ignores_connection_order() {
        let adapter = GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        };
        let original = adapter.connections();
        let hash = stable_connection_hash(&original).unwrap();
        let mut reordered = original.clone();
        reordered.reverse();
        assert_eq!(stable_connection_hash(&reordered).unwrap(), hash);

        let mut remapped = original;
        remapped[0].details["withdrawal_destinations"] = json!({"wallet":"0x111"});
        assert_ne!(stable_connection_hash(&remapped).unwrap(), hash);
        remapped[0].details["withdrawal_destinations"]["wallet"] = json!("0x222");
        assert_ne!(stable_connection_hash(&remapped).unwrap(), hash);
    }

    #[test]
    fn plan_actor_policy_rejects_human_interactive_writes_before_permission_modes() {
        assert!(plan_step_actor_allowed(
            OperationKind::Read,
            &gofin_domain::ExecutionActor::AgentReadable
        ));
        assert!(plan_step_actor_allowed(
            OperationKind::Write,
            &gofin_domain::ExecutionActor::AgentInvocable
        ));
        assert!(!plan_step_actor_allowed(
            OperationKind::Write,
            &gofin_domain::ExecutionActor::HumanInteractive
        ));
        assert!(!plan_step_actor_allowed(
            OperationKind::Read,
            &gofin_domain::ExecutionActor::AgentInvocable
        ));
    }

    #[tokio::test]
    async fn generic_read_failure_is_durable_and_releases_the_unresolved_slot() {
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(true),
            read_gate: None,
        });
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![adapter.clone()],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let authorized = engine
            .authorize_financial_plan(generic_read_request("generic-read-failure-auth"))
            .await
            .unwrap();
        let failed = engine
            .execute_financial_plan_step(
                &authorized.plan_id,
                "read-account",
                OperationRequest {
                    request_id: "generic-read-failure-attempt".into(),
                    operation: "future.read".into(),
                    input: json!({"account":"account-1"}),
                    run_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(failed.status, Status::Failed);
        let status = engine
            .financial_plan_status(&authorized.plan_id)
            .await
            .unwrap();
        assert_eq!(
            status.steps[0].state,
            FinancialPlanStepProgressState::InProgress
        );
        assert_eq!(status.steps[0].attempts[0].status, Status::Failed);
        assert_eq!(
            status.next_eligible_step_id.as_deref(),
            Some("read-account")
        );
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn revoking_during_a_read_preserves_revocation_and_durable_result() {
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let adapter = Arc::new(GenericReadAdapter {
            definition_override: Mutex::new(None),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: Some((started.clone(), release.clone())),
        });
        let engine = Arc::new(
            Engine::new(
                Store::in_memory().unwrap(),
                vec![adapter.clone()],
                Arc::new(PlanTestAuthorizer),
                EngineOptions::default(),
            )
            .unwrap(),
        );
        let authorized = engine
            .authorize_financial_plan(generic_read_request("generic-read-revoke-auth"))
            .await
            .unwrap();
        let execution_engine = engine.clone();
        let plan_id = authorized.plan_id.clone();
        let execution = tokio::spawn(async move {
            execution_engine
                .execute_financial_plan_step(
                    &plan_id,
                    "read-account",
                    OperationRequest {
                        request_id: "generic-read-revoke-attempt".into(),
                        operation: "future.read".into(),
                        input: json!({"account":"account-1"}),
                        run_id: None,
                    },
                )
                .await
        });
        started.acquire().await.unwrap().forget();
        let revoked = engine
            .revoke_financial_plan(&authorized.plan_id)
            .await
            .unwrap();
        assert_eq!(
            revoked.revision.status,
            FinancialPlanAuthorizationStatus::Revoked
        );
        release.add_permits(1);
        let result = execution.await.unwrap().unwrap();
        assert_eq!(result.status, Status::Completed);
        let status = engine
            .financial_plan_status(&authorized.plan_id)
            .await
            .unwrap();
        assert_eq!(
            status.revision.status,
            FinancialPlanAuthorizationStatus::Revoked
        );
        assert_eq!(status.steps[0].attempts[0].result, Some(json!({"value":7})));
        assert!(status.next_eligible_step_id.is_none());
        assert_eq!(adapter.reads.load(Ordering::SeqCst), 1);
    }

    fn resource_amount(resource: &str, token: &str, units: &str) -> ResourceAmount {
        ResourceAmount {
            resource: ResourceId(resource.into()),
            amount: Amount {
                asset: asset(token),
                atoms: Quantity::new(units).unwrap(),
            },
        }
    }

    #[test]
    fn exact_scope_without_fees_or_with_unique_zero_fee_identity_is_valid() {
        let debit = resource_amount("hl:spot:primary", "USDC", "11000000");
        let mut scope = FinancialPlanExactScope {
            mode: financial_plans::FinancialPlanExactMode::Exact,
            maximum_debits: vec![debit],
            maximum_costs: vec![],
            possible_credits: vec![],
        };
        validate_exact_scope(&scope).unwrap();

        scope.maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("hl:spot:primary".into()),
            },
            amount: Amount {
                asset: asset("USDC"),
                atoms: Quantity::zero(),
            },
            treatment: CostTreatment::AdditionalDebit,
        });
        validate_exact_scope(&scope).unwrap();
        scope.maximum_costs.push(scope.maximum_costs[0].clone());
        assert_eq!(
            validate_exact_scope(&scope).unwrap_err().code,
            "PLAN_EXACT_SCOPE_INVALID"
        );
    }

    fn conversion_and_fee() -> Vec<FinancialEffect> {
        // Fee intentionally precedes the gross receipt to ensure accounting
        // does not depend on adapter effect ordering.
        vec![
            FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: ResourceId("acct".into()),
                },
                amount: Amount {
                    asset: asset("ETH"),
                    atoms: Quantity::new("1").unwrap(),
                },
                category: "commission".into(),
                treatment: CostTreatment::DeductedFromCredit,
            },
            FinancialEffect::Conversion {
                resource: ResourceId("acct".into()),
                spent: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("20").unwrap(),
                },
                received: Amount {
                    asset: asset("ETH"),
                    atoms: Quantity::new("100").unwrap(),
                },
            },
        ]
    }

    #[test]
    fn completion_requires_every_declared_target_and_deducts_output_fees_independent_of_order() {
        let effects = conversion_and_fee();
        let completion = financial_plans::FinancialStepCompletion {
            principal_spend: Some(resource_amount("acct", "USDT", "20")),
            minimum_net_receipt: Some(resource_amount("acct", "ETH", "99")),
            operation_succeeded: false,
        };
        assert!(completion_met(&completion, &effects, false).unwrap());

        let insufficient_receipt = financial_plans::FinancialStepCompletion {
            minimum_net_receipt: Some(resource_amount("acct", "ETH", "100")),
            ..completion.clone()
        };
        assert!(!completion_met(&insufficient_receipt, &effects, false).unwrap());

        let insufficient_spend = financial_plans::FinancialStepCompletion {
            principal_spend: Some(resource_amount("acct", "USDT", "21")),
            ..completion
        };
        assert!(!completion_met(&insufficient_spend, &effects, false).unwrap());

        let already_spent_but_short_receipt = financial_plans::FinancialPlanStepSpec {
            action_deadline_seconds: None,
            step_id: "step".into(),
            operation: "synthetic.write".into(),
            input: json!({}),
            permitted_variations: json!({}),
            completion: financial_plans::FinancialStepCompletion {
                principal_spend: Some(resource_amount("acct", "USDT", "20")),
                minimum_net_receipt: Some(resource_amount("acct", "ETH", "101")),
                operation_succeeded: false,
            },
            funding_dependencies: vec![],
        };
        let progress =
            summarize_plan_effects(&effects, "step", &FinancialPlanUsage::default()).unwrap();
        let step_permission = permission(json!({"account": "a"}), json!({}), "30");
        let aggregate_remaining = vec![resource_amount("acct", "USDT", "10")];
        assert!(
            step_authority_cannot_complete(
                &already_spent_but_short_receipt,
                &step_permission,
                &progress.0,
                &aggregate_remaining,
                &[],
                &effects,
                false
            )
            .unwrap()
        );
    }

    #[test]
    fn escrow_progress_supplies_only_proved_output_and_refund_for_plan_funding() {
        let wallet = ResourceId("solana:mainnet-beta:wallet".into());
        let escrow = ResourceId("solana:mainnet-beta:trigger-order:order".into());
        let input = asset("INPUT");
        let output = asset("OUTPUT");
        let partial = FinancialEffect::EscrowProgress {
            wallet_resource: wallet.clone(),
            escrow_resource: escrow,
            input_asset: input.clone(),
            output_asset: output.clone(),
            wallet_debit: Quantity::new("100").unwrap(),
            escrow_deposit: Quantity::new("100").unwrap(),
            input_transfer_fee: Quantity::new("0").unwrap(),
            input_filled: Quantity::new("40").unwrap(),
            output_filled: Quantity::new("80").unwrap(),
            input_refunded: Quantity::new("0").unwrap(),
            remaining_input: Quantity::new("60").unwrap(),
            state: gofin_domain::EscrowLifecycleState::Open,
        };
        let completion = financial_plans::FinancialStepCompletion {
            principal_spend: Some(resource_amount(
                "solana:mainnet-beta:wallet",
                "INPUT",
                "100",
            )),
            minimum_net_receipt: Some(resource_amount(
                "solana:mainnet-beta:wallet",
                "OUTPUT",
                "80",
            )),
            operation_succeeded: false,
        };
        assert!(completion_met(&completion, std::slice::from_ref(&partial), false).unwrap());
        let mut plan = prior_record();
        plan.usage = FinancialPlanUsage::default();
        plan.revision.spec.aggregate_bounds.maximum_debits = vec![resource_amount(
            "solana:mainnet-beta:wallet",
            "INPUT",
            "100",
        )];
        let scope = plan.revision.permissions[0].permission.as_mut().unwrap();
        scope.spec.maximum_debits = vec![resource_amount(
            "solana:mainnet-beta:wallet",
            "INPUT",
            "100",
        )];
        scope.possible_credits = vec![
            FinancialPlanFundingIdentity {
                resource: wallet.clone(),
                asset: output.clone(),
            },
            FinancialPlanFundingIdentity {
                resource: wallet.clone(),
                asset: input.clone(),
            },
        ];
        let mut cancelled = partial;
        if let FinancialEffect::EscrowProgress {
            input_refunded,
            remaining_input,
            state,
            ..
        } = &mut cancelled
        {
            *input_refunded = Quantity::new("60").unwrap();
            *remaining_input = Quantity::new("0").unwrap();
            *state = gofin_domain::EscrowLifecycleState::Cancelled;
        }
        let operation = settled_effect_record(vec![cancelled]);
        account_financial_plan_effects(&mut plan, &operation).unwrap();
        assert_eq!(plan.usage.consumed_debits[0].amount.atoms.as_str(), "100");
        assert_eq!(plan.usage.verified_proceeds.len(), 2);
        assert!(
            plan.usage
                .verified_proceeds
                .iter()
                .any(|row| row.asset == input && row.available.as_str() == "60")
        );
    }

    #[test]
    fn deferred_receipt_capacity_grants_no_principal_and_preserves_exact_target() {
        let mut approved = permission(json!({"claim":"fixed"}), json!({}), "1");
        let scope = approved.permission.as_mut().unwrap();
        scope.spec.maximum_debits.clear();
        scope.deferred_receipt = Some(resource_amount("destination", "ETH", "100"));
        let mut step = financial_plans::FinancialPlanStepSpec {
            action_deadline_seconds: None,
            step_id: "claim".into(),
            operation: "synthetic.claim".into(),
            input: json!({}),
            permitted_variations: json!({}),
            completion: financial_plans::FinancialStepCompletion {
                principal_spend: None,
                minimum_net_receipt: Some(resource_amount("destination", "ETH", "100")),
                operation_succeeded: true,
            },
            funding_dependencies: vec![],
        };
        assert!(
            !step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false).unwrap()
        );
        assert!(permission_debits(&approved).is_empty());
        for target in [
            resource_amount("other", "ETH", "100"),
            resource_amount("destination", "USDT", "100"),
            resource_amount("destination", "ETH", "101"),
        ] {
            step.completion.minimum_net_receipt = Some(target);
            assert!(
                step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false)
                    .unwrap()
            );
        }
        step.completion.minimum_net_receipt = Some(resource_amount("destination", "ETH", "100"));
        approved.permission.as_mut().unwrap().deferred_receipt = None;
        assert!(
            step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false).unwrap()
        );
    }

    #[test]
    fn finite_adapter_position_source_preserves_identity_and_single_attempt_limit() {
        let mut approved = permission(json!({"withdraw":"fixed"}), json!({}), "1");
        let adapter = approved.permission.as_mut().unwrap();
        adapter.spec.maximum_debits.clear();
        adapter.possible_credits = vec![FinancialPlanFundingIdentity {
            resource: ResourceId("wallet:one".into()),
            asset: asset("WETH"),
        }];
        let mut step = financial_plans::FinancialPlanStepSpec {
            step_id: "withdraw".into(),
            operation: "synthetic.withdraw".into(),
            input: json!({}),
            permitted_variations: json!({}),
            completion: financial_plans::FinancialStepCompletion {
                principal_spend: None,
                minimum_net_receipt: Some(resource_amount("wallet:one", "WETH", "300")),
                operation_succeeded: true,
            },
            funding_dependencies: vec![],
            action_deadline_seconds: None,
        };
        assert!(
            !step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], true).unwrap()
        );
        // Possible outputs alone still grant no source authority.
        assert!(
            step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false).unwrap()
        );
        for target in [
            resource_amount("wallet:other", "WETH", "300"),
            resource_amount("wallet:one", "USDC", "300"),
        ] {
            step.completion.minimum_net_receipt = Some(target);
            assert!(
                step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], true).unwrap()
            );
        }
        step.completion.minimum_net_receipt = Some(resource_amount("wallet:one", "WETH", "300"));
        let effects = vec![FinancialEffect::UsableCredit {
            to: ResourceId("wallet:one".into()),
            amount: Amount {
                asset: asset("WETH"),
                atoms: Quantity::new("200").unwrap(),
            },
        }];
        assert!(
            step_authority_cannot_complete(&step, &approved, &[], &[], &[], &effects, true)
                .unwrap()
        );
        // A rent debit allowance must not restore a consumed position source.
        approved.permission.as_mut().unwrap().spec.maximum_debits =
            vec![resource_amount("wallet:one", "SOL", "10")];
        assert!(
            step_authority_cannot_complete(
                &step,
                &approved,
                &[],
                &[resource_amount("wallet:one", "SOL", "10")],
                &[],
                &effects,
                true
            )
            .unwrap()
        );
    }

    #[test]
    fn obligation_bounded_credit_can_satisfy_receipt_completion_only_with_matching_scope() {
        let mut approved = permission(json!({"borrow":"fixed"}), json!({}), "1");
        {
            let adapter = approved.permission.as_mut().unwrap();
            adapter.spec.maximum_debits.clear();
            adapter.possible_credits = vec![FinancialPlanFundingIdentity {
                resource: ResourceId("wallet:one".into()),
                asset: asset("WETH"),
            }];
            adapter.cumulative_scope = Some(financial_plans::FinancialPlanCumulativeScope {
                maximum_obligations: vec![resource_amount("wallet:one", "WETH", "400")],
                scope_id: "synthetic-borrow-scope".into(),
                maximum_debits: vec![],
                maximum_costs: vec![],
            });
        }
        let step = financial_plans::FinancialPlanStepSpec {
            step_id: "borrow".into(),
            operation: "synthetic.borrow".into(),
            input: json!({}),
            permitted_variations: json!({}),
            completion: financial_plans::FinancialStepCompletion {
                principal_spend: None,
                minimum_net_receipt: Some(resource_amount("wallet:one", "WETH", "300")),
                operation_succeeded: true,
            },
            funding_dependencies: vec![],
            action_deadline_seconds: None,
        };

        assert!(
            !step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false).unwrap()
        );
        assert!(
            step_authority_cannot_complete(
                &step,
                &approved,
                &[],
                &[],
                &[resource_amount("wallet:one", "WETH", "200")],
                &[],
                false
            )
            .unwrap()
        );
        assert!(
            step_authority_cannot_complete(
                &step,
                &approved,
                &[],
                &[],
                &[resource_amount("wallet:one", "WETH", "400")],
                &[],
                false
            )
            .unwrap()
        );

        approved.permission.as_mut().unwrap().possible_credits[0].resource =
            ResourceId("wallet:other".into());
        assert!(
            step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false).unwrap()
        );
        approved.permission.as_mut().unwrap().possible_credits[0] = FinancialPlanFundingIdentity {
            resource: ResourceId("wallet:one".into()),
            asset: asset("USDC"),
        };
        assert!(
            step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false).unwrap()
        );
        approved
            .permission
            .as_mut()
            .unwrap()
            .possible_credits
            .clear();
        assert!(
            step_authority_cannot_complete(&step, &approved, &[], &[], &[], &[], false).unwrap()
        );
    }

    #[test]
    fn plan_status_keeps_borrow_receipt_step_eligible_when_obligation_cap_covers_it() {
        let mut record = prior_record();
        let step = financial_plans::FinancialPlanStepSpec {
            step_id: "borrow".into(),
            operation: "synthetic.borrow".into(),
            input: json!({"wallet":"synthetic-wallet"}),
            permitted_variations: json!({}),
            completion: financial_plans::FinancialStepCompletion {
                principal_spend: None,
                minimum_net_receipt: Some(resource_amount("wallet:one", "WETH", "300")),
                operation_succeeded: true,
            },
            funding_dependencies: vec![],
            action_deadline_seconds: None,
        };
        let mut approved = permission(json!({"borrow":"fixed"}), json!({}), "1");
        approved.step_id = "borrow".into();
        {
            let adapter = approved.permission.as_mut().unwrap();
            adapter.spec.maximum_debits.clear();
            adapter.possible_credits = vec![FinancialPlanFundingIdentity {
                resource: ResourceId("wallet:one".into()),
                asset: asset("WETH"),
            }];
            adapter.cumulative_scope = Some(financial_plans::FinancialPlanCumulativeScope {
                maximum_obligations: vec![resource_amount("wallet:one", "WETH", "400")],
                scope_id: "synthetic-borrow-scope".into(),
                maximum_debits: vec![],
                maximum_costs: vec![],
            });
        }
        record.revision.spec.steps = vec![step];
        record.revision.spec.aggregate_bounds.maximum_debits.clear();
        record.revision.spec.aggregate_bounds.maximum_costs.clear();
        record.revision.permissions = vec![approved];
        record.revision.status = FinancialPlanAuthorizationStatus::Active;
        record.usage = FinancialPlanUsage::default();
        record.attempts.clear();
        record.active_attempt_id = None;

        // Status consults the registered adapter's local reservation semantics.
        // This fixture supplies no exclusive resources and performs no I/O.
        let mut definition = GenericReadAdapter::definition();
        definition.name = record.revision.spec.steps[0].operation.clone();
        definition.kind = OperationKind::Write;
        definition.execution_actor = gofin_domain::ExecutionActor::AgentInvocable;
        let adapter = GenericReadAdapter {
            definition_override: Mutex::new(Some(definition)),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        };
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![Arc::new(adapter)],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let status = engine.build_plan_status(&record, &record.revision).unwrap();
        assert_eq!(
            status.steps[0].state,
            FinancialPlanStepProgressState::NotStarted
        );
        assert_eq!(status.next_eligible_step_id.as_deref(), Some("borrow"));
        assert!(status.steps[0].blockers.is_empty());
    }

    #[test]
    fn plan_status_keeps_position_withdrawal_receipt_step_eligible() {
        let mut record = prior_record();
        let step = financial_plans::FinancialPlanStepSpec {
            step_id: "withdraw-position".into(),
            operation: "aave.withdraw".into(),
            input: json!({"amount":"all"}),
            permitted_variations: json!({}),
            completion: financial_plans::FinancialStepCompletion {
                principal_spend: None,
                minimum_net_receipt: Some(resource_amount("wallet:one", "WETH", "300")),
                operation_succeeded: true,
            },
            funding_dependencies: vec![],
            action_deadline_seconds: None,
        };
        let mut approved = permission(json!({"withdraw":"fixed"}), json!({}), "1");
        approved.step_id = "withdraw-position".into();
        {
            let adapter = approved.permission.as_mut().unwrap();
            adapter.spec.maximum_debits.clear();
            adapter.spec.maximum_costs.push(CostBound {
                location: AccountingLocation::Position {
                    resource: ResourceId("wallet:one".into()),
                    position: "aave:pool:weth:supply".into(),
                    position_kind: gofin_domain::PositionKind::Supply,
                },
                amount: Amount {
                    asset: asset("WETH"),
                    atoms: Quantity::new("400").unwrap(),
                },
                treatment: CostTreatment::IncludedInDebit,
            });
            adapter.possible_credits = vec![FinancialPlanFundingIdentity {
                resource: ResourceId("wallet:one".into()),
                asset: asset("WETH"),
            }];
            adapter.deferred_receipt = Some(resource_amount("wallet:one", "WETH", "400"));
        }
        record.revision.spec.steps = vec![step];
        record.revision.spec.aggregate_bounds.maximum_debits.clear();
        record.revision.spec.aggregate_bounds.maximum_costs.clear();
        record.revision.permissions = vec![approved];
        record.revision.status = FinancialPlanAuthorizationStatus::Active;
        record.usage = FinancialPlanUsage::default();
        record.attempts.clear();
        record.active_attempt_id = None;

        // Status consults the registered adapter's local reservation semantics.
        // This fixture supplies no exclusive resources and performs no I/O.
        let mut definition = GenericReadAdapter::definition();
        definition.name = record.revision.spec.steps[0].operation.clone();
        definition.kind = OperationKind::Write;
        definition.execution_actor = gofin_domain::ExecutionActor::AgentInvocable;
        let adapter = GenericReadAdapter {
            definition_override: Mutex::new(Some(definition)),
            reads: AtomicUsize::new(0),
            prepares: AtomicUsize::new(0),
            changed_connection: AtomicBool::new(false),
            changed_authorization_identity: AtomicBool::new(false),
            fail_read: AtomicBool::new(false),
            read_gate: None,
        };
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![Arc::new(adapter)],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let status = engine.build_plan_status(&record, &record.revision).unwrap();
        assert_eq!(
            status.steps[0].state,
            FinancialPlanStepProgressState::NotStarted
        );
        assert_eq!(
            status.next_eligible_step_id.as_deref(),
            Some("withdraw-position")
        );
        assert!(status.steps[0].blockers.is_empty());
    }

    #[test]
    fn cumulative_obligation_usage_counts_verified_and_uncertain_attempts() {
        for verified in [true, false] {
            let mut plan = prior_record();
            plan.revision.permissions[0]
                .permission
                .as_mut()
                .unwrap()
                .cumulative_scope = Some(financial_plans::FinancialPlanCumulativeScope {
                scope_id: "synthetic-borrow-scope".into(),
                maximum_debits: vec![],
                maximum_costs: vec![],
                maximum_obligations: vec![resource_amount("acct", "WETH", "10")],
            });

            let operation = if verified {
                settled_effect_record(vec![FinancialEffect::DebtChange {
                    resource: ResourceId("acct".into()),
                    debt: "synthetic-liability".into(),
                    asset: asset("WETH"),
                    delta: gofin_domain::SignedQuantity::new("6").unwrap(),
                    anchor: None,
                }])
            } else {
                let timestamp = now();
                let prepared = PreparedOperation {
                    operation: "synthetic.borrow".into(),
                    input: json!({}),
                    binding: json!({}),
                    summary: "synthetic borrow reservation".into(),
                    checks: vec![],
                    warnings: vec![],
                    observed_at: timestamp.clone(),
                    expires_at: timestamp.clone(),
                    valuation: None,
                    action_deadline: None,
                    intent: gofin_domain::FinancialIntent {
                        expected: vec![EffectConstraint::Debt {
                            resource: ResourceId("acct".into()),
                            debt: "synthetic-liability".into(),
                            asset: asset("WETH"),
                            direction: Direction::Increase,
                            maximum_change: Quantity::new("6").unwrap(),
                        }],
                        ..gofin_domain::FinancialIntent::default()
                    },
                    operation_version: 1,
                };
                OperationRecord {
                    id: "attempt-1".into(),
                    request_id: "request-1".into(),
                    fingerprint: "fingerprint".into(),
                    fingerprint_version: 1,
                    operation: "synthetic.borrow".into(),
                    input: json!({}),
                    status: Status::Unknown,
                    created_at: timestamp.clone(),
                    updated_at: timestamp,
                    preview: Some(prepared),
                    approval: None,
                    authorization_source: None,
                    submission: Some(gofin_domain::Submission {
                        kind: "synthetic".into(),
                        reference: "synthetic-submission".into(),
                        data: json!({}),
                    }),
                    dispatch_evidence: None,
                    effects: None,
                    last_reconciliation_evidence: None,
                    error: None,
                    run_id: None,
                    position_baseline: None,
                }
            };
            let mut store = Store::in_memory().unwrap();
            store
                .create_request(
                    &operation.id,
                    &operation.request_id,
                    &operation.fingerprint,
                    &operation.operation,
                    None,
                    if verified { "completed" } else { "unknown" },
                    &to_value(&operation).unwrap(),
                    &now(),
                )
                .unwrap();
            if !verified {
                plan.usage
                    .reservations
                    .push(financial_plans::FinancialPlanReservation {
                        attempt_id: "attempt-1".into(),
                        step_id: "step-1".into(),
                        debits: vec![],
                        costs: vec![],
                    });
            }

            let usage =
                cumulative_obligation_usage_by_scope(&plan, &plan.revision, &store).unwrap();
            assert_eq!(
                usage.get("synthetic-borrow-scope"),
                Some(&vec![resource_amount("acct", "WETH", "6")])
            );
        }
    }

    #[test]
    fn operation_success_is_conjunctive_and_supports_zero_debit_effects() {
        let success = financial_plans::FinancialStepCompletion {
            principal_spend: None,
            minimum_net_receipt: None,
            operation_succeeded: true,
        };
        assert!(!completion_met(&success, &[], false).unwrap());
        assert!(completion_met(&success, &[], true).unwrap());

        let conjunctive = financial_plans::FinancialStepCompletion {
            principal_spend: Some(resource_amount("acct", "USDT", "20")),
            minimum_net_receipt: None,
            operation_succeeded: true,
        };
        assert!(!completion_met(&conjunctive, &conversion_and_fee(), false).unwrap());
        assert!(completion_met(&conjunctive, &conversion_and_fee(), true).unwrap());
    }

    #[test]
    fn committed_external_receipt_completes_target_without_rewriting_source_proof() {
        let completion = financial_plans::FinancialStepCompletion {
            principal_spend: Some(resource_amount("source", "USDT", "10")),
            minimum_net_receipt: Some(resource_amount("destination", "ETH", "100")),
            operation_succeeded: true,
        };
        let source_proof = vec![FinancialEffect::CashDebit {
            resource: ResourceId("source".into()),
            amount: Amount {
                asset: asset("USDT"),
                atoms: Quantity::new("10").unwrap(),
            },
        }];
        assert!(!completion_met(&completion, &source_proof, true).unwrap());
        let immutable_source_proof = source_proof.clone();
        let usage = FinancialPlanUsage {
            verified_proceeds: vec![FinancialPlanProceeds {
                producing_step_id: "bridge-deposit".into(),
                resource: ResourceId("destination".into()),
                asset: asset("ETH"),
                available: Quantity::new("100").unwrap(),
                consumed: Quantity::new("97").unwrap(),
            }],
            ..FinancialPlanUsage::default()
        };
        let mut status_effects = source_proof.clone();
        include_committed_plan_receipt(&completion, &mut status_effects, &usage, "bridge-deposit")
            .unwrap();
        assert!(completion_met(&completion, &status_effects, true).unwrap());
        assert_eq!(source_proof, immutable_source_proof);
        assert_eq!(
            status_effects.last(),
            Some(&FinancialEffect::UsableCredit {
                to: ResourceId("destination".into()),
                amount: Amount {
                    asset: asset("ETH"),
                    atoms: Quantity::new("100").unwrap(),
                },
            })
        );

        let short_usage = FinancialPlanUsage {
            verified_proceeds: vec![FinancialPlanProceeds {
                producing_step_id: "bridge-deposit".into(),
                resource: ResourceId("destination".into()),
                asset: asset("ETH"),
                available: Quantity::new("99").unwrap(),
                consumed: Quantity::new("96").unwrap(),
            }],
            ..FinancialPlanUsage::default()
        };
        let mut short_effects = source_proof.clone();
        include_committed_plan_receipt(
            &completion,
            &mut short_effects,
            &short_usage,
            "bridge-deposit",
        )
        .unwrap();
        assert!(!completion_met(&completion, &short_effects, true).unwrap());

        let unrelated_usage = FinancialPlanUsage {
            verified_proceeds: vec![FinancialPlanProceeds {
                producing_step_id: "different-step".into(),
                resource: ResourceId("destination".into()),
                asset: asset("ETH"),
                available: Quantity::new("100").unwrap(),
                consumed: Quantity::zero(),
            }],
            ..FinancialPlanUsage::default()
        };
        let mut unrelated_effects = source_proof;
        include_committed_plan_receipt(
            &completion,
            &mut unrelated_effects,
            &unrelated_usage,
            "bridge-deposit",
        )
        .unwrap();
        assert!(!completion_met(&completion, &unrelated_effects, true).unwrap());
    }

    #[test]
    fn only_completed_plan_linked_destination_proof_unblocks_receipt_projection() {
        let mut transfer = gofin_domain::BridgeTransferRecord {
            version: 1,
            id: "transfer-1".into(),
            grant_id: None,
            plan_scope: Some(gofin_domain::BridgePlanScope {
                plan_id: "plan-1".into(),
                revision_id: "revision-1".into(),
            }),
            metadata: Value::Null,
            settlement_expectation: None,
            approved_bounds: None,
            prospective_debit: None,
            state: gofin_domain::BridgeTransferState::InTransit,
            quote_revisions: Vec::new(),
            steps: vec![gofin_domain::BridgeStepLink {
                transfer_id: "transfer-1".into(),
                grant_id: None,
                plan_authority: Some(gofin_domain::BridgePlanAuthority {
                    plan_id: "plan-1".into(),
                    revision_id: "revision-1".into(),
                    plan_step_id: "bridge-deposit".into(),
                }),
                step_id: "provider-deposit".into(),
                operation_id: Some("operation-1".into()),
                kind: gofin_domain::BridgeStepKind::Deposit,
                depends_on: Vec::new(),
                state: gofin_domain::BridgeStepState::Settled,
            }],
            allocation_owner: gofin_domain::BridgeAllocationOwner::Transfer {
                transfer_id: "transfer-1".into(),
            },
            transit: None,
            applied_proofs: Vec::new(),
            settlement_proofs: Vec::new(),
        };
        let completed_attempt = vec![financial_plans::FinancialPlanAttemptSummary {
            attempt_id: "operation-1".into(),
            request_id: "request-1".into(),
            logical_request_id: None,
            step_id: "bridge-deposit".into(),
            status: Status::Completed,
            authorization_source: None,
            yolo_provenance: None,
            submission: None,
            error: None,
            result: None,
        }];

        assert!(!committed_plan_bridge_destination_proof(
            &transfer,
            "plan-1",
            "bridge-deposit",
            &completed_attempt
        ));

        transfer.state = gofin_domain::BridgeTransferState::Completed;
        transfer
            .settlement_proofs
            .push(gofin_domain::BridgeSettlementProof {
                transfer_id: "transfer-1".into(),
                outcome: gofin_domain::BridgeSettlementOutcome::Refund,
                evidence_id: "refund-proof".into(),
                effects: Vec::new(),
                evidence: Value::Null,
                observed_at: "2026-01-01T00:00:00Z".into(),
                proof_digest: "proof-digest".into(),
            });
        assert!(!committed_plan_bridge_destination_proof(
            &transfer,
            "plan-1",
            "bridge-deposit",
            &completed_attempt
        ));

        transfer.state = gofin_domain::BridgeTransferState::Failed;
        transfer.settlement_proofs[0].outcome = gofin_domain::BridgeSettlementOutcome::Destination;
        assert!(!committed_plan_bridge_destination_proof(
            &transfer,
            "plan-1",
            "bridge-deposit",
            &completed_attempt
        ));

        transfer.state = gofin_domain::BridgeTransferState::Completed;
        assert!(committed_plan_bridge_destination_proof(
            &transfer,
            "plan-1",
            "bridge-deposit",
            &completed_attempt
        ));
        assert!(!committed_plan_bridge_destination_proof(
            &transfer,
            "another-plan",
            "bridge-deposit",
            &completed_attempt
        ));
    }

    #[test]
    fn exact_plan_schema_validation_is_complete_and_offline() {
        let schema = json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{"account":{"type":"string"},"amount":{"type":"integer","minimum":1}},
            "required":["account","amount"],
            "additionalProperties":false
        });
        assert!(
            validate_plan_json_schema(&schema, &json!({"account":"synthetic", "amount":2})).is_ok()
        );
        assert!(validate_plan_json_schema(&schema, &json!({"account":"synthetic"})).is_err());
        assert!(
            validate_plan_json_schema(
                &json!({"$ref":"https://example.invalid/schema.json"}),
                &json!({})
            )
            .is_err()
        );
    }

    #[test]
    fn origin_movement_is_provenance_not_a_second_cash_debit() {
        let effects = vec![
            FinancialEffect::CashDebit {
                resource: ResourceId("acct".into()),
                amount: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("20").unwrap(),
                },
            },
            FinancialEffect::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: ResourceId("acct".into()),
                },
                to: AccountingLocation::Position {
                    resource: ResourceId("market".into()),
                    position: "supply".into(),
                    position_kind: gofin_domain::PositionKind::Supply,
                },
                debit: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("20").unwrap(),
                },
                credit: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("20").unwrap(),
                },
            },
        ];
        let usage =
            summarize_plan_effects(&effects, "supply", &FinancialPlanUsage::default()).unwrap();
        assert_eq!(usage.0, vec![resource_amount("acct", "USDT", "20")]);
    }

    fn settled_effect_record(effects: Vec<FinancialEffect>) -> OperationRecord {
        let submission = gofin_domain::Submission {
            kind: "synthetic".into(),
            reference: "synthetic-submission".into(),
            data: json!({}),
        };
        OperationRecord {
            id: "attempt-1".into(),
            request_id: "request-1".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: "synthetic.write".into(),
            input: json!({}),
            status: Status::Completed,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:01Z".into(),
            preview: None,
            approval: None,
            authorization_source: None,
            submission: Some(submission.clone()),
            dispatch_evidence: None,
            position_baseline: None,
            effects: Some(gofin_domain::VerifiedEffects {
                evidence_version: 1,
                submission,
                effects,
                evidence: json!({"synthetic":true}),
                observed_at: "2026-01-01T00:00:01Z".into(),
                proof_digest: "synthetic-proof".into(),
            }),
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    #[test]
    fn zero_cost_settlement_preserves_principal_without_positive_fee_usage() {
        let mut plan = prior_record();
        plan.usage = FinancialPlanUsage::default();
        plan.revision.permissions[0]
            .permission
            .as_mut()
            .unwrap()
            .possible_credits
            .push(FinancialPlanFundingIdentity {
                resource: ResourceId("spot".into()),
                asset: asset("USDT"),
            });
        let amount = Amount {
            asset: asset("USDT"),
            atoms: Quantity::new("5").unwrap(),
        };
        let operation = settled_effect_record(vec![
            FinancialEffect::Transfer {
                from: ResourceId("acct".into()),
                to: ResourceId("spot".into()),
                debit: amount.clone(),
                credit: amount,
            },
            FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: ResourceId("acct".into()),
                },
                amount: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::zero(),
                },
                category: "synthetic-zero-fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            },
        ]);
        account_financial_plan_effects(&mut plan, &operation).unwrap();
        assert_eq!(
            plan.usage.consumed_debits,
            vec![resource_amount("acct", "USDT", "5")]
        );
        assert!(
            plan.usage
                .consumed_costs
                .iter()
                .all(|cost| cost.amount.atoms.is_zero())
        );
        assert!(plan.usage.reservations.is_empty());
        assert_eq!(plan.usage.verified_proceeds.len(), 1);
        assert_eq!(plan.usage.verified_proceeds[0].resource.0, "spot");
        assert_eq!(plan.usage.verified_proceeds[0].available.as_str(), "5");
    }

    #[test]
    fn settlement_accounting_counts_cash_once_and_never_counts_provenance_only_as_cash() {
        let mut plan = prior_record();
        plan.usage = FinancialPlanUsage::default();
        plan.revision.spec.steps[0].funding_dependencies =
            vec![financial_plans::FinancialPlanFundingDependency {
                funding_scope: None,
                maximum_fraction_bps: None,
                producing_step_id: "producer".into(),
                resource: ResourceId("acct".into()),
                asset: asset("USDT"),
            }];
        let mut producer = step_spec();
        producer.step_id = "producer".into();
        producer.operation = "synthetic.producer".into();
        producer.funding_dependencies.clear();
        plan.revision.spec.steps.insert(0, producer);
        plan.usage.verified_proceeds = vec![financial_plans::FinancialPlanProceeds {
            producing_step_id: "producer".into(),
            resource: ResourceId("acct".into()),
            asset: asset("USDT"),
            available: Quantity::new("10").unwrap(),
            consumed: Quantity::zero(),
        }];

        let cash_and_provenance = settled_effect_record(vec![
            FinancialEffect::CashDebit {
                resource: ResourceId("acct".into()),
                amount: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("5").unwrap(),
                },
            },
            FinancialEffect::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: ResourceId("acct".into()),
                },
                to: AccountingLocation::Position {
                    resource: ResourceId("market".into()),
                    position: "supply".into(),
                    position_kind: gofin_domain::PositionKind::Supply,
                },
                debit: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("5").unwrap(),
                },
                credit: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("5").unwrap(),
                },
            },
        ]);
        account_financial_plan_effects(&mut plan, &cash_and_provenance).unwrap();
        assert_eq!(
            plan.usage.consumed_debits,
            vec![resource_amount("acct", "USDT", "5")]
        );
        assert_eq!(
            plan.usage.verified_proceeds[0].consumed,
            Quantity::new("5").unwrap()
        );

        let mut provenance_only_plan = prior_record();
        provenance_only_plan.usage = FinancialPlanUsage::default();
        let provenance_only = settled_effect_record(vec![FinancialEffect::OriginMovement {
            from: AccountingLocation::Position {
                resource: ResourceId("market".into()),
                position: "supply".into(),
                position_kind: gofin_domain::PositionKind::Supply,
            },
            to: AccountingLocation::Cash {
                resource: ResourceId("acct".into()),
            },
            debit: Amount {
                asset: asset("USDT"),
                atoms: Quantity::new("7").unwrap(),
            },
            credit: Amount {
                asset: asset("USDT"),
                atoms: Quantity::new("7").unwrap(),
            },
        }]);
        account_financial_plan_effects(&mut provenance_only_plan, &provenance_only).unwrap();
        assert!(provenance_only_plan.usage.consumed_debits.is_empty());
    }

    fn step_spec() -> financial_plans::FinancialPlanStepSpec {
        financial_plans::FinancialPlanStepSpec {
            action_deadline_seconds: None,
            step_id: "step-1".into(),
            operation: "synthetic.write".into(),
            input: json!({"request_id": "seed-a"}),
            permitted_variations: json!({"quantity": {"maximum": "50"}}),
            completion: financial_plans::FinancialStepCompletion {
                principal_spend: Some(resource_amount("acct", "USDT", "20")),
                minimum_net_receipt: None,
                operation_succeeded: false,
            },
            funding_dependencies: vec![],
        }
    }

    fn permission(fixed: Value, parameters: Value, debit: &str) -> FinancialPlanStepPermission {
        FinancialPlanStepPermission {
            step_id: "step-1".into(),
            generated_input_fields: vec![],
            exact: None,
            permission: Some(PlanPermission {
                recovery_parent: None,
                spec: BoundarySpec {
                    fixed,
                    parameters,
                    maximum_debits: vec![resource_amount("acct", "USDT", debit)],
                    maximum_costs: vec![],
                    summary: "synthetic fixed scope".into(),
                },
                possible_credits: vec![],
                fee_assets: vec![],
                deferred_receipt: None,
                authorization_seconds: None,
                cumulative_scope: None,
            }),
        }
    }

    #[test]
    fn incurred_debt_caps_ignore_repayment_and_retain_uncertain_reservations_after_restart() {
        for verified in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("SYNTHETIC-obligation.sqlite");
            let mut store = Store::open(&path).unwrap();
            let mut plan = prior_record();
            let asset = resource_amount("acct", "USDT", "6").amount.asset;
            plan.revision.permissions[0]
                .permission
                .as_mut()
                .unwrap()
                .cumulative_scope = Some(financial_plans::FinancialPlanCumulativeScope {
                scope_id: "SYNTHETIC-debt".into(),
                maximum_debits: vec![],
                maximum_costs: vec![],
                maximum_obligations: vec![resource_amount("acct", "USDT", "10")],
            });
            let candidate = |units: &str| PreparedOperation {
                operation: "synthetic.borrow".into(),
                input: json!({}),
                binding: json!({}),
                summary: "synthetic debt".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: now(),
                expires_at: now(),
                valuation: None,
                action_deadline: None,
                operation_version: 1,
                intent: gofin_domain::FinancialIntent {
                    expected: vec![EffectConstraint::Debt {
                        resource: ResourceId("acct".into()),
                        debt: "SYNTHETIC-liability".into(),
                        asset: asset.clone(),
                        direction: Direction::Increase,
                        maximum_change: Quantity::new(units).unwrap(),
                    }],
                    ..Default::default()
                },
            };
            let submission = Submission {
                kind: "synthetic".into(),
                reference: "SYNTHETIC-submission".into(),
                data: json!({}),
            };
            let effects = if verified {
                Some(gofin_domain::VerifiedEffects {
                    evidence_version: 1,
                    submission: submission.clone(),
                    effects: vec![
                        FinancialEffect::DebtChange {
                            resource: ResourceId("acct".into()),
                            debt: "SYNTHETIC-liability".into(),
                            asset: asset.clone(),
                            delta: gofin_domain::SignedQuantity::new("6").unwrap(),
                            anchor: None,
                        },
                        FinancialEffect::DebtChange {
                            resource: ResourceId("acct".into()),
                            debt: "SYNTHETIC-liability".into(),
                            asset: asset.clone(),
                            delta: gofin_domain::SignedQuantity::new("-6").unwrap(),
                            anchor: None,
                        },
                    ],
                    evidence: json!({"synthetic":"gross accounting test only"}),
                    observed_at: now(),
                    proof_digest: "SYNTHETIC".into(),
                })
            } else {
                None
            };
            let operation = OperationRecord {
                id: "attempt-1".into(),
                request_id: "SYNTHETIC-request".into(),
                fingerprint: "SYNTHETIC".into(),
                fingerprint_version: 1,
                operation: "synthetic.borrow".into(),
                input: json!({}),
                status: if verified {
                    Status::Completed
                } else {
                    Status::Unknown
                },
                created_at: now(),
                updated_at: now(),
                preview: Some(candidate("6")),
                approval: None,
                authorization_source: None,
                submission: Some(submission),
                dispatch_evidence: None,
                effects,
                last_reconciliation_evidence: None,
                error: None,
                position_baseline: None,
                run_id: None,
            };
            store
                .create_request(
                    &operation.id,
                    &operation.request_id,
                    &operation.fingerprint,
                    &operation.operation,
                    None,
                    if verified { "completed" } else { "unknown" },
                    &to_value(&operation).unwrap(),
                    &now(),
                )
                .unwrap();
            if !verified {
                plan.usage
                    .reservations
                    .push(financial_plans::FinancialPlanReservation {
                        attempt_id: "attempt-1".into(),
                        step_id: "step-1".into(),
                        debits: vec![],
                        costs: vec![],
                    });
            }
            drop(store);
            let reopened = Store::open(path).unwrap();
            validate_cumulative_scope_caps(&plan, "step-1", &candidate("4"), &reopened).unwrap();
            let error = validate_cumulative_scope_caps(&plan, "step-1", &candidate("5"), &reopened)
                .unwrap_err();
            assert_eq!(error.code, "PLAN_OBLIGATION_CAP_EXCEEDED");
            let details = error.details.unwrap();
            assert_eq!(details["step_id"], "step-1");
            assert_eq!(details["required_atoms"], "11");
            assert_eq!(details["maximum_atoms"], "10");
        }
        let legacy: financial_plans::FinancialPlanCumulativeScope = serde_json::from_value(
            json!({"scope_id":"legacy","maximum_debits":[],"maximum_costs":[]}),
        )
        .unwrap();
        assert!(legacy.maximum_obligations.is_empty());
    }

    #[test]
    fn cumulative_parent_scope_cannot_be_widened_by_a_plan_revision() {
        let mut prior = prior_record();
        let mut scoped = permission(json!({"route":"fixed"}), json!({}), "20");
        scoped.permission.as_mut().unwrap().cumulative_scope = Some(
            gofin_domain::financial_plans::FinancialPlanCumulativeScope {
                maximum_obligations: vec![],
                scope_id: "SYNTHETIC-bridge-parent".into(),
                maximum_debits: vec![resource_amount("acct", "USDT", "20")],
                maximum_costs: vec![],
            },
        );
        prior.revision.permissions = vec![scoped.clone()];
        let store = Store::in_memory().unwrap();

        validate_cumulative_revision_scopes(
            &prior,
            &prior.revision.spec,
            &[scoped.clone()],
            &store,
        )
        .unwrap();
        let mut widened = scoped;
        widened
            .permission
            .as_mut()
            .unwrap()
            .cumulative_scope
            .as_mut()
            .unwrap()
            .maximum_debits[0]
            .amount
            .atoms = Quantity::new("21").unwrap();
        assert_eq!(
            validate_cumulative_revision_scopes(&prior, &prior.revision.spec, &[widened], &store)
                .unwrap_err()
                .code,
            "PLAN_CUMULATIVE_SCOPE_CHANGED"
        );
    }

    fn bridge_intent_permission(fee: &str, refund_asset: &str) -> FinancialPlanStepPermission {
        let mut permission = permission(
            json!({"refund_asset": refund_asset, "route": "agent-nominated"}),
            json!({}),
            "20",
        );
        permission.step_id = "bridge-step".into();
        permission.permission.as_mut().unwrap().cumulative_scope = Some(
            gofin_domain::financial_plans::FinancialPlanCumulativeScope {
                maximum_obligations: vec![],
                scope_id: "bridge-step".into(),
                maximum_debits: vec![],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash {
                        resource: ResourceId("wallet:ethereum".into()),
                    },
                    amount: Amount {
                        asset: asset("USDC"),
                        atoms: Quantity::new(fee).unwrap(),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
            },
        );
        permission
    }

    fn make_unstarted_bridge_intent(prior: &mut FinancialPlanAuthorizationRecord) {
        prior.revision.spec.steps[0].operation = "bridge.execute".into();
        prior.revision.spec.steps[0].step_id = "bridge-step".into();
        prior.revision.permissions = vec![bridge_intent_permission("2", "USDC")];
        prior.attempts.clear();
        prior.active_attempt_id = None;
        prior.usage = FinancialPlanUsage::default();
    }

    #[test]
    fn unstarted_bridge_execute_intent_can_revise_fee_and_refund_scope_across_history() {
        let mut prior = prior_record();
        make_unstarted_bridge_intent(&mut prior);
        let first_revision = serde_json::to_value(&prior.revision).unwrap();
        let first_record = serde_json::to_value(&prior).unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .begin_financial_plan_request(
                "SYNTHETIC-bridge-revision-1",
                "SYNTHETIC-fingerprint-1",
                &prior.revision.plan_id,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        store
            .commit_financial_plan_revision(
                "SYNTHETIC-bridge-revision-1",
                "SYNTHETIC-fingerprint-1",
                &prior.revision.plan_id,
                &prior.revision.revision_id,
                None,
                &first_revision,
                &first_record,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();

        prior.revision.revision_id = "rev-2".into();
        prior.revision.permissions = vec![bridge_intent_permission("3", "USDC")];
        let proposed = bridge_intent_permission("4", "USDG");
        validate_cumulative_revision_scopes(&prior, &prior.revision.spec, &[proposed], &store)
            .unwrap();
    }

    #[test]
    fn started_bridge_execute_intent_cannot_change_cumulative_fee_or_refund_scope() {
        let mut prior = prior_record();
        make_unstarted_bridge_intent(&mut prior);
        prior.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: prior.revision.plan_id.clone(),
            revision_id: prior.revision.revision_id.clone(),
            step_id: "bridge-step".into(),
            attempt_id: "SYNTHETIC-bridge-attempt".into(),
            request_fingerprint: "fingerprint".into(),
            execution_binding_hash: "hash".into(),
            submission_id: None,
            result: None,
            logical_request_id: None,
        });
        let store = Store::in_memory().unwrap();
        assert_eq!(
            validate_cumulative_revision_scopes(
                &prior,
                &prior.revision.spec,
                &[bridge_intent_permission("4", "USDG")],
                &store,
            )
            .unwrap_err()
            .code,
            "PLAN_CUMULATIVE_SCOPE_CHANGED"
        );
    }

    #[test]
    fn started_latest_bridge_scope_survives_superseded_unused_history() {
        let mut prior = prior_record();
        make_unstarted_bridge_intent(&mut prior);
        let old_revision = serde_json::to_value(&prior.revision).unwrap();
        let old_record = serde_json::to_value(&prior).unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .begin_financial_plan_request(
                "SYNTHETIC-bridge-history-request",
                "SYNTHETIC-bridge-history-fingerprint",
                &prior.revision.plan_id,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        store
            .commit_financial_plan_revision(
                "SYNTHETIC-bridge-history-request",
                "SYNTHETIC-bridge-history-fingerprint",
                &prior.revision.plan_id,
                &prior.revision.revision_id,
                None,
                &old_revision,
                &old_record,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();

        prior.revision.revision_id = "rev-2".into();
        prior.revision.permissions = vec![bridge_intent_permission("3", "USDC")];
        prior.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: prior.revision.plan_id.clone(),
            revision_id: prior.revision.revision_id.clone(),
            step_id: "bridge-step".into(),
            attempt_id: "SYNTHETIC-bridge-started-attempt".into(),
            request_fingerprint: "fingerprint".into(),
            execution_binding_hash: "hash".into(),
            submission_id: Some("SYNTHETIC-submission".into()),
            result: None,
            logical_request_id: None,
        });

        validate_cumulative_revision_scopes(
            &prior,
            &prior.revision.spec,
            &[bridge_intent_permission("3", "USDC")],
            &store,
        )
        .expect("the submitted v2 intent must renew despite its unused v1 scope");
        assert_eq!(
            validate_cumulative_revision_scopes(
                &prior,
                &prior.revision.spec,
                &[bridge_intent_permission("4", "USDG")],
                &store,
            )
            .unwrap_err()
            .code,
            "PLAN_CUMULATIVE_SCOPE_CHANGED"
        );
    }

    #[test]
    fn adapter_plan_duration_ceiling_is_enforced_before_authorization() {
        let mut permitted = permission(json!({}), json!({}), "30");
        permitted.permission.as_mut().unwrap().authorization_seconds = Some(30);
        assert!(validate_plan_authorization_duration(30, &[permitted.clone()]).is_ok());
        assert_eq!(
            validate_plan_authorization_duration(31, &[permitted])
                .unwrap_err()
                .code,
            "PLAN_AUTHORIZATION_DURATION_EXCEEDED"
        );

        let mut invalid = permission(json!({}), json!({}), "30");
        invalid.permission.as_mut().unwrap().authorization_seconds = Some(0);
        assert_eq!(
            validate_plan_authorization_duration(1, &[invalid])
                .unwrap_err()
                .code,
            "PLAN_AUTHORIZATION_DURATION_EXCEEDED"
        );
    }

    #[test]
    fn plan_accounting_diagnostics_report_all_steps_caps_and_fee_treatments() {
        let mut spec = prior_record().revision.spec;
        let mut permitted = permission(json!({}), json!({}), "30");
        let fee = CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("wallet:ethereum".into()),
            },
            amount: Amount {
                asset: asset("ETH"),
                atoms: Quantity::new("7").unwrap(),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let scope = financial_plans::FinancialPlanCumulativeScope {
            scope_id: "shared-fees".into(),
            maximum_debits: vec![resource_amount("acct", "USDT", "40")],
            maximum_costs: vec![fee.clone()],
            maximum_obligations: vec![],
        };
        permitted.permission.as_mut().unwrap().spec.maximum_costs = vec![fee.clone()];
        permitted.permission.as_mut().unwrap().cumulative_scope = Some(scope);
        // The same resource and asset with a different treatment must not satisfy
        // this cap. Surface that difference instead of suggesting a larger number.
        let mut wrong_treatment = fee.clone();
        wrong_treatment.treatment = CostTreatment::IncludedInDebit;
        spec.aggregate_bounds.maximum_costs = vec![wrong_treatment];
        let mut second = permitted.clone();
        second.step_id = "step-2".into();
        let mut step = spec.steps[0].clone();
        step.step_id = second.step_id.clone();
        step.completion.principal_spend = None;
        step.completion.operation_succeeded = true;
        spec.steps.push(step);
        let permissions = vec![permitted, second];
        let error = validate_plan_permissions(&spec, &permissions).unwrap_err();
        assert_eq!(error.code, "PLAN_CUMULATIVE_SCOPE_OUTSIDE_AGGREGATE");
        let details = error.details.unwrap();
        let errors = details["validation_errors"].as_array().unwrap();
        assert_eq!(errors.len(), 4); // shared debit + fee and each step's fee identity
        assert_eq!(errors[0]["details"]["required_atoms"], "40");
        assert_eq!(errors[0]["details"]["maximum_atoms"], "30");
        let fee_detail = &errors[1]["details"];
        assert_eq!(fee_detail["scope_id"], "shared-fees");
        assert_eq!(fee_detail["steps"].as_array().unwrap().len(), 2);
        assert_eq!(fee_detail["steps"][1]["step_id"], "step-2");
        assert_eq!(fee_detail["steps"][0]["operation"], "synthetic.write");
        assert_eq!(fee_detail["asset"], json!(fee.amount.asset));
        assert_eq!(fee_detail["location"], json!(fee.location));
        assert_eq!(
            fee_detail["treatment"],
            json!(CostTreatment::AdditionalDebit)
        );
        assert_eq!(fee_detail["required_atoms"], "7");
        assert!(fee_detail["maximum_atoms"].is_null());
        assert_eq!(
            fee_detail["declared_cost_bounds"][0]["treatment"],
            json!(CostTreatment::IncludedInDebit)
        );
        assert_eq!(errors[2]["details"]["step_id"], "step-1");
        assert_eq!(errors[3]["details"]["step_id"], "step-2");

        spec.aggregate_bounds.maximum_debits[0].amount.atoms = Quantity::new("40").unwrap();
        let mut insufficient_fee = fee.clone();
        insufficient_fee.amount.atoms = Quantity::new("1").unwrap();
        spec.aggregate_bounds.maximum_costs = vec![insufficient_fee, fee];
        validate_plan_permissions(&spec, &permissions).unwrap();
    }

    #[test]
    fn required_completion_spend_must_fit_aggregate_without_summing_optional_caps() {
        let original = prior_record();
        for (steps, target, aggregate, succeeds) in [
            (1, "20", "19", false),
            (2, "20", "30", false),
            (2, "20", "40", true),
            (2, "10", "20", true),
        ] {
            let mut spec = original.revision.spec.clone();
            spec.steps.clear();
            spec.aggregate_bounds.maximum_debits = vec![resource_amount("acct", "USDT", aggregate)];
            let mut permissions = Vec::new();
            for index in 0..steps {
                let mut step = step_spec();
                step.step_id = format!("step-{index}");
                step.completion.principal_spend = Some(resource_amount("acct", "USDT", target));
                let mut permitted = permission(json!({}), json!({}), "100");
                permitted.step_id = step.step_id.clone();
                spec.steps.push(step);
                permissions.push(permitted);
            }
            let result = validate_plan_permissions(&spec, &permissions);
            if succeeds {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.code, "PLAN_COMPLETION_OUTSIDE_AGGREGATE");
                assert_eq!(error.details.unwrap()["maximum_atoms"], aggregate);
            }
        }
    }

    #[test]
    fn required_completion_spend_keeps_exact_resource_and_asset_identities_separate() {
        let original = prior_record();
        let mut spec = original.revision.spec.clone();
        spec.steps.clear();
        spec.aggregate_bounds.maximum_debits.clear();
        let mut permissions = Vec::new();
        for (index, (resource, token)) in [("acct", "USDT"), ("other", "USDT"), ("acct", "ETH")]
            .into_iter()
            .enumerate()
        {
            let amount = resource_amount(resource, token, "20");
            let mut step = step_spec();
            step.step_id = format!("step-{index}");
            step.completion.principal_spend = Some(amount.clone());
            let mut permitted = permission(json!({}), json!({}), "100");
            permitted.step_id = step.step_id.clone();
            permitted.permission.as_mut().unwrap().spec.maximum_debits = vec![amount.clone()];
            spec.aggregate_bounds.maximum_debits.push(amount);
            spec.steps.push(step);
            permissions.push(permitted);
        }
        validate_plan_permissions(&spec, &permissions).unwrap();
    }

    pub(crate) fn prior_record() -> FinancialPlanAuthorizationRecord {
        let spec = financial_plans::FinancialPlanSpec {
            version: 1,
            name: "Plan".into(),
            summary: None,
            steps: vec![step_spec()],
            aggregate_bounds: financial_plans::FinancialPlanBounds {
                maximum_debits: vec![resource_amount("acct", "USDT", "30")],
                maximum_costs: vec![],
            },
        };
        let permissions = vec![permission(
            json!({"account": "a"}),
            json!({"qty": "1..50"}),
            "30",
        )];
        FinancialPlanAuthorizationRecord {
            revision: FinancialPlanRevision {
                authorization_source: None,
                yolo_provenance: None,
                plan_id: "plan".into(),
                revision_id: "rev-1".into(),
                spec,
                permissions,
                approval_binding_hash: "hash".into(),
                approved_at: "2026-01-01T00:00:00Z".into(),
                expires_at: "2027-01-01T00:00:00Z".into(),
                valid_for_seconds: 600,
                status: FinancialPlanAuthorizationStatus::Expired,
                replaces_revision_id: None,
            },
            approval: gofin_domain::ApprovalAudit {
                source: None,
                yolo_provenance: None,
                mechanism: "synthetic".into(),
                binding_hash: "hash".into(),
                challenge_nonce: "nonce".into(),
                approved_at: "2026-01-01T00:00:00Z".into(),
                expires_at: "2027-01-01T00:00:00Z".into(),
            },
            request_fingerprint: "fingerprint".into(),
            usage: FinancialPlanUsage {
                consumed_debits: vec![resource_amount("acct", "USDT", "20")],
                ..FinancialPlanUsage::default()
            },
            attempts: vec![FinancialPlanAttemptLinkage {
                plan_id: "plan".into(),
                revision_id: "rev-1".into(),
                step_id: "step-1".into(),
                attempt_id: "attempt-1".into(),
                request_fingerprint: "fingerprint".into(),
                execution_binding_hash: "hash".into(),
                submission_id: None,
                result: None,
                logical_request_id: None,
            }],
            active_attempt_id: None,
        }
    }

    struct AuthorityExpiryAdapter;
    #[async_trait::async_trait]
    impl gofin_domain::Adapter for AuthorityExpiryAdapter {
        fn definitions(&self) -> Vec<gofin_domain::Definition> {
            vec![gofin_domain::Definition {
                name: "synthetic.write".into(),
                description: "synthetic write with a signed deadline".into(),
                kind: OperationKind::Write,
                execution_actor: gofin_domain::ExecutionActor::AgentInvocable,
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                error_schema: json!({"type":"object"}),
                examples: vec![],
                coverage: "synthetic".into(),
                completion: "synthetic".into(),
                semantics: json!({
                    "authority_expiry_fields":[{"pointer":"/deadline","format":"unix_seconds"}]
                }),
            }]
        }
        async fn read(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(json!({}))
        }
        async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
            Err(internal("not used in authority expiry unit scenario"))
        }
        fn validate_boundary_candidate(
            &self,
            _: &BoundarySpec,
            _: &PreparedOperation,
        ) -> Result<()> {
            Ok(())
        }
        async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
            Ok(prepared.clone())
        }
        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn gofin_domain::SubmissionJournal>,
        ) -> Result<Submission> {
            Err(internal("not used in authority expiry unit scenario"))
        }
        async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
            Err(internal("not used in authority expiry unit scenario"))
        }
    }

    #[test]
    fn named_plan_candidate_rejects_deadline_after_revision_expiry() {
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![Arc::new(AuthorityExpiryAdapter)],
            Arc::new(PlanTestAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let mut revision = prior_record().revision;
        revision.expires_at = "2027-01-01T00:10:00Z".into();
        let prepared = PreparedOperation {
            operation: "synthetic.write".into(),
            input: json!({"request_id":"seed-a","deadline":"1798762201"}),
            binding: json!({}),
            summary: "synthetic write".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: "2027-01-01T00:00:00Z".into(),
            expires_at: "2027-01-01T00:02:00Z".into(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let error = engine
            .validate_plan_candidate(
                &AuthorityExpiryAdapter,
                &revision,
                "step-1",
                &prepared.input,
                &prepared.input,
                "attempt-1",
                &prepared,
            )
            .unwrap_err();
        assert_eq!(error.code, "BOUNDARY_VIOLATION");
        assert_eq!(
            error.details.unwrap()["boundary_violation"]["constraint"],
            "deadline"
        );
    }

    #[test]
    fn broker_generated_attempt_inputs_are_stable_per_attempt_and_preserve_variable_inputs() {
        let mut record = prior_record();
        record.revision.spec.steps[0].input =
            json!({"request_id":"approved-template","quantity":"1"});
        record.revision.permissions[0].generated_input_fields = vec!["/request_id".into()];
        let raw = json!({"request_id":"approved-template","quantity":"2"});
        let first = plan_attempt_input(&record.revision, "step-1", &raw, "attempt-1").unwrap();
        let repeated = plan_attempt_input(&record.revision, "step-1", &raw, "attempt-1").unwrap();
        let second = plan_attempt_input(&record.revision, "step-1", &raw, "attempt-2").unwrap();
        assert_eq!(first, repeated);
        assert_ne!(first["request_id"], second["request_id"]);
        assert_eq!(first["quantity"], "2");
        assert!(first["request_id"].as_str().unwrap().starts_with("fp-"));
        assert!(first["request_id"].as_str().unwrap().len() <= 32);

        let tampered = json!({"request_id":"caller-replacement","quantity":"2"});
        assert_eq!(
            plan_attempt_input(&record.revision, "step-1", &tampered, "attempt-3")
                .unwrap_err()
                .code,
            "PLAN_STEP_SCOPE_MISMATCH"
        );
        let mut exact = record.revision.clone();
        exact.permissions[0].permission = None;
        exact.permissions[0].exact = Some(FinancialPlanExactPermission {
            scope: FinancialPlanExactScope {
                mode: financial_plans::FinancialPlanExactMode::Exact,
                maximum_debits: vec![],
                maximum_costs: vec![],
                possible_credits: vec![],
            },
            definition_hash: "definition".into(),
            connections_hash: "connections".into(),
        });
        assert_eq!(
            plan_attempt_input(&exact, "step-1", &raw, "attempt-4")
                .unwrap_err()
                .code,
            "PLAN_STEP_SCOPE_MISMATCH"
        );
    }

    #[test]
    fn generated_attempt_metadata_requires_a_registered_string_property_and_matching_candidate() {
        let definition = Definition {
            name: "synthetic.write".into(),
            description: "synthetic".into(),
            kind: OperationKind::Write,
            execution_actor: gofin_domain::ExecutionActor::AgentInvocable,
            input_schema: json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"requestId":{"type":"string","maxLength":128}},
                "required":["requestId"],
                "additionalProperties":false
            }),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic".into(),
            completion: "synthetic".into(),
            semantics: json!({"broker_generated_attempt_input_fields":["/requestId"]}),
        };
        let template = json!({"requestId":"template-value"});
        let fields = generated_attempt_input_fields(&definition, &template, false).unwrap();
        assert_eq!(fields, vec!["/requestId"]);
        let value = generated_attempt_input_value("plan", "revision", "step", "attempt").unwrap();
        validate_generated_attempt_input_schema(
            &definition.input_schema,
            &template,
            &fields,
            &value,
        )
        .unwrap();
        let mut wrong = template.clone();
        wrong["requestId"] = json!(4);
        assert_eq!(
            generated_attempt_input_fields(&definition, &wrong, false)
                .unwrap_err()
                .code,
            "PLAN_GENERATED_INPUT_METADATA_INVALID"
        );
        assert_eq!(
            generated_attempt_input_fields(&definition, &template, true)
                .unwrap_err()
                .code,
            "PLAN_READ_SCOPE_INVALID"
        );
        let expected = json!({"requestId":value});
        validate_prepared_generated_input(&fields, &expected, &expected).unwrap();
        let changed = json!({"requestId":"different"});
        assert_eq!(
            validate_prepared_generated_input(&fields, &expected, &changed)
                .unwrap_err()
                .code,
            "PLAN_GENERATED_INPUT_MISMATCH"
        );
    }

    #[test]
    fn started_revision_preserves_identity_completion_and_fixed_scope_but_allows_input_and_bound_changes()
     {
        let prior = prior_record();
        let mut next = prior.revision.spec.clone();
        next.name = "Renamed plan".into();
        next.steps[0].input = json!({"request_id": "seed-b"});
        next.steps[0].permitted_variations = json!({"quantity": {"maximum": "60"}});
        next.aggregate_bounds.maximum_debits = vec![resource_amount("acct", "USDT", "25")];
        let permissions = vec![permission(
            json!({"account": "a"}),
            json!({"qty": "1..60"}),
            "25",
        )];
        let store = Store::in_memory().unwrap();
        validate_revision_transition(Some(&prior), &next, &permissions, &prior.usage, &store)
            .unwrap();

        let changed_fixed = vec![permission(
            json!({"account": "other"}),
            json!({"qty": "1..60"}),
            "25",
        )];
        assert!(
            validate_revision_transition(Some(&prior), &next, &changed_fixed, &prior.usage, &store)
                .is_err()
        );

        let mut changed_completion = next.clone();
        changed_completion.steps[0].completion.principal_spend =
            Some(resource_amount("acct", "USDT", "21"));
        assert!(
            validate_revision_transition(
                Some(&prior),
                &changed_completion,
                &permissions,
                &prior.usage,
                &store
            )
            .is_err()
        );
    }

    #[test]
    fn started_step_bounds_cover_consumption_by_identity_not_vector_order() {
        let consumed = vec![
            resource_amount("acct-a", "USDT", "5"),
            resource_amount("acct-b", "ETH", "2"),
        ];
        let reordered = vec![
            resource_amount("acct-b", "ETH", "2"),
            resource_amount("acct-a", "USDT", "5"),
        ];
        ensure_bounds_cover_usage(&reordered, &consumed, &[], &[]).unwrap();
        let too_low = vec![
            resource_amount("acct-b", "ETH", "2"),
            resource_amount("acct-a", "USDT", "4"),
        ];
        assert!(ensure_bounds_cover_usage(&too_low, &consumed, &[], &[]).is_err());
        let wrong_identity = vec![
            resource_amount("acct-b", "ETH", "2"),
            resource_amount("acct-c", "USDT", "5"),
        ];
        assert!(ensure_bounds_cover_usage(&wrong_identity, &consumed, &[], &[]).is_err());
    }

    #[test]
    fn historical_plan_json_roundtrips_and_remains_cas_updatable_without_new_fields() {
        let record = prior_record();
        let old_json = to_value(&record).unwrap();
        assert!(
            old_json["revision"]["spec"]["steps"][0]["completion"]
                .get("operation_succeeded")
                .is_none()
        );
        assert!(
            old_json["revision"]["permissions"][0]
                .get("exact")
                .is_none()
        );
        assert!(old_json["attempts"][0].get("result").is_none());
        let restored: FinancialPlanAuthorizationRecord = from_value(old_json.clone()).unwrap();
        assert_eq!(to_value(&restored).unwrap(), old_json);

        let mut store = Store::in_memory().unwrap();
        let request_id = "old-plan-cas-request";
        let fingerprint = "old-plan-cas-fingerprint";
        store
            .begin_financial_plan_request(request_id, fingerprint, "plan", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .commit_financial_plan_revision(
                request_id,
                fingerprint,
                "plan",
                "rev-1",
                None,
                &old_json["revision"],
                &old_json,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        let stored_old_json = store.financial_plan("plan").unwrap().unwrap();
        let mut loaded: FinancialPlanAuthorizationRecord =
            from_value(stored_old_json.clone()).unwrap();
        assert_eq!(to_value(&loaded).unwrap(), stored_old_json);
        loaded.usage.consumed_debits[0].amount.atoms = Quantity::new("21").unwrap();
        let updated = to_value(&loaded).unwrap();
        assert!(
            updated["revision"]["spec"]["steps"][0]["completion"]
                .get("operation_succeeded")
                .is_none()
        );
        assert!(updated["revision"]["permissions"][0].get("exact").is_none());
        store
            .compare_update_financial_plan(
                "plan",
                &stored_old_json,
                &updated,
                "2026-01-01T00:00:01Z",
            )
            .unwrap();
        assert_eq!(store.financial_plan("plan").unwrap(), Some(updated));
    }

    #[test]
    fn started_exact_revision_preserves_input_and_completion_but_can_change_caps_above_usage() {
        let mut prior = prior_record();
        prior.revision.spec.steps[0].input = json!({"account":"protected-account"});
        prior.revision.permissions[0].generated_input_fields = vec!["/requestId".into()];
        let original_scope = FinancialPlanExactScope {
            mode: financial_plans::FinancialPlanExactMode::Exact,
            maximum_debits: vec![resource_amount("acct", "USDT", "30")],
            maximum_costs: vec![],
            possible_credits: vec![],
        };
        prior.revision.permissions[0] = FinancialPlanStepPermission {
            step_id: "step-1".into(),
            generated_input_fields: vec!["/requestId".into()],
            exact: Some(FinancialPlanExactPermission {
                scope: original_scope.clone(),
                definition_hash: "definition".into(),
                connections_hash: "connections".into(),
            }),
            permission: None,
        };
        let store = Store::in_memory().unwrap();
        let next_with_cap = |amount: &str| {
            let mut spec = prior.revision.spec.clone();
            spec.aggregate_bounds.maximum_debits = vec![resource_amount("acct", "USDT", amount)];
            let mut exact = original_scope.clone();
            exact.maximum_debits = vec![resource_amount("acct", "USDT", amount)];
            let permissions = vec![FinancialPlanStepPermission {
                step_id: "step-1".into(),
                generated_input_fields: vec!["/requestId".into()],
                exact: Some(FinancialPlanExactPermission {
                    scope: exact,
                    definition_hash: "definition".into(),
                    connections_hash: "connections".into(),
                }),
                permission: None,
            }];
            (spec, permissions)
        };
        let (allowed, allowed_permissions) = next_with_cap("25");
        assert!(
            validate_revision_transition(
                Some(&prior),
                &allowed,
                &allowed_permissions,
                &prior.usage,
                &store,
            )
            .is_ok()
        );

        let mut changed_generated_fields = allowed_permissions.clone();
        changed_generated_fields[0].generated_input_fields = vec!["/clientId".into()];
        assert_eq!(
            validate_revision_transition(
                Some(&prior),
                &allowed,
                &changed_generated_fields,
                &prior.usage,
                &store,
            )
            .unwrap_err()
            .code,
            "PLAN_REVISION_SCOPE_CHANGED"
        );

        let (below_usage, below_permissions) = next_with_cap("19");
        assert!(
            validate_revision_transition(
                Some(&prior),
                &below_usage,
                &below_permissions,
                &prior.usage,
                &store,
            )
            .is_err()
        );

        let mut changed_input = allowed.clone();
        changed_input.steps[0].input["account"] = json!("different-account");
        assert!(
            validate_revision_transition(
                Some(&prior),
                &changed_input,
                &allowed_permissions,
                &prior.usage,
                &store,
            )
            .is_err()
        );

        let mut changed_completion = allowed.clone();
        changed_completion.steps[0].completion.principal_spend =
            Some(resource_amount("acct", "USDT", "21"));
        assert!(
            validate_revision_transition(
                Some(&prior),
                &changed_completion,
                &allowed_permissions,
                &prior.usage,
                &store,
            )
            .is_err()
        );
    }

    #[test]
    fn unrepresentable_plan_expiry_is_rejected_by_duration_validation() {
        assert!(plan_duration(u64::MAX).is_err());
        // Fits in i64 and the duration type, but exceeds chrono's representable
        // UTC timestamp range when added to the current time.
        assert!(plan_duration(10_000_000_000_000).is_err());
    }

    #[test]
    fn first_plan_bridge_claim_can_bind_a_lower_approved_minimum_for_same_targets() {
        let target =
            |resource: &str, asset_ref: &str, minimum: &str| gofin_domain::BridgeSettlementTarget {
                resource: ResourceId(resource.into()),
                asset: asset(asset_ref),
                minimum_atoms: Quantity::new(minimum).unwrap(),
            };
        let quote = gofin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:destination", "USDC", "9883478507950693")],
            refund: vec![target("wallet:source", "ETH", "123")],
        };
        let selected = gofin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:destination", "USDC", "9840000000000000")],
            refund: vec![target("wallet:source", "ETH", "100")],
        };

        assert!(bridge_expectation_preserves_quote_targets(
            &quote, &selected
        ));
    }

    #[test]
    fn first_plan_bridge_claim_rejects_changed_target_identity() {
        let target =
            |resource: &str, asset_ref: &str, minimum: &str| gofin_domain::BridgeSettlementTarget {
                resource: ResourceId(resource.into()),
                asset: asset(asset_ref),
                minimum_atoms: Quantity::new(minimum).unwrap(),
            };
        let quote = gofin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:destination", "USDC", "100")],
            refund: vec![target("wallet:source", "ETH", "20")],
        };
        let changed_route = gofin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:attacker", "USDC", "1")],
            refund: vec![target("wallet:source", "ETH", "1")],
        };
        let changed_asset = gofin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:destination", "OTHER", "1")],
            refund: vec![target("wallet:source", "ETH", "1")],
        };

        assert!(!bridge_expectation_preserves_quote_targets(
            &quote,
            &changed_route
        ));
        assert!(!bridge_expectation_preserves_quote_targets(
            &quote,
            &changed_asset
        ));
    }

    #[test]
    fn next_step_selection_never_skips_a_blocked_or_unresolved_first_incomplete_step() {
        let progress = |step_id: &str, state, blockers: Vec<String>| FinancialPlanStepProgress {
            step_id: step_id.into(),
            state,
            attempts: vec![],
            consumed_debits: vec![],
            consumed_costs: vec![],
            verified_receipts: vec![],
            human_resolutions: vec![],
            blockers,
        };
        let blocked_sequence = vec![
            progress("done", FinancialPlanStepProgressState::Completed, vec![]),
            progress(
                "blocked",
                FinancialPlanStepProgressState::Blocked,
                vec!["Remaining step authority cannot satisfy its completion target.".into()],
            ),
            progress("later", FinancialPlanStepProgressState::NotStarted, vec![]),
        ];
        assert_eq!(select_next_eligible_step(&blocked_sequence), None);

        let unresolved_sequence = vec![
            progress(
                "first",
                FinancialPlanStepProgressState::InProgress,
                vec!["A previous attempt must be reconciled.".into()],
            ),
            progress("later", FinancialPlanStepProgressState::NotStarted, vec![]),
        ];
        assert_eq!(select_next_eligible_step(&unresolved_sequence), None);

        let partial_sequence = vec![
            progress(
                "partial",
                FinancialPlanStepProgressState::InProgress,
                vec![],
            ),
            progress("later", FinancialPlanStepProgressState::NotStarted, vec![]),
        ];
        assert_eq!(
            select_next_eligible_step(&partial_sequence).as_deref(),
            Some("partial")
        );
    }
    fn funding_record(
        scope: Option<financial_plans::FinancialPlanFundingScope>,
        proceeds: &str,
    ) -> FinancialPlanAuthorizationRecord {
        let mut plan = prior_record();
        plan.usage = FinancialPlanUsage::default();
        let mut producer = step_spec();
        producer.step_id = "producer".into();
        plan.revision.spec.steps.insert(0, producer);
        let mut producer_permission = plan.revision.permissions[0].clone();
        producer_permission.step_id = "producer".into();
        producer_permission
            .permission
            .as_mut()
            .unwrap()
            .possible_credits = vec![FinancialPlanFundingIdentity {
            resource: ResourceId("acct".into()),
            asset: asset("USDT"),
        }];
        plan.revision.permissions.insert(0, producer_permission);
        plan.revision.spec.steps[1].funding_dependencies =
            vec![financial_plans::FinancialPlanFundingDependency {
                producing_step_id: "producer".into(),
                resource: ResourceId("acct".into()),
                asset: asset("USDT"),
                funding_scope: scope,
                maximum_fraction_bps: None,
            }];
        let fee = CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("acct".into()),
            },
            amount: Amount {
                asset: asset("USDT"),
                atoms: Quantity::from_u64(2),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        plan.revision.spec.aggregate_bounds.maximum_costs = vec![fee.clone()];
        plan.revision.permissions[1]
            .permission
            .as_mut()
            .unwrap()
            .spec
            .maximum_costs = vec![fee];
        plan.usage.verified_proceeds = vec![FinancialPlanProceeds {
            producing_step_id: "producer".into(),
            resource: ResourceId("acct".into()),
            asset: asset("USDT"),
            available: Quantity::new(proceeds).unwrap(),
            consumed: Quantity::zero(),
        }];
        plan
    }
    fn funded_prepared(principal: &str, fee: &str) -> PreparedOperation {
        PreparedOperation {
            operation: "synthetic.write".into(),
            input: json!({}),
            binding: json!({}),
            summary: "synthetic".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![resource_amount("acct", "USDT", principal)],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash {
                        resource: ResourceId("acct".into()),
                    },
                    amount: Amount {
                        asset: asset("USDT"),
                        atoms: Quantity::new(fee).unwrap(),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
                expected: vec![],
                locks: vec![],
            },
            operation_version: 1,
            action_deadline: None,
        }
    }

    fn coinbase_withdrawal_exact_scope(
        principal: &str,
        reserve: &str,
    ) -> (
        financial_plans::FinancialPlanExactScope,
        financial_plans::FinancialPlanBounds,
        Value,
    ) {
        let resource = ResourceId("coinbase:portfolio:SYNTHETIC".into());
        let usdc = AssetId {
            network: "coinbase".into(),
            reference: "USDC".into(),
            decimals: 36,
        };
        let base_usdc = AssetId {
            network: "ethereum:8453".into(),
            reference: "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".into(),
            decimals: 6,
        };
        let debit = ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: usdc.clone(),
                atoms: Quantity::from_decimal(principal, 36).unwrap(),
            },
        };
        let fee = CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: usdc.clone(),
                atoms: Quantity::from_decimal(reserve, 36).unwrap(),
            },
            treatment: CostTreatment::VenueCalculatedAdditionalDebit,
        };
        let scope = financial_plans::FinancialPlanExactScope {
            mode: financial_plans::FinancialPlanExactMode::Exact,
            maximum_debits: vec![debit.clone()],
            maximum_costs: vec![fee.clone()],
            possible_credits: vec![FinancialPlanFundingIdentity {
                resource: ResourceId(
                    "ethereum:8453:0x0000000000000000000000000000000000000001".into(),
                ),
                asset: base_usdc,
            }],
        };
        let aggregate = financial_plans::FinancialPlanBounds {
            maximum_debits: vec![debit],
            maximum_costs: vec![fee],
        };
        let input = json!({
            "account":"coinbase",
            "accountId":"00000000-0000-4000-8000-000000000001",
            "destination":"synthetic-wallet",
            "address":"0x0000000000000000000000000000000000000001",
            "network":"base",
            "amount":principal,
            "feeMode":"venue_calculated",
            "feeReserveUsdc":reserve,
            "requestId":"synthetic-withdrawal"
        });
        (scope, aggregate, input)
    }

    #[test]
    fn exact_coinbase_venue_fee_scope_binds_observed_network_token_recipient_identities() {
        for (network, asset_network, address, token, decimals) in [
            (
                "base",
                "ethereum:8453",
                "0x0000000000000000000000000000000000000001",
                "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                6,
            ),
            (
                "polygon",
                "ethereum:137",
                "0x0000000000000000000000000000000000000001",
                "native",
                18,
            ),
            (
                "solana",
                "solana:mainnet-beta",
                "11111111111111111111111111111112",
                "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                6,
            ),
        ] {
            let (mut scope, aggregate, mut input) = coinbase_withdrawal_exact_scope("1.5", "0.1");
            input["network"] = json!(network);
            input["address"] = json!(address);
            input["token"] = json!(token);
            scope.possible_credits[0].resource = ResourceId(format!("{asset_network}:{address}"));
            scope.possible_credits[0].asset = AssetId {
                network: asset_network.into(),
                reference: token.into(),
                decimals,
            };
            validate_exact_coinbase_venue_fee_policy(
                "coinbase.withdraw",
                &input,
                &scope,
                &aggregate,
                &[],
            )
            .unwrap();
            for mismatch in ["asset_network", "token", "precision", "resource"] {
                let mut bad_input = input.clone();
                let mut bad_scope = scope.clone();
                match mismatch {
                    "asset_network" => {
                        bad_scope.possible_credits[0].asset.network = "ethereum:424242".into()
                    }
                    "token" => {
                        bad_input["token"] = json!("0x0000000000000000000000000000000000000002")
                    }
                    "precision" => bad_scope.possible_credits[0].asset.decimals = 0,
                    _ => bad_scope.possible_credits[0].resource.0.push_str("-other"),
                }
                assert_eq!(
                    validate_exact_coinbase_venue_fee_policy(
                        "coinbase.withdraw",
                        &bad_input,
                        &bad_scope,
                        &aggregate,
                        &[]
                    )
                    .unwrap_err()
                    .code,
                    "PLAN_VENUE_CALCULATED_FEE_UNSUPPORTED",
                    "{network}: {mismatch}"
                );
            }
        }
    }

    #[test]
    fn exact_venue_fee_plan_review_accepts_only_bound_coinbase_usdc_scope() {
        let (scope, aggregate, input) = coinbase_withdrawal_exact_scope("1", "0.1");
        validate_exact_coinbase_venue_fee_policy(
            "coinbase.withdraw",
            &input,
            &scope,
            &aggregate,
            &[],
        )
        .unwrap();

        let mut wrong_currency = input.clone();
        wrong_currency["currency"] = json!("ETH");
        assert_eq!(
            validate_exact_coinbase_venue_fee_policy(
                "coinbase.withdraw",
                &wrong_currency,
                &scope,
                &aggregate,
                &[],
            )
            .unwrap_err()
            .code,
            "PLAN_VENUE_CALCULATED_FEE_UNSUPPORTED"
        );

        let mut wrong_asset = scope.clone();
        wrong_asset.maximum_costs[0].amount.asset.reference = "USD".into();
        assert!(
            validate_exact_coinbase_venue_fee_policy(
                "coinbase.withdraw",
                &input,
                &wrong_asset,
                &aggregate,
                &[],
            )
            .is_err()
        );

        let mut wrong_recipient = scope.clone();
        wrong_recipient.possible_credits[0].resource =
            ResourceId("ethereum:8453:0x0000000000000000000000000000000000000002".into());
        assert!(
            validate_exact_coinbase_venue_fee_policy(
                "coinbase.withdraw",
                &input,
                &wrong_recipient,
                &aggregate,
                &[],
            )
            .is_err()
        );

        let mut wrong_source = scope.clone();
        wrong_source.maximum_debits[0].resource = ResourceId("coinbase:account:id".into());
        wrong_source.maximum_costs[0].location = AccountingLocation::Cash {
            resource: wrong_source.maximum_debits[0].resource.clone(),
        };
        assert!(
            validate_exact_coinbase_venue_fee_policy(
                "coinbase.withdraw",
                &input,
                &wrong_source,
                &aggregate,
                &[],
            )
            .is_err()
        );

        let mut extra_cost = scope.clone();
        extra_cost
            .maximum_costs
            .push(scope.maximum_costs[0].clone());
        assert!(
            validate_exact_coinbase_venue_fee_policy(
                "coinbase.withdraw",
                &input,
                &extra_cost,
                &aggregate,
                &[],
            )
            .is_err()
        );
        assert!(
            validate_exact_coinbase_venue_fee_policy(
                "synthetic.withdraw",
                &input,
                &scope,
                &aggregate,
                &[],
            )
            .is_err()
        );

        let dependency =
            |funding_scope| gofin_domain::financial_plans::FinancialPlanFundingDependency {
                producing_step_id: "sale".into(),
                resource: scope.maximum_debits[0].resource.clone(),
                asset: scope.maximum_debits[0].amount.asset.clone(),
                funding_scope,
                maximum_fraction_bps: None,
            };
        assert_eq!(
            validate_exact_coinbase_venue_fee_policy(
                "coinbase.withdraw",
                &input,
                &scope,
                &aggregate,
                &[dependency(None)],
            )
            .unwrap_err()
            .code,
            "PLAN_VENUE_CALCULATED_FEE_UNSUPPORTED",
            "legacy/default all_debits cannot fund unknown venue fees"
        );
        validate_exact_coinbase_venue_fee_policy(
            "coinbase.withdraw",
            &input,
            &scope,
            &aggregate,
            &[dependency(Some(
                gofin_domain::financial_plans::FinancialPlanFundingScope::PrincipalOnly,
            ))],
        )
        .unwrap();
    }
    fn save_funding_attempt(
        plan: &mut FinancialPlanAuthorizationRecord,
        store: &mut Store,
        step_id: &str,
        attempt_id: &str,
        effects: Vec<FinancialEffect>,
    ) {
        let mut operation = settled_effect_record(effects);
        operation.id = attempt_id.into();
        operation.request_id = format!("request-{attempt_id}");
        operation.fingerprint = format!("fingerprint-{attempt_id}");
        operation.operation = "synthetic.write".into();
        plan.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: plan.revision.plan_id.clone(),
            revision_id: plan.revision.revision_id.clone(),
            step_id: step_id.into(),
            attempt_id: attempt_id.into(),
            request_fingerprint: operation.fingerprint.clone(),
            execution_binding_hash: "synthetic-binding".into(),
            logical_request_id: None,
            submission_id: None,
            result: None,
        });
        store
            .create_request(
                &operation.id,
                &operation.request_id,
                &operation.fingerprint,
                &operation.operation,
                None,
                "completed",
                &to_value(&operation).unwrap(),
                &operation.created_at,
            )
            .unwrap();
    }

    fn proceeds_credit(amount: &str) -> FinancialEffect {
        FinancialEffect::UsableCredit {
            to: ResourceId("acct".into()),
            amount: Amount {
                asset: asset("USDT"),
                atoms: Quantity::new(amount).unwrap(),
            },
        }
    }

    fn proceeds_debit(amount: &str) -> FinancialEffect {
        FinancialEffect::CashDebit {
            resource: ResourceId("acct".into()),
            amount: Amount {
                asset: asset("USDT"),
                atoms: Quantity::new(amount).unwrap(),
            },
        }
    }

    #[test]
    fn principal_only_full_proceeds_keeps_fee_caps_and_reservations_independent() {
        use financial_plans::FinancialPlanFundingScope as Scope;
        let prepared = funded_prepared("10", "1");
        for legacy in [None, Some(Scope::AllDebits)] {
            assert_eq!(
                validate_plan_funding(&funding_record(legacy, "10"), "step-1", &prepared)
                    .unwrap_err()
                    .code,
                "PLAN_FUNDING_NOT_SETTLED"
            );
        }
        let mut plan = funding_record(Some(Scope::PrincipalOnly), "10");
        validate_plan_funding(&plan, "step-1", &prepared).unwrap();
        validate_plan_aggregate_remaining(&plan, &prepared).unwrap();
        assert_eq!(
            validate_plan_funding(&plan, "step-1", &funded_prepared("11", "1"))
                .unwrap_err()
                .code,
            "PLAN_FUNDING_NOT_SETTLED"
        );
        assert_eq!(
            validate_plan_funding(&plan, "step-1", &funded_prepared("0", "1"))
                .unwrap_err()
                .code,
            "PLAN_FUNDING_UNUSED"
        );
        assert!(validate_plan_aggregate_remaining(&plan, &funded_prepared("10", "3")).is_err());
        plan.usage
            .reservations
            .push(financial_plans::FinancialPlanReservation {
                attempt_id: "other-attempt".into(),
                step_id: "step-1".into(),
                debits: vec![],
                costs: vec![prepared.intent.maximum_costs[0].clone()],
            });
        assert!(validate_plan_aggregate_remaining(&plan, &funded_prepared("10", "2")).is_err());
        let mut fee_only = plan.clone();
        fee_only.revision.permissions[1]
            .permission
            .as_mut()
            .unwrap()
            .spec
            .maximum_debits
            .clear();
        fee_only.revision.spec.steps[1].completion.principal_spend = None;
        fee_only.revision.spec.steps[1]
            .completion
            .operation_succeeded = true;
        assert_eq!(
            validate_plan_permissions(&fee_only.revision.spec, &fee_only.revision.permissions)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_UNUSED"
        );
        let mut ambiguous = plan.clone();
        let duplicate = ambiguous.revision.spec.steps[1].funding_dependencies[0].clone();
        ambiguous.revision.spec.steps[1]
            .funding_dependencies
            .push(duplicate);
        assert!(ambiguous.revision.spec.validate().is_err());
        assert_eq!(
            validate_plan_permissions(&ambiguous.revision.spec, &ambiguous.revision.permissions)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_AMBIGUOUS"
        );
    }

    #[test]
    fn zero_permission_caps_are_not_evidence_of_funding_consumption() {
        use financial_plans::FinancialPlanFundingScope as Scope;
        for scope in [Scope::PrincipalOnly, Scope::AllDebits] {
            let mut plan = funding_record(Some(scope), "10");
            let step = &mut plan.revision.spec.steps[1];
            step.completion.principal_spend = None;
            step.completion.operation_succeeded = true;
            let permission = plan.revision.permissions[1].permission.as_mut().unwrap();
            for debit in &mut permission.spec.maximum_debits {
                debit.amount.atoms = Quantity::zero();
            }
            for cost in &mut permission.spec.maximum_costs {
                cost.amount.atoms = Quantity::zero();
            }
            assert_eq!(
                validate_plan_permissions(&plan.revision.spec, &plan.revision.permissions)
                    .unwrap_err()
                    .code,
                "PLAN_FUNDING_UNUSED"
            );
        }
    }

    #[test]
    fn principal_only_funding_keeps_fee_overrun_separate_and_fully_booked() {
        let mut plan = funding_record(
            Some(financial_plans::FinancialPlanFundingScope::PrincipalOnly),
            "10",
        );
        let permission = plan.revision.permissions[1].permission.as_mut().unwrap();
        permission.spec.maximum_costs[0].treatment = CostTreatment::VenueCalculatedAdditionalDebit;
        plan.revision.spec.aggregate_bounds.maximum_costs[0].treatment =
            CostTreatment::VenueCalculatedAdditionalDebit;

        let mut candidate = funded_prepared("10", "2");
        candidate.operation = "coinbase.withdraw".into();
        candidate.input = json!({"network":"base","feeMode":"venue_calculated","amount":"10","feeReserveUsdc":"2"});
        candidate.intent.maximum_costs[0].treatment = CostTreatment::VenueCalculatedAdditionalDebit;
        validate_plan_funding(&plan, "step-1", &candidate).unwrap();
        assert_eq!(
            validate_plan_funding(
                &funding_record(
                    Some(financial_plans::FinancialPlanFundingScope::PrincipalOnly),
                    "9",
                ),
                "step-1",
                &candidate,
            )
            .unwrap_err()
            .code,
            "PLAN_FUNDING_NOT_SETTLED",
            "producer proceeds must cover principal; the adapter separately guards principal plus reserve from cash"
        );
        let mut corrupt_usage = funding_record(
            Some(financial_plans::FinancialPlanFundingScope::PrincipalOnly),
            "10",
        );
        corrupt_usage.usage.verified_proceeds[0].consumed = Quantity::new("11").unwrap();
        assert_eq!(
            validate_plan_funding(&corrupt_usage, "step-1", &candidate)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_USAGE_INVALID"
        );

        let fee_asset = asset("USDT");
        let fee_resource = ResourceId("acct".into());
        let mut operation = settled_effect_record(vec![
            FinancialEffect::CashDebit {
                resource: fee_resource.clone(),
                amount: Amount {
                    asset: fee_asset.clone(),
                    atoms: Quantity::new("10").unwrap(),
                },
            },
            FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: fee_resource,
                },
                amount: Amount {
                    asset: fee_asset,
                    atoms: Quantity::new("7").unwrap(),
                },
                category: "venue_withdrawal_fee".into(),
                treatment: CostTreatment::VenueCalculatedAdditionalDebit,
            },
        ]);
        operation.id = "venue-withdrawal-attempt".into();
        plan.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: plan.revision.plan_id.clone(),
            revision_id: plan.revision.revision_id.clone(),
            step_id: "step-1".into(),
            attempt_id: operation.id.clone(),
            request_fingerprint: operation.fingerprint.clone(),
            execution_binding_hash: "synthetic-binding".into(),
            logical_request_id: None,
            submission_id: None,
            result: None,
        });

        account_financial_plan_effects(&mut plan, &operation).unwrap();
        assert_eq!(plan.usage.consumed_costs[0].amount.atoms.as_str(), "7");
        assert_eq!(plan.usage.verified_proceeds[0].consumed.as_str(), "10");
        assert_eq!(
            remaining_costs(
                &plan.revision.spec.aggregate_bounds.maximum_costs,
                Some(&plan.usage.consumed_costs),
            )
            .unwrap()[0]
                .amount
                .atoms
                .as_str(),
            "0",
            "an overrun consumes approved fee authority and creates no future fee headroom"
        );
    }

    #[test]
    fn funding_fraction_limits_the_cumulative_share_of_verified_receipts() {
        let mut plan = funding_record(None, "10");
        plan.attempts.clear();
        plan.revision.spec.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(5_000);
        let half = funded_prepared("5", "0");
        validate_plan_funding(&plan, "step-1", &half).unwrap();

        let above_half = funded_prepared("6", "0");
        assert_eq!(
            validate_plan_funding(&plan, "step-1", &above_half)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_FRACTION_EXCEEDED"
        );

        plan.usage.verified_proceeds[0].consumed = Quantity::new("1").unwrap();
        let remaining_within_half = funded_prepared("4", "0");
        validate_plan_funding(&plan, "step-1", &remaining_within_half).unwrap();
        assert_eq!(
            validate_plan_funding(&plan, "step-1", &funded_prepared("5", "0"))
                .unwrap_err()
                .code,
            "PLAN_FUNDING_FRACTION_EXCEEDED"
        );
    }

    #[test]
    fn fraction_is_per_consumer_while_global_proceeds_still_prevent_double_spend() {
        let mut plan = funding_record(None, "10");
        plan.attempts.clear();
        plan.revision.spec.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(5_000);
        let mut second_consumer = plan.revision.spec.steps[1].clone();
        second_consumer.step_id = "step-2".into();
        plan.revision.spec.steps.push(second_consumer);
        let mut second_permission = plan.revision.permissions[1].clone();
        second_permission.step_id = "step-2".into();
        plan.revision.permissions.push(second_permission);
        let mut store = Store::in_memory().unwrap();
        save_funding_attempt(
            &mut plan,
            &mut store,
            "producer",
            "producer-attempt",
            vec![proceeds_credit("10")],
        );
        save_funding_attempt(
            &mut plan,
            &mut store,
            "step-1",
            "first-consumer-attempt",
            vec![proceeds_debit("5")],
        );

        // Each dependency gets its own 50% allowance even though the first
        // consumer already used half of the same producer's verified receipt.
        validate_plan_funding_with_store(&plan, "step-2", &funded_prepared("5", "0"), &store)
            .unwrap();
        // Even a wider second-step fraction cannot bypass the separate global
        // verified-proceeds balance after the first consumer's debit.
        plan.usage.verified_proceeds[0].consumed = Quantity::new("5").unwrap();
        plan.revision.spec.steps[2].funding_dependencies[0].maximum_fraction_bps = Some(10_000);
        assert_eq!(
            validate_plan_funding_with_store(&plan, "step-2", &funded_prepared("6", "0"), &store,)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_NOT_SETTLED"
        );
    }

    pub(crate) fn assert_human_fraction_consumption(store: &Store, operation: &OperationRecord) {
        let mut plan = funding_record(
            Some(financial_plans::FinancialPlanFundingScope::PrincipalOnly),
            "100",
        );
        plan.attempts.clear();
        let identity = &operation.preview.as_ref().unwrap().intent.debits[0];
        let dependency = &mut plan.revision.spec.steps[1].funding_dependencies[0];
        dependency.resource = identity.resource.clone();
        dependency.asset = identity.amount.asset.clone();
        dependency.maximum_fraction_bps = Some(1_500);
        plan.usage.verified_proceeds[0].resource = identity.resource.clone();
        plan.usage.verified_proceeds[0].asset = identity.amount.asset.clone();
        plan.usage.verified_proceeds[0].consumed = Quantity::from_u64(10);
        plan.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: plan.revision.plan_id.clone(),
            revision_id: plan.revision.revision_id.clone(),
            step_id: "step-1".into(),
            attempt_id: operation.id.clone(),
            request_fingerprint: operation.fingerprint.clone(),
            execution_binding_hash: "synthetic".into(),
            logical_request_id: None,
            submission_id: None,
            result: None,
        });
        let mut next = funded_prepared("5", "0");
        next.intent.debits[0].resource = identity.resource.clone();
        next.intent.debits[0].amount.asset = identity.amount.asset.clone();
        next.intent.maximum_costs.clear();
        assert_eq!(
            replay_plan_funding_consumption(&plan, store)
                .unwrap()
                .values()
                .next()
                .unwrap()
                .to_string(),
            "10"
        );
        validate_plan_funding_with_store(&plan, "step-1", &next, store).unwrap();
        next.intent.debits[0].amount.atoms = Quantity::from_u64(6);
        assert_eq!(
            validate_plan_funding_with_store(&plan, "step-1", &next, store)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_FRACTION_EXCEEDED"
        );
    }

    #[test]
    fn repeated_attempts_cannot_exceed_one_consumers_fraction() {
        let mut plan = funding_record(None, "10");
        plan.attempts.clear();
        plan.revision.spec.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(5_000);
        let mut store = Store::in_memory().unwrap();
        save_funding_attempt(
            &mut plan,
            &mut store,
            "producer",
            "producer-attempt",
            vec![proceeds_credit("10")],
        );
        save_funding_attempt(
            &mut plan,
            &mut store,
            "step-1",
            "consumer-attempt",
            vec![proceeds_debit("1")],
        );
        validate_plan_funding_with_store(&plan, "step-1", &funded_prepared("4", "0"), &store)
            .unwrap();
        assert_eq!(
            validate_plan_funding_with_store(&plan, "step-1", &funded_prepared("5", "0"), &store,)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_FRACTION_EXCEEDED"
        );
    }

    #[test]
    fn replay_attributes_consumption_from_external_bridge_receipts() {
        let mut plan = funding_record(None, "10");
        plan.attempts.clear();
        plan.revision.spec.steps[0].operation = "bridge.execute".into();
        plan.revision.spec.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(5_000);
        // Destination proof is stored separately from the bridge source
        // operation's immutable effects (which contain no usable receipt).
        plan.usage.verified_proceeds[0].consumed = Quantity::new("3").unwrap();
        let mut store = Store::in_memory().unwrap();
        save_funding_attempt(
            &mut plan,
            &mut store,
            "producer",
            "bridge-source-attempt",
            vec![FinancialEffect::TransitPrincipal {
                transfer_id: "synthetic-bridge".into(),
                amount: Amount {
                    asset: asset("USDT"),
                    atoms: Quantity::new("10").unwrap(),
                },
            }],
        );
        save_funding_attempt(
            &mut plan,
            &mut store,
            "step-1",
            "settled-consumer-attempt",
            vec![proceeds_debit("3")],
        );

        let consumed = replay_plan_funding_consumption(&plan, &store).unwrap();
        assert_eq!(consumed.values().next().unwrap().to_string(), "3");
        validate_plan_funding_with_store(&plan, "step-1", &funded_prepared("2", "0"), &store)
            .unwrap();
        assert_eq!(
            validate_plan_funding_with_store(&plan, "step-1", &funded_prepared("3", "0"), &store,)
                .unwrap_err()
                .code,
            "PLAN_FUNDING_FRACTION_EXCEEDED"
        );
    }

    #[test]
    fn started_funding_consumer_cannot_broaden_its_approved_fraction_on_revision() {
        let mut prior = funding_record(None, "10");
        prior.revision.spec.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(5_000);
        prior.attempts.push(FinancialPlanAttemptLinkage {
            plan_id: prior.revision.plan_id.clone(),
            revision_id: prior.revision.revision_id.clone(),
            step_id: "step-1".into(),
            attempt_id: "started-fraction-consumer-attempt".into(),
            request_fingerprint: "synthetic-fingerprint".into(),
            execution_binding_hash: "synthetic-binding".into(),
            logical_request_id: None,
            submission_id: None,
            result: None,
        });
        let store = Store::in_memory().unwrap();

        let unchanged = prior.revision.spec.clone();
        validate_revision_transition(
            Some(&prior),
            &unchanged,
            &prior.revision.permissions,
            &prior.usage,
            &store,
        )
        .unwrap();

        let mut broadened = prior.revision.spec.clone();
        broadened.steps[1].funding_dependencies[0].maximum_fraction_bps = Some(7_500);
        assert_eq!(
            validate_revision_transition(
                Some(&prior),
                &broadened,
                &prior.revision.permissions,
                &prior.usage,
                &store,
            )
            .unwrap_err()
            .code,
            "PLAN_REVISION_HISTORY_CONFLICT"
        );
    }

    #[test]
    fn zero_candidate_principal_respects_exact_bounded_and_aggregate_fee_authority() {
        let store = Store::in_memory().unwrap();
        for exact in [false, true] {
            let mut plan = prior_record();
            plan.attempts.clear();
            plan.usage = FinancialPlanUsage::default();
            plan.revision.spec.steps[0].completion.principal_spend = None;
            plan.revision.spec.steps[0].completion.operation_succeeded = true;
            let mut prepared = funded_prepared("0", "5");
            let fees = prepared.intent.maximum_costs.clone();
            plan.revision.spec.aggregate_bounds.maximum_debits.clear();
            plan.revision.spec.aggregate_bounds.maximum_costs = fees.clone();
            let permission = &mut plan.revision.permissions[0];
            if exact {
                permission.permission = None;
                permission.exact = Some(FinancialPlanExactPermission {
                    scope: financial_plans::FinancialPlanExactScope {
                        mode: financial_plans::FinancialPlanExactMode::Exact,
                        maximum_debits: vec![],
                        maximum_costs: fees,
                        possible_credits: vec![],
                    },
                    definition_hash: "unchanged-definition".into(),
                    connections_hash: "unchanged-connection".into(),
                });
            } else {
                let scope = &mut permission.permission.as_mut().unwrap().spec;
                scope.maximum_debits.clear();
                scope.maximum_costs = fees;
            }
            validate_candidate_plan_caps(&plan, "step-1", &prepared, &store).unwrap();
            validate_plan_aggregate_remaining(&plan, &prepared).unwrap();

            // A zero placeholder must not create principal authority in either
            // the step or the cumulative plan, even for the same exact asset.
            prepared.intent.debits[0].amount.atoms = Quantity::from_u64(1);
            assert_eq!(
                validate_candidate_plan_caps(&plan, "step-1", &prepared, &store)
                    .unwrap_err()
                    .code,
                "BOUNDARY_VIOLATION"
            );
            assert_eq!(
                validate_plan_aggregate_remaining(&plan, &prepared)
                    .unwrap_err()
                    .code,
                "BOUNDARY_VIOLATION"
            );
            prepared.intent.debits[0].amount.atoms = Quantity::zero();
            prepared.intent.maximum_costs[0].amount.atoms = Quantity::from_u64(6);
            assert_eq!(
                validate_candidate_plan_caps(&plan, "step-1", &prepared, &store)
                    .unwrap_err()
                    .code,
                "BOUNDARY_VIOLATION"
            );
            assert_eq!(
                validate_plan_aggregate_remaining(&plan, &prepared)
                    .unwrap_err()
                    .code,
                "BOUNDARY_VIOLATION"
            );
        }
    }

    #[test]
    fn named_plan_fee_cap_applies_to_fee_bearing_cancellation_without_principal() {
        let fee = CostBound {
            location: AccountingLocation::Cash {
                resource: ResourceId("acct".into()),
            },
            amount: Amount {
                asset: asset("USDT"),
                atoms: Quantity::new("5").unwrap(),
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut plan = prior_record();
        plan.attempts.clear();
        plan.revision.permissions[0]
            .permission
            .as_mut()
            .unwrap()
            .spec
            .maximum_costs = vec![fee.clone()];
        plan.revision.spec.aggregate_bounds.maximum_costs = vec![fee.clone()];
        let mut prepared = funded_prepared("0", "5");
        prepared.intent.debits.clear();
        prepared.intent.maximum_costs = vec![fee];
        prepared.intent.expected = vec![EffectConstraint::SubmissionCancellation {
            operation_id: "original-order".into(),
            submission: Submission {
                kind: "synthetic_tx".into(),
                reference: "original-signature".into(),
                data: json!({"request_id":"synthetic-original"}),
            },
        }];
        let store = Store::in_memory().unwrap();
        validate_candidate_plan_caps(&plan, "step-1", &prepared, &store).unwrap();

        prepared.intent.maximum_costs[0].amount.atoms = Quantity::new("6").unwrap();
        assert_eq!(
            validate_candidate_plan_caps(&plan, "step-1", &prepared, &store)
                .unwrap_err()
                .code,
            "BOUNDARY_VIOLATION"
        );
    }
    #[test]
    fn principal_only_settlement_orders_costs_safely_and_survives_restart_and_revision_checks() {
        use financial_plans::FinancialPlanFundingScope as Scope;
        let prepared = funded_prepared("10", "1");
        let principal = FinancialEffect::CashDebit {
            resource: ResourceId("acct".into()),
            amount: prepared.intent.debits[0].amount.clone(),
        };
        let fee = FinancialEffect::Cost {
            location: prepared.intent.maximum_costs[0].location.clone(),
            amount: prepared.intent.maximum_costs[0].amount.clone(),
            category: "synthetic-gas".into(),
            treatment: CostTreatment::AdditionalDebit,
        };
        let mut usages = vec![];
        for effects in [vec![principal.clone(), fee.clone()], vec![fee, principal]] {
            let mut plan = funding_record(Some(Scope::PrincipalOnly), "10");
            account_financial_plan_effects(&mut plan, &settled_effect_record(effects)).unwrap();
            assert_eq!(plan.usage.verified_proceeds[0].consumed.as_str(), "10");
            assert_eq!(plan.usage.consumed_costs[0].amount.atoms.as_str(), "1");
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("principal-only.sqlite");
            let serialized = serde_json::to_value(&plan).unwrap();
            {
                let mut store = Store::open(&path).unwrap();
                store
                    .begin_financial_plan_request(
                        "synthetic-funding",
                        "fingerprint",
                        "plan",
                        &now(),
                    )
                    .unwrap();
                store
                    .commit_financial_plan_revision(
                        "synthetic-funding",
                        "fingerprint",
                        "plan",
                        "rev-1",
                        None,
                        &serde_json::to_value(&plan.revision).unwrap(),
                        &serialized,
                        &now(),
                    )
                    .unwrap();
            }
            let store = Store::open(&path).unwrap();
            let recovered: FinancialPlanAuthorizationRecord =
                serde_json::from_value(store.financial_plan("plan").unwrap().unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&recovered).unwrap(), serialized);
            assert_eq!(
                validate_plan_funding(&recovered, "step-1", &funded_prepared("1", "1"))
                    .unwrap_err()
                    .code,
                "PLAN_FUNDING_NOT_SETTLED"
            );
            let mut next = recovered.revision.spec.clone();
            next.steps[1].funding_dependencies[0].funding_scope = None;
            assert_eq!(
                validate_revision_transition(
                    Some(&recovered),
                    &next,
                    &recovered.revision.permissions,
                    &recovered.usage,
                    &store
                )
                .unwrap_err()
                .code,
                "PLAN_REVISION_HISTORY_CONFLICT"
            );
            usages.push(plan.usage);
        }
        assert_eq!(usages[0], usages[1]);
    }
    struct FundingGasRpc {
        balance: u64,
    }
    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for FundingGasRpc {
        async fn request(&self, method: &str, _params: Value) -> Result<Value> {
            Ok(match method {
                "eth_chainId" => json!("0x1"),
                "eth_call" => json!("0x"),
                "eth_estimateGas" => json!("0x5208"),
                "eth_getBlockByNumber" => json!({"baseFeePerGas":"0x1"}),
                "eth_maxPriorityFeePerGas" => json!("0x1"),
                "eth_getTransactionCount" => json!("0x0"),
                "eth_getBalance" => json!(format!("{:#x}", self.balance)),
                _ => panic!("unexpected synthetic funding RPC {method}"),
            })
        }
    }
    #[tokio::test]
    async fn principal_only_does_not_let_full_proceeds_pay_their_own_maximum_gas() {
        use gofin_evm::alloy::primitives::{Address, Bytes, U256};
        let plan = funding_record(
            Some(financial_plans::FinancialPlanFundingScope::PrincipalOnly),
            "1000000",
        );
        validate_plan_funding(
            &plan,
            "step-1",
            &funded_prepared("1000000", "1000000000000"),
        )
        .unwrap();
        for (balance, success) in [(1_000_000, false), (1_000_001_000_000, true)] {
            let runtime = gofin_evm::EvmRuntime::new(Arc::new(FundingGasRpc { balance }), 1, None);
            let result = runtime
                .prepare(
                    Address::repeat_byte(1),
                    Address::repeat_byte(2),
                    U256::from(1_000_000),
                    Bytes::new(),
                    U256::from(1_000_000_000_000u64),
                )
                .await;
            if success {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().code, "INSUFFICIENT_GAS_BALANCE");
            }
        }
    }
}
