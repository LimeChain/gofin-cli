use super::*;
use fin_domain::financial_plans::{
    self, FinancialPlanAttemptLinkage, FinancialPlanAuthorizationRecord,
    FinancialPlanAuthorizationRequest, FinancialPlanAuthorizationStatus,
    FinancialPlanExactPermission, FinancialPlanExactScope, FinancialPlanFundingIdentity,
    FinancialPlanProceeds, FinancialPlanReview, FinancialPlanRevision, FinancialPlanStatus,
    FinancialPlanStepPermission, FinancialPlanStepProgress, FinancialPlanStepProgressState,
    FinancialPlanUsage,
};

impl Engine {
    /// Prepare balance-independent adapter permissions and return the canonical
    /// review hash. Preview is read-only and never creates an approval request.
    pub async fn preview_financial_plan(
        &self,
        request: FinancialPlanAuthorizationRequest,
    ) -> Result<FinancialPlanReview> {
        self.validate_plan_request(&request)?;
        let permissions = self.prepare_plan_permissions(&request.plan).await?;
        validate_plan_permissions(&request.plan, &permissions)?;
        validate_plan_authorization_duration(request.valid_for_seconds, &permissions)?;
        let prior = self.plan_replacement(&request)?;
        let carried_forward_usage = prior
            .as_ref()
            .map(|record| record.usage.clone())
            .unwrap_or_default();
        {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            validate_revision_transition(
                prior.as_ref(),
                &request.plan,
                &permissions,
                &carried_forward_usage,
                &store,
            )?;
        }
        let review_hash = financial_plans::financial_plan_review_hash(
            &request.plan,
            &permissions,
            request.valid_for_seconds,
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
            valid_for_seconds: request.valid_for_seconds,
            replaces_revision_id: request.replaces_revision_id,
            plan_id: prior.as_ref().map(|record| record.revision.plan_id.clone()),
            carried_forward_usage,
            remaining_debits,
            remaining_costs,
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

        let permissions = self.prepare_plan_permissions(&request.plan).await?;
        validate_plan_permissions(&request.plan, &permissions)?;
        validate_plan_authorization_duration(request.valid_for_seconds, &permissions)?;
        let prior = self.plan_replacement(&request)?;
        let carried_usage = prior
            .as_ref()
            .map(|record| record.usage.clone())
            .unwrap_or_default();
        let review_hash = financial_plans::financial_plan_review_hash(
            &request.plan,
            &permissions,
            request.valid_for_seconds,
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
                "Run `fin plans preview` again and review the regenerated plan before authorization.",
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
            validate_revision_transition(
                prior.as_ref(),
                &request.plan,
                &permissions,
                &carried_usage,
                &store,
            )?;
        }
        let binding = financial_plans::financial_plan_approval_binding(
            &plan_id,
            &revision_id,
            &request.plan,
            &permissions,
            request.valid_for_seconds,
            request.replaces_revision_id.as_deref(),
            &carried_usage,
        );
        let approved_binding_hash = binding_hash(&binding)?;
        plan_duration(request.valid_for_seconds)?;
        let challenge = AuthorizationChallenge {
            purpose: "financial_plan".into(),
            operation_id: plan_id.clone(),
            binding,
            binding_hash: approved_binding_hash.clone(),
            summary: format!(
                "Approve the plan “{}” with {} ordered steps for {} seconds.",
                request.plan.name,
                request.plan.steps.len(),
                request.valid_for_seconds
            ),
            nonce: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            expires_at: (Utc::now() + chrono::Duration::seconds(90)).to_rfc3339(),
        };
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            if !store
                .begin_financial_plan_request(&request.request_id, &fingerprint, &plan_id, &now())
                .map_err(map_store)?
            {
                return Err(plan_error(
                    "PLAN_REQUEST_UNAVAILABLE",
                    "This plan request is already being processed.",
                    "Inspect plan status and retry only with the original request identity.",
                ));
            }
        }
        let approval = match self.authorizer.authorize(&challenge).await {
            Ok(approval) => approval,
            Err(error) => {
                self.store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .fail_financial_plan_request(&request.request_id, &fingerprint, &now())
                    .map_err(map_store)?;
                return Err(error);
            }
        };
        if let Err(error) = validate_approval(&approval, &challenge, Utc::now()) {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .fail_financial_plan_request(&request.request_id, &fingerprint, &now())
                .map_err(map_store)?;
            return Err(error);
        }
        let approved_at = parse_time(&approval.approved_at)?;
        let seconds = i64::try_from(request.valid_for_seconds).map_err(|_| {
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
        let revision = FinancialPlanRevision {
            plan_id: plan_id.clone(),
            revision_id: revision_id.clone(),
            spec: request.plan,
            permissions,
            approval_binding_hash: approved_binding_hash,
            approved_at: approval.approved_at.clone(),
            expires_at,
            valid_for_seconds: request.valid_for_seconds,
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
        let timestamp = now();
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

    pub async fn financial_plan_status(&self, plan_id: &str) -> Result<FinancialPlanStatus> {
        let _guard = self.write_queue.lock().await;
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
            let _ = self.reconcile_serial(attempt_id).await?;
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
                    &now(),
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
            let definition_hash = binding_hash(&json!({
                "operation": definition.name,
                "kind": definition.kind,
                "execution_actor": definition.execution_actor,
                "input_schema": definition.input_schema,
                "semantics": definition.semantics,
            }))?;
            if definition_hash != exact.definition_hash
                || stable_connection_hash(&adapter.authorization_identities())?
                    != exact.connections_hash
            {
                return Err(plan_error(
                    "PLAN_OPERATION_CONTRACT_CHANGED",
                    "The registered read contract or protected connection identity changed after approval.",
                    "Review and authorize a fresh plan revision.",
                ));
            }
            let output = adapter.read(operation, input).await?;
            validate_plan_json_schema(&definition.output_schema, &output)?;
            return Ok(PreparedOperation {
                operation: operation.into(),
                input: input.clone(),
                binding: json!({"plan_read_preview": true, "result": output}),
                summary: format!("Read-only plan step `{step_id}`."),
                checks: vec![],
                warnings: vec![],
                observed_at: now(),
                expires_at: now(),
                valuation: None,
                intent: fin_domain::FinancialIntent::default(),
                operation_version: 1,
            });
        }
        let preview_attempt_id = Uuid::new_v4().to_string();
        let execution_input =
            plan_attempt_input(&record.revision, step_id, input, &preview_attempt_id)?;
        let context = self.preparation_context(None)?;
        let prepared = bounded_evidence(
            operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare_with_context(operation, &execution_input, &context),
        )
        .await?;
        self.validate_plan_candidate(
            &*adapter,
            &record.revision,
            step_id,
            input,
            &preview_attempt_id,
            &prepared,
        )?;
        {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            validate_candidate_plan_caps(&record, step_id, &prepared, &store)?;
        }
        validate_prepared(&prepared, operation, Utc::now())?;
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
        self.execute_financial_plan_step_serial(plan_id, step_id, request, None)
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
        self.execute_financial_plan_step_serial(
            plan_id,
            step_id,
            request,
            Some(previous_attempt_id),
        )
        .await
    }

    async fn execute_financial_plan_step_serial(
        &self,
        plan_id: &str,
        step_id: &str,
        request: OperationRequest,
        replacement: Option<&str>,
    ) -> Result<OperationRecord> {
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
        let mut plan = self.load_financial_plan(plan_id)?;
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
                || parse_time(&plan.revision.expires_at)? <= Utc::now()
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
        let execution_input = plan_attempt_input(&plan.revision, step_id, &request.input, &id)?;
        let mut execution_request = request.clone();
        execution_request.input = execution_input.clone();
        let context = self.preparation_context(None)?;
        let mut prepared = bounded_evidence(
            &request.operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare_with_context(&request.operation, &execution_input, &context),
        )
        .await?;
        let required_origins = self.cost_origin_assets(&prepared, None)?;
        self.complete_cost_origin_prices(&mut prepared, &required_origins)
            .await?;
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        self.validate_plan_candidate(
            &*adapter,
            &plan.revision,
            step_id,
            &request.input,
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
        validate_plan_funding(&plan, step_id, &prepared)?;

        let timestamp = now();
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
            preview: Some(prepared.clone()),
            approval: Some(plan.approval.clone()),
            submission: None,
            dispatch_evidence: None,
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
            submission_id: None,
            result: None,
        });
        plan.active_attempt_id = Some(id.clone());
        plan.usage
            .reservations
            .push(fin_domain::financial_plans::FinancialPlanReservation {
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
            previous.error = Some(FinError::new(
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
                    &request.request_id,
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
                        &request.request_id,
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
                        &request.request_id,
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
                Some(prepared),
                super::ExecutionRoute::Financial,
            )
            .await;
        match result {
            Ok(record) => Ok(record),
            Err(error) => {
                if !operation.status.terminal() {
                    operation.status = if operation.submission.is_some() {
                        Status::Unknown
                    } else {
                        status_for_error(&error)
                    };
                    operation.error = Some(error);
                    operation.updated_at = now();
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
                Ok(operation)
            }
        }
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
        adapter: std::sync::Arc<dyn fin_domain::Adapter>,
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
        let definition_hash = binding_hash(&json!({
            "operation": definition.name,
            "kind": definition.kind,
            "execution_actor": definition.execution_actor,
            "input_schema": definition.input_schema,
            "semantics": definition.semantics,
        }))?;
        if definition_hash != exact.definition_hash
            || stable_connection_hash(&adapter.authorization_identities())?
                != exact.connections_hash
        {
            return Err(plan_error(
                "PLAN_OPERATION_CONTRACT_CHANGED",
                "The registered read contract or protected connection identity changed after approval.",
                "Review and authorize a fresh plan revision.",
            ));
        }
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
        let timestamp = now();
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
            submission: None,
            dispatch_evidence: None,
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
            submission_id: None,
            result: None,
        });
        plan.active_attempt_id = Some(id.clone());
        let updated_plan = to_value(&plan)?;
        let created = if let Some(mut previous) = previous_operation {
            previous.status = Status::Rejected;
            previous.updated_at = timestamp.clone();
            previous.error = Some(FinError::new(
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
        operation.updated_at = now();
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
        if request.request_id.trim().is_empty() || request.valid_for_seconds == 0 {
            return Err(plan_error(
                "PLAN_REQUEST_INVALID",
                "A request ID and positive plan duration are required.",
                "Supply a stable request ID and a positive representable duration.",
            ));
        }
        let duration = plan_duration(request.valid_for_seconds)?;
        Utc::now().checked_add_signed(duration).ok_or_else(|| {
            plan_error(
                "PLAN_DURATION_INVALID",
                "Plan expiry cannot be represented from the current time.",
                "Choose a shorter consent duration.",
            )
        })?;
        Ok(())
    }

    async fn prepare_plan_permissions(
        &self,
        spec: &financial_plans::FinancialPlanSpec,
    ) -> Result<Vec<FinancialPlanStepPermission>> {
        let mut permissions = Vec::new();
        for step in &spec.steps {
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
            validate_plan_json_schema(&definition.input_schema, &step.input)?;
            let generated_input_fields =
                generated_attempt_input_fields(&definition, &step.input, is_read)?;
            validate_generated_attempt_input_schema(
                &definition.input_schema,
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
                validate_plan_json_schema(&definition.input_schema, &step.input)?;
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
                let definition_hash = binding_hash(&json!({
                    "operation": definition.name,
                    "kind": definition.kind,
                    "execution_actor": definition.execution_actor,
                    "input_schema": definition.input_schema,
                    "semantics": definition.semantics,
                }))?;
                let connections_hash = stable_connection_hash(&adapter.authorization_identities())?;
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
                let permission = bounded_evidence(
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
                permissions.push(FinancialPlanStepPermission {
                    step_id: step.step_id.clone(),
                    exact: None,
                    generated_input_fields,
                    permission: Some(permission),
                });
            }
        }
        Ok(permissions)
    }

    pub(super) fn validate_plan_candidate(
        &self,
        adapter: &dyn fin_domain::Adapter,
        revision: &FinancialPlanRevision,
        step_id: &str,
        raw_input: &Value,
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
        validate_generated_plan_candidate(
            adapter, revision, step_id, raw_input, attempt_id, prepared,
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
            || parse_time(&record.revision.expires_at)? <= Utc::now()
        {
            return Err(plan_error(
                "PLAN_AUTHORIZATION_INACTIVE",
                "Plan authorization is expired or revoked.",
                "Obtain fresh authorization before starting another step.",
            ));
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
                "Keep the identity field from the plan template; Fin assigns its concrete value per attempt.",
            ));
        }
        if record.active_attempt_id.is_some() {
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

    fn build_plan_status(
        &self,
        record: &FinancialPlanAuthorizationRecord,
        revision: &FinancialPlanRevision,
    ) -> Result<FinancialPlanStatus> {
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
            for attempt in &attempts {
                let operation = self.load_record(&attempt.attempt_id)?;
                if let Some(verified) = &operation.effects {
                    step_effects.extend(verified.effects.clone());
                }
                if operation.status == Status::Completed && operation.operation == "bridge.transfer"
                {
                    completed_bridge_deposit_attempts.extend(
                        bridge_transfers
                            .iter()
                            .filter(|transfer| {
                                plan_bridge_deposit_link(transfer, &revision.plan_id, &step.step_id)
                                    .is_some_and(|link| {
                                        link.operation_id.as_deref() == Some(operation.id.as_str())
                                            && link.state == fin_domain::BridgeStepState::Settled
                                    })
                            })
                            .map(|transfer| transfer.id.clone()),
                    );
                }
                attempt_summaries.push(financial_plans::FinancialPlanAttemptSummary {
                    attempt_id: operation.id.clone(),
                    request_id: operation.request_id.clone(),
                    step_id: step.step_id.clone(),
                    status: operation.status.clone(),
                    submission: operation.submission.clone(),
                    error: operation.error.clone(),
                    result: attempt.result.clone(),
                });
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
            let completed =
                completion_met(&step.completion, &completion_effects, operation_succeeded)?;
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
                            && transfer.state != fin_domain::BridgeTransferState::Completed
                    })
                });
            if awaiting_bridge_destination {
                let recovery_required = bridge_transfers.iter().any(|transfer| {
                    completed_bridge_deposit_attempts
                        .iter()
                        .any(|transfer_id| transfer_id == &transfer.id)
                        && matches!(
                            transfer.state,
                            fin_domain::BridgeTransferState::Refunded
                                | fin_domain::BridgeTransferState::Failed
                                | fin_domain::BridgeTransferState::NeedsAttention
                        )
                });
                blockers.push(if recovery_required {
                    "The bridge source write is complete; its parent has a refund or failure outcome, so the destination target requires recovery instead of resubmission.".into()
                } else {
                    "The bridge source write is complete; wait for its verified destination receipt instead of submitting it again.".into()
                });
            }
            let permission = permission_for_step(revision, &step.step_id)?;
            let exhausted = !completed
                && !unresolved
                && step_authority_cannot_complete(
                    step,
                    permission,
                    &consumed_debits,
                    &remaining_debits,
                )?;
            if exhausted {
                blockers
                    .push("Remaining step authority cannot satisfy its completion target.".into());
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
            });
        }
        let mut blockers = Vec::new();
        if record.active_attempt_id.is_some() {
            blockers.push("A plan attempt is unresolved.".into());
        }
        let authorization_active = revision.status == FinancialPlanAuthorizationStatus::Active
            && parse_time(&revision.expires_at)? > Utc::now();
        let pending_bridge_settlements = bridge_transfers
            .iter()
            .filter(|transfer| {
                !matches!(
                    transfer.state,
                    fin_domain::BridgeTransferState::Completed
                        | fin_domain::BridgeTransferState::Refunded
                        | fin_domain::BridgeTransferState::Failed
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
                    fin_domain::BridgeTransferState::Refunded
                        | fin_domain::BridgeTransferState::Failed
                        | fin_domain::BridgeTransferState::NeedsAttention
                )
            })
            .count();
        if recovery_bridge_count > 0 {
            blockers.push(format!(
                "{recovery_bridge_count} plan-linked bridge transfer(s) require recovery; their principal is not successful destination proceeds."
            ));
        }
        let next = select_next_eligible_step(&steps);
        // Expiry stops dispatching unfinished work. It does not invalidate
        // source operations that already settled or block proof-only bridge
        // reconciliation after all writable steps are done.
        if !authorization_active && next.is_some() {
            blockers.push("Plan authorization is expired, revoked, or superseded.".into());
        }
        Ok(FinancialPlanStatus {
            plan_id: revision.plan_id.clone(),
            revision: revision.clone(),
            usage: record.usage.clone(),
            steps,
            remaining_debits,
            remaining_costs,
            next_eligible_step_id: if record.active_attempt_id.is_some() || !authorization_active {
                None
            } else {
                next
            },
            blockers,
        })
    }
}

pub(super) fn validate_exact_plan_candidate(
    adapter: &dyn fin_domain::Adapter,
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
    validate_plan_json_schema(&definition.input_schema, raw_input)?;
    validate_exact_plan_identity(adapter, operation, exact)
}

fn validate_plan_operation_identity(
    adapter: &dyn fin_domain::Adapter,
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
    adapter: &dyn fin_domain::Adapter,
    operation: &str,
    exact: &FinancialPlanExactPermission,
) -> Result<()> {
    let definition = adapter
        .definitions()
        .into_iter()
        .find(|definition| definition.name == operation)
        .ok_or_else(|| internal("registry definition disappeared"))?;
    let definition_hash = binding_hash(&json!({
        "operation": definition.name,
        "kind": definition.kind,
        "execution_actor": definition.execution_actor,
        "input_schema": definition.input_schema,
        "semantics": definition.semantics,
    }))?;
    let connections_hash = stable_connection_hash(&adapter.authorization_identities())?;
    if definition_hash != exact.definition_hash || connections_hash != exact.connections_hash {
        return Err(plan_error(
            "PLAN_OPERATION_CONTRACT_CHANGED",
            "The registered operation contract or protected connection identity changed after approval.",
            "Review and authorize a fresh plan revision.",
        ));
    }
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

fn plan_bridge_deposit_link<'a>(
    transfer: &'a fin_domain::BridgeTransferRecord,
    plan_id: &str,
    step_id: &str,
) -> Option<&'a fin_domain::BridgeStepLink> {
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
            fin_domain::BridgeStepKind::Deposit | fin_domain::BridgeStepKind::Finalize
        ) && link.plan_authority.as_ref().is_some_and(|authority| {
            authority.plan_id == plan_id && authority.plan_step_id == step_id
        })
    })
}

fn committed_plan_bridge_destination_proof(
    transfer: &fin_domain::BridgeTransferRecord,
    plan_id: &str,
    step_id: &str,
    attempts: &[financial_plans::FinancialPlanAttemptSummary],
) -> bool {
    transfer.state == fin_domain::BridgeTransferState::Completed
        && plan_bridge_deposit_link(transfer, plan_id, step_id).is_some_and(|link| {
            link.state == fin_domain::BridgeStepState::Settled
                && link.operation_id.as_ref().is_some_and(|operation_id| {
                    attempts.iter().any(|attempt| {
                        attempt.attempt_id == *operation_id && attempt.status == Status::Completed
                    })
                })
        })
        && transfer.settlement_proofs.iter().any(|proof| {
            proof.transfer_id == transfer.id
                && proof.outcome == fin_domain::BridgeSettlementOutcome::Destination
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

fn permission_debits(permission: &FinancialPlanStepPermission) -> &[fin_domain::ResourceAmount] {
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

fn permission_costs(permission: &FinancialPlanStepPermission) -> &[fin_domain::CostBound] {
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
                debit.resource == dependency.resource && debit.amount.asset == dependency.asset
            }) || (dependency.includes_additional_costs()
                && permission_costs(permission).iter().any(|cost| {
                    cost.treatment == fin_domain::CostTreatment::AdditionalDebit
                        && cost.location.resource() == &dependency.resource
                        && cost.amount.asset == dependency.asset
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
    validate_cumulative_plan_scopes(spec, permissions)?;
    validate_aggregate_permission_bounds(spec, permissions)
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
                "Choose a plan duration within every step's approved authorization limit and review again.",
            ));
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
    for scope in scopes.values() {
        for debit in &scope.maximum_debits {
            if !spec
                .aggregate_bounds
                .maximum_debits
                .iter()
                .any(|aggregate| {
                    aggregate.resource == debit.resource
                        && aggregate.amount.asset == debit.amount.asset
                        && aggregate.amount.atoms >= debit.amount.atoms
                })
            {
                return Err(plan_error(
                    "PLAN_CUMULATIVE_SCOPE_OUTSIDE_AGGREGATE",
                    "A shared adapter debit cap exceeds the plan's aggregate debit cap.",
                    "Raise the aggregate plan limit only if the complete plan should authorize it.",
                ));
            }
        }
        for cost in &scope.maximum_costs {
            if !spec.aggregate_bounds.maximum_costs.iter().any(|aggregate| {
                aggregate.location == cost.location
                    && aggregate.amount.asset == cost.amount.asset
                    && aggregate.treatment == cost.treatment
                    && aggregate.amount.atoms >= cost.amount.atoms
            }) {
                return Err(plan_error(
                    "PLAN_CUMULATIVE_SCOPE_OUTSIDE_AGGREGATE",
                    "A shared adapter fee cap exceeds the plan's aggregate fee cap.",
                    "Raise the aggregate plan limit only if the complete plan should authorize it.",
                ));
            }
        }
    }
    Ok(())
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
        if cost.amount.atoms.is_zero()
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
                "Exact cost caps must be positive and unique by location, asset, and treatment.",
                "Use one positive maximum cost for each exact fee identity.",
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
    adapter: &dyn fin_domain::Adapter,
    revision: &FinancialPlanRevision,
    step_id: &str,
    raw_input: &Value,
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
    let expected_input = plan_attempt_input(revision, step_id, raw_input, attempt_id)?;
    validate_plan_json_schema(&definition.input_schema, &expected_input)?;
    validate_prepared_generated_input(
        &permission.generated_input_fields,
        &expected_input,
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
                "Keep the identity field from the plan template; Fin assigns its concrete value per attempt.",
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

fn plan_step_actor_allowed(kind: OperationKind, actor: &fin_domain::ExecutionActor) -> bool {
    match kind {
        OperationKind::Read => *actor == fin_domain::ExecutionActor::AgentReadable,
        OperationKind::Write => *actor == fin_domain::ExecutionActor::AgentInvocable,
        _ => false,
    }
}

fn stable_connection_hash(connections: &[fin_domain::ConnectionMetadata]) -> Result<String> {
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
        .map(|value| Ok((fin_domain::canonical_json(&value)?, value)))
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
    for permission in permissions {
        for item in permission_debits(permission) {
            if !spec
                .aggregate_bounds
                .maximum_debits
                .iter()
                .any(|cap| cap.resource == item.resource && cap.amount.asset == item.amount.asset)
            {
                return Err(plan_error(
                    "PLAN_AGGREGATE_IDENTITY_MISMATCH",
                    "Aggregate debit bounds omit a resource and asset used by a step permission.",
                    "Declare a cumulative cap for every permitted debit identity.",
                ));
            }
        }
        for item in permission_costs(permission) {
            if !spec.aggregate_bounds.maximum_costs.iter().any(|cap| {
                cap.location == item.location
                    && cap.amount.asset == item.amount.asset
                    && cap.treatment == item.treatment
            }) {
                return Err(plan_error(
                    "PLAN_AGGREGATE_IDENTITY_MISMATCH",
                    "Aggregate cost bounds omit a fee identity used by a step permission.",
                    "Declare a cumulative cap for every permitted fee identity.",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_candidate_plan_caps(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
    store: &Store,
) -> Result<()> {
    let permission = permission_for_step(&record.revision, step_id)?;
    let scope = plan_boundary_spec(permission)?;
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
    kind: fin_domain::BridgeStepKind,
    expected_transfer: Value,
    updated_transfer: fin_domain::BridgeTransferRecord,
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
    let marker = prepared.binding.get(fin_domain::BRIDGE_STEP_METADATA_KEY);
    if request.operation != "bridge.transfer" {
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
    let kind: fin_domain::BridgeStepKind = serde_json::from_value(
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
    let expectation: fin_domain::BridgeSettlementExpectation = serde_json::from_value(
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
    if store_transfer.grant_id.is_some()
        || (store_transfer.transit.is_some() && !kind.is_destination_stage())
        || store_transfer
            .plan_scope
            .as_ref()
            .is_some_and(|scope| scope.plan_id != plan_id)
        || !matches!(
            store_transfer.state,
            fin_domain::BridgeTransferState::Prepared
                | fin_domain::BridgeTransferState::ApprovalPending
                | fin_domain::BridgeTransferState::SourcePending
                | fin_domain::BridgeTransferState::InTransit
                | fin_domain::BridgeTransferState::DestinationPending
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
        .unwrap_or(fin_domain::BridgePlanScope {
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
    let authority = fin_domain::BridgePlanAuthority {
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
            prior.step_id == *dependency && prior.state == fin_domain::BridgeStepState::Settled
        })
    });
    let link = &mut store_transfer.steps[link_index];
    if link.kind != kind
        || link.grant_id.is_some()
        || link.operation_id.is_some()
        || link.state != fin_domain::BridgeStepState::Unsubmitted
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
    link.operation_id = Some(operation_id.to_owned());
    link.plan_authority = Some(authority);
    link.state = fin_domain::BridgeStepState::Submitted;
    store_transfer.plan_scope = Some(scope);
    if kind.is_destination_stage() && store_transfer.transit.is_none() {
        return Err(plan_error(
            "PLAN_BRIDGE_SOURCE_PROOF_REQUIRED",
            "A deferred bridge stage requires settled original source proof.",
            "Reconcile the initiation before executing its proof or finalization.",
        ));
    }
    store_transfer.state = kind.pending_state();
    store_transfer.version = store_transfer
        .version
        .checked_add(1)
        .ok_or_else(|| internal("bridge transfer version overflow"))?;
    Ok(Some(PlanBridgeAttemptClaim {
        transfer_id,
        bridge_step_id,
        kind,
        expected_transfer,
        updated_transfer: store_transfer,
    }))
}

fn bridge_expectation_preserves_quote_targets(
    quote: &fin_domain::BridgeSettlementExpectation,
    selected: &fin_domain::BridgeSettlementExpectation,
) -> bool {
    let targets_match = |quoted: &[fin_domain::BridgeSettlementTarget],
                         chosen: &[fin_domain::BridgeSettlementTarget]| {
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
            spent_debits = add_amounts(&spent_debits, &reservation.debits)?;
            spent_costs = add_cost_bounds(&spent_costs, &reservation.costs)?;
        }
    }
    let remaining_debits = remaining_debits(&scope.maximum_debits, Some(&spent_debits))?;
    let remaining_costs = remaining_costs(&scope.maximum_costs, Some(&spent_costs))?;
    let authorization = BoundaryAuthorization {
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
    if step.completion.minimum_net_receipt.is_some() {
        let any_principal_capacity = remaining.iter().any(|item| {
            !item.amount.atoms.is_zero()
                && aggregate_remaining_debits.iter().any(|aggregate| {
                    aggregate.resource == item.resource
                        && aggregate.amount.asset == item.amount.asset
                        && !aggregate.amount.atoms.is_zero()
                })
        });
        let deferred_capacity = permission
            .permission
            .as_ref()
            .and_then(|value| value.deferred_receipt.as_ref())
            .zip(step.completion.minimum_net_receipt.as_ref())
            .is_some_and(|(capacity, target)| {
                capacity.resource == target.resource
                    && capacity.amount.asset == target.amount.asset
                    && capacity.amount.atoms >= target.amount.atoms
            });
        if !any_principal_capacity && !deferred_capacity {
            return Ok(true);
        }
    }
    Ok(false)
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

fn validate_plan_funding(
    record: &FinancialPlanAuthorizationRecord,
    step_id: &str,
    prepared: &PreparedOperation,
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
        let debit_required = prepared
            .intent
            .debits
            .iter()
            .filter(|debit| {
                debit.resource == dependency.resource && debit.amount.asset == dependency.asset
            })
            .fold(BigUint::zero(), |sum, debit| {
                sum + debit.amount.atoms.integer()
            });
        let fee_required = prepared
            .intent
            .maximum_costs
            .iter()
            .filter(|cost| {
                cost.treatment == fin_domain::CostTreatment::AdditionalDebit
                    && cost.location.resource() == &dependency.resource
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
        let proceeds = record
            .usage
            .verified_proceeds
            .iter()
            .filter(|item| {
                item.producing_step_id == dependency.producing_step_id
                    && item.resource == dependency.resource
                    && item.asset == dependency.asset
            })
            .fold(BigUint::zero(), |sum, item| {
                sum + item.available.integer() - item.consumed.integer()
            });
        if proceeds < required {
            return Err(plan_error(
                "PLAN_FUNDING_NOT_SETTLED",
                "Verified proceeds from the declared producing step do not cover this attempt.",
                "Reconcile the producing step or reduce the dependent input; unrelated balances cannot satisfy this dependency.",
            ));
        }
    }
    Ok(())
}

fn add_cost_bounds(base: &[CostBound], additions: &[CostBound]) -> Result<Vec<CostBound>> {
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
            _ => {}
        }
    }
    for effect in effects {
        if let FinancialEffect::Cost {
            location,
            amount,
            treatment: fin_domain::CostTreatment::DeductedFromCredit,
            ..
        } = effect
        {
            let key = resource_asset_key(&ResourceAmount {
                resource: location.resource().clone(),
                amount: Amount {
                    asset: amount.asset.clone(),
                    atoms: fin_domain::Quantity::zero(),
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
                atoms: fin_domain::Quantity::new((committed - observed).to_string())?,
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
    let mut debits = Vec::new();
    let mut costs = Vec::new();
    let mut credits = BTreeMap::<String, (ResourceId, fin_domain::AssetId, BigUint)>::new();
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
            treatment: fin_domain::CostTreatment::DeductedFromCredit,
            ..
        } => Some((location, amount)),
        _ => None,
    }) {
        let (location, amount) = cost;
        let key = resource_asset_key(&ResourceAmount {
            resource: location.resource().clone(),
            amount: Amount {
                asset: amount.asset.clone(),
                atoms: fin_domain::Quantity::zero(),
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
    asset: &fin_domain::AssetId,
    quantity: BigUint,
) -> Result<()> {
    let key = resource_asset_key(&ResourceAmount {
        resource: resource.clone(),
        amount: Amount {
            asset: asset.clone(),
            atoms: fin_domain::Quantity::zero(),
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
    let mut debit_map = BTreeMap::<String, (ResourceId, fin_domain::AssetId, BigUint)>::new();
    let mut cost_map = BTreeMap::<
        String,
        (
            fin_domain::AccountingLocation,
            fin_domain::AssetId,
            fin_domain::CostTreatment,
            BigUint,
        ),
    >::new();
    let mut credit_map = BTreeMap::<String, (ResourceId, fin_domain::AssetId, BigUint)>::new();
    for effect in &verified.effects {
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
    for (location, asset, _, fee) in cost_map
        .values()
        .filter(|(_, _, treatment, _)| treatment == &fin_domain::CostTreatment::DeductedFromCredit)
    {
        let key = resource_asset_key(&ResourceAmount {
            resource: location.resource().clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: fin_domain::Quantity::zero(),
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
                atoms: fin_domain::Quantity::new(amount.to_string())?,
            },
        };
        plan.usage.consumed_debits = add_amounts(&plan.usage.consumed_debits, &[item])?;
    }
    for (location, asset, _, amount) in cost_map
        .values()
        .filter(|(_, _, treatment, _)| treatment == &fin_domain::CostTreatment::AdditionalDebit)
    {
        consume_plan_funding(plan, &step_id, location.resource(), asset, amount, true)?;
    }
    for (location, asset, treatment, amount) in cost_map.into_values() {
        plan.usage.consumed_costs = add_cost_bounds(
            &plan.usage.consumed_costs,
            &[CostBound {
                location,
                amount: Amount {
                    asset,
                    atoms: fin_domain::Quantity::new(amount.to_string())?,
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
                    .checked_add(&fin_domain::Quantity::new(amount.to_string())?)?;
            } else {
                plan.usage.verified_proceeds.push(FinancialPlanProceeds {
                    producing_step_id: step_id.clone(),
                    resource: identity.resource,
                    asset: identity.asset,
                    available: fin_domain::Quantity::new(amount.to_string())?,
                    consumed: fin_domain::Quantity::zero(),
                });
            }
        }
    }
    Ok(())
}

fn consume_plan_funding(
    plan: &mut FinancialPlanAuthorizationRecord,
    step_id: &str,
    resource: &ResourceId,
    asset: &fin_domain::AssetId,
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
        let proceeds = plan
            .usage
            .verified_proceeds
            .iter_mut()
            .find(|proceeds| {
                proceeds.producing_step_id == dependency.producing_step_id
                    && &proceeds.resource == resource
                    && &proceeds.asset == asset
            })
            .ok_or_else(|| {
                plan_error(
                    "PLAN_FUNDING_NOT_SETTLED",
                    "The dependent step has no verified proceeds from its declared producer.",
                    "Keep the operation unresolved and reconcile the producing step.",
                )
            })?;
        let available = proceeds.available.integer() - proceeds.consumed.integer();
        let used = std::cmp::min(available, remaining.clone());
        proceeds.consumed = proceeds
            .consumed
            .checked_add(&fin_domain::Quantity::new(used.to_string())?)?;
        remaining -= used;
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
    map: &mut BTreeMap<String, (ResourceId, fin_domain::AssetId, BigUint)>,
    resource: &ResourceId,
    asset: &fin_domain::AssetId,
    amount: BigUint,
) {
    let key = serde_json::to_string(&(resource, asset)).unwrap_or_default();
    let entry = map
        .entry(key)
        .or_insert((resource.clone(), asset.clone(), BigUint::zero()));
    entry.2 += amount;
}

fn validate_revision_transition(
    prior: Option<&FinancialPlanAuthorizationRecord>,
    next: &financial_plans::FinancialPlanSpec,
    next_permissions: &[FinancialPlanStepPermission],
    usage: &FinancialPlanUsage,
    store: &Store,
) -> Result<()> {
    let Some(prior) = prior else {
        return Ok(());
    };
    validate_cumulative_revision_scopes(prior, next_permissions, store)?;
    validate_active_bridge_revision_scopes(prior, next_permissions, store)?;
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
                .any(|attempt| attempt.step_id == step.step_id)
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
            if old.step_id != new.step_id
                || old.operation != new.operation
                || old.completion != new.completion
                || old.funding_dependencies != new.funding_dependencies
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

fn validate_cumulative_revision_scopes(
    prior: &FinancialPlanAuthorizationRecord,
    next_permissions: &[FinancialPlanStepPermission],
    store: &Store,
) -> Result<()> {
    let mut historical = std::collections::BTreeMap::<
        String,
        fin_domain::financial_plans::FinancialPlanCumulativeScope,
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
    for scope in revisions
        .iter()
        .flat_map(|revision| revision.permissions.iter())
        .filter_map(|permission| permission.permission.as_ref()?.cumulative_scope.as_ref())
    {
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

fn validate_active_bridge_revision_scopes(
    prior: &FinancialPlanAuthorizationRecord,
    next_permissions: &[FinancialPlanStepPermission],
    store: &Store,
) -> Result<()> {
    let transfers = store.bridge_transfers().map_err(map_store)?;
    for transfer in transfers.into_iter().filter(|transfer| {
        transfer
            .plan_scope
            .as_ref()
            .is_some_and(|scope| scope.plan_id == prior.revision.plan_id)
            && !matches!(
                transfer.state,
                fin_domain::BridgeTransferState::Completed
                    | fin_domain::BridgeTransferState::Refunded
                    | fin_domain::BridgeTransferState::Failed
            )
    }) {
        let prior_permissions = prior.revision.permissions.iter().filter(|permission| {
            permission
                .permission
                .as_ref()
                .and_then(|permission| permission.cumulative_scope.as_ref())
                .is_some_and(|scope| scope.scope_id == transfer.id)
        });
        let mut found = false;
        for old in prior_permissions {
            found = true;
            let replacement = next_permissions
                .iter()
                .find(|permission| permission.step_id == old.step_id);
            let superseded_unsubmitted = old
                .permission
                .as_ref()
                .and_then(|permission| {
                    permission.spec.fixed["plan_step_identity"]["step_id"].as_str()
                })
                .is_some_and(|id| {
                    transfer.steps.iter().any(|link| {
                        link.step_id == id
                            && link.state == fin_domain::BridgeStepState::Superseded
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
        && parse_time(&record.revision.expires_at)? <= Utc::now()
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
                &now(),
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

pub(super) fn plan_error(code: &str, message: &str, retry: &str) -> FinError {
    FinError::new(code, "financial_plan", message, retry)
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
mod tests {
    use super::*;
    use fin_domain::financial_plans::PlanPermission;
    use fin_domain::{
        AccountingLocation, Amount, AssetId, CostTreatment, FinancialEffect, Quantity, ResourceId,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn asset(reference: &str) -> AssetId {
        AssetId {
            network: "synthetic".into(),
            reference: reference.into(),
            decimals: 2,
        }
    }

    struct GenericReadAdapter {
        reads: AtomicUsize,
        prepares: AtomicUsize,
        changed_connection: AtomicBool,
        changed_authorization_identity: AtomicBool,
        fail_read: AtomicBool,
        read_gate: Option<(Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>)>,
    }

    impl GenericReadAdapter {
        fn definition() -> fin_domain::Definition {
            fin_domain::Definition {
                name: "future.read".into(),
                description: "Synthetic future read".into(),
                kind: fin_domain::OperationKind::Read,
                execution_actor: fin_domain::ExecutionActor::AgentReadable,
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

        fn connection(&self) -> fin_domain::ConnectionMetadata {
            fin_domain::ConnectionMetadata {
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

        fn authorization_identity(&self) -> fin_domain::ConnectionMetadata {
            fin_domain::ConnectionMetadata {
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
    impl fin_domain::Adapter for GenericReadAdapter {
        fn definitions(&self) -> Vec<fin_domain::Definition> {
            vec![Self::definition()]
        }

        fn connections(&self) -> Vec<fin_domain::ConnectionMetadata> {
            vec![self.connection()]
        }

        fn authorization_identities(&self) -> Vec<fin_domain::ConnectionMetadata> {
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
                return Err(FinError::validation("synthetic read failure"));
            }
            Ok(json!({"value":7}))
        }

        async fn prepare(&self, _operation: &str, _input: &Value) -> Result<PreparedOperation> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Err(FinError::validation("generic exact read must not prepare"))
        }

        async fn revalidate(&self, _prepared: &PreparedOperation) -> Result<PreparedOperation> {
            Err(FinError::validation("read must not revalidate"))
        }

        async fn submit(
            &self,
            _prepared: &PreparedOperation,
            _journal: Arc<dyn fin_domain::SubmissionJournal>,
        ) -> Result<Submission> {
            Err(FinError::validation("read must not submit"))
        }

        async fn reconcile(&self, _record: &OperationRecord) -> Result<Reconciliation> {
            Err(FinError::validation("read must not reconcile"))
        }
    }

    struct PlanTestAuthorizer;

    #[async_trait::async_trait]
    impl fin_domain::Authorizer for PlanTestAuthorizer {
        async fn authorize(
            &self,
            challenge: &AuthorizationChallenge,
        ) -> Result<fin_domain::ApprovalAudit> {
            Ok(fin_domain::ApprovalAudit {
                mechanism: "synthetic-plan-test".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: fin_domain::now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    fn generic_read_request(request_id: &str) -> FinancialPlanAuthorizationRequest {
        FinancialPlanAuthorizationRequest {
            request_id: request_id.into(),
            plan: financial_plans::FinancialPlanSpec {
                version: financial_plans::FINANCIAL_PLAN_VERSION,
                name: "Generic exact read".into(),
                steps: vec![financial_plans::FinancialPlanStepSpec {
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
            valid_for_seconds: 600,
            replaces_revision_id: None,
            expected_review_hash: None,
        }
    }

    #[tokio::test]
    async fn generic_registered_read_uses_schema_identity_and_persists_result_without_prepare() {
        let adapter = Arc::new(GenericReadAdapter {
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
            &fin_domain::ExecutionActor::AgentReadable
        ));
        assert!(plan_step_actor_allowed(
            OperationKind::Write,
            &fin_domain::ExecutionActor::AgentInvocable
        ));
        assert!(!plan_step_actor_allowed(
            OperationKind::Write,
            &fin_domain::ExecutionActor::HumanInteractive
        ));
        assert!(!plan_step_actor_allowed(
            OperationKind::Read,
            &fin_domain::ExecutionActor::AgentInvocable
        ));
    }

    #[tokio::test]
    async fn generic_read_failure_is_durable_and_releases_the_unresolved_slot() {
        let adapter = Arc::new(GenericReadAdapter {
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
    fn exact_scope_without_fees_is_valid_but_zero_fee_caps_remain_invalid() {
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
            )
            .unwrap()
        );
    }

    #[test]
    fn deferred_receipt_capacity_grants_no_principal_and_preserves_exact_target() {
        let mut approved = permission(json!({"claim":"fixed"}), json!({}), "1");
        let scope = approved.permission.as_mut().unwrap();
        scope.spec.maximum_debits.clear();
        scope.deferred_receipt = Some(resource_amount("destination", "ETH", "100"));
        let mut step = financial_plans::FinancialPlanStepSpec {
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
        assert!(!step_authority_cannot_complete(&step, &approved, &[], &[]).unwrap());
        assert!(permission_debits(&approved).is_empty());
        for target in [
            resource_amount("other", "ETH", "100"),
            resource_amount("destination", "USDT", "100"),
            resource_amount("destination", "ETH", "101"),
        ] {
            step.completion.minimum_net_receipt = Some(target);
            assert!(step_authority_cannot_complete(&step, &approved, &[], &[]).unwrap());
        }
        step.completion.minimum_net_receipt = Some(resource_amount("destination", "ETH", "100"));
        approved.permission.as_mut().unwrap().deferred_receipt = None;
        assert!(step_authority_cannot_complete(&step, &approved, &[], &[]).unwrap());
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
        let mut transfer = fin_domain::BridgeTransferRecord {
            version: 1,
            id: "transfer-1".into(),
            grant_id: None,
            plan_scope: Some(fin_domain::BridgePlanScope {
                plan_id: "plan-1".into(),
                revision_id: "revision-1".into(),
            }),
            metadata: Value::Null,
            settlement_expectation: None,
            approved_bounds: None,
            prospective_debit: None,
            state: fin_domain::BridgeTransferState::InTransit,
            quote_revisions: Vec::new(),
            steps: vec![fin_domain::BridgeStepLink {
                transfer_id: "transfer-1".into(),
                grant_id: None,
                plan_authority: Some(fin_domain::BridgePlanAuthority {
                    plan_id: "plan-1".into(),
                    revision_id: "revision-1".into(),
                    plan_step_id: "bridge-deposit".into(),
                }),
                step_id: "provider-deposit".into(),
                operation_id: Some("operation-1".into()),
                kind: fin_domain::BridgeStepKind::Deposit,
                depends_on: Vec::new(),
                state: fin_domain::BridgeStepState::Settled,
            }],
            allocation_owner: fin_domain::BridgeAllocationOwner::Transfer {
                transfer_id: "transfer-1".into(),
            },
            transit: None,
            applied_proofs: Vec::new(),
            settlement_proofs: Vec::new(),
        };
        let completed_attempt = vec![financial_plans::FinancialPlanAttemptSummary {
            attempt_id: "operation-1".into(),
            request_id: "request-1".into(),
            step_id: "bridge-deposit".into(),
            status: Status::Completed,
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

        transfer.state = fin_domain::BridgeTransferState::Completed;
        transfer
            .settlement_proofs
            .push(fin_domain::BridgeSettlementProof {
                transfer_id: "transfer-1".into(),
                outcome: fin_domain::BridgeSettlementOutcome::Refund,
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

        transfer.state = fin_domain::BridgeTransferState::Failed;
        transfer.settlement_proofs[0].outcome = fin_domain::BridgeSettlementOutcome::Destination;
        assert!(!committed_plan_bridge_destination_proof(
            &transfer,
            "plan-1",
            "bridge-deposit",
            &completed_attempt
        ));

        transfer.state = fin_domain::BridgeTransferState::Completed;
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
                    position_kind: fin_domain::PositionKind::Supply,
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
        let submission = fin_domain::Submission {
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
            submission: Some(submission.clone()),
            dispatch_evidence: None,
            effects: Some(fin_domain::VerifiedEffects {
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
                    position_kind: fin_domain::PositionKind::Supply,
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
                position_kind: fin_domain::PositionKind::Supply,
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
    fn cumulative_parent_scope_cannot_be_widened_by_a_plan_revision() {
        let mut prior = prior_record();
        let mut scoped = permission(json!({"route":"fixed"}), json!({}), "20");
        scoped.permission.as_mut().unwrap().cumulative_scope =
            Some(fin_domain::financial_plans::FinancialPlanCumulativeScope {
                scope_id: "SYNTHETIC-bridge-parent".into(),
                maximum_debits: vec![resource_amount("acct", "USDT", "20")],
                maximum_costs: vec![],
            });
        prior.revision.permissions = vec![scoped.clone()];
        let store = Store::in_memory().unwrap();

        validate_cumulative_revision_scopes(&prior, &[scoped.clone()], &store).unwrap();
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
            validate_cumulative_revision_scopes(&prior, &[widened], &store)
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

    fn prior_record() -> FinancialPlanAuthorizationRecord {
        let spec = financial_plans::FinancialPlanSpec {
            version: 1,
            name: "Plan".into(),
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
            approval: fin_domain::ApprovalAudit {
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
            }],
            active_attempt_id: None,
        }
    }

    struct AuthorityExpiryAdapter;
    #[async_trait::async_trait]
    impl fin_domain::Adapter for AuthorityExpiryAdapter {
        fn definitions(&self) -> Vec<fin_domain::Definition> {
            vec![fin_domain::Definition {
                name: "synthetic.write".into(),
                description: "synthetic write with a signed deadline".into(),
                kind: OperationKind::Write,
                execution_actor: fin_domain::ExecutionActor::AgentInvocable,
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
            _: Arc<dyn fin_domain::SubmissionJournal>,
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
        };
        let error = engine
            .validate_plan_candidate(
                &AuthorityExpiryAdapter,
                &revision,
                "step-1",
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
            execution_actor: fin_domain::ExecutionActor::AgentInvocable,
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
            |resource: &str, asset_ref: &str, minimum: &str| fin_domain::BridgeSettlementTarget {
                resource: ResourceId(resource.into()),
                asset: asset(asset_ref),
                minimum_atoms: Quantity::new(minimum).unwrap(),
            };
        let quote = fin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:destination", "USDC", "9883478507950693")],
            refund: vec![target("wallet:source", "ETH", "123")],
        };
        let selected = fin_domain::BridgeSettlementExpectation {
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
            |resource: &str, asset_ref: &str, minimum: &str| fin_domain::BridgeSettlementTarget {
                resource: ResourceId(resource.into()),
                asset: asset(asset_ref),
                minimum_atoms: Quantity::new(minimum).unwrap(),
            };
        let quote = fin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:destination", "USDC", "100")],
            refund: vec![target("wallet:source", "ETH", "20")],
        };
        let changed_route = fin_domain::BridgeSettlementExpectation {
            destination: vec![target("wallet:attacker", "USDC", "1")],
            refund: vec![target("wallet:source", "ETH", "1")],
        };
        let changed_asset = fin_domain::BridgeSettlementExpectation {
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
    impl fin_evm::EvmRpc for FundingGasRpc {
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
        use fin_evm::alloy::primitives::{Address, Bytes, U256};
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
            let runtime = fin_evm::EvmRuntime::new(Arc::new(FundingGasRpc { balance }), 1, None);
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
