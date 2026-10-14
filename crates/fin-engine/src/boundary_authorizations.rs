use super::*;

impl Engine {
    /// Build the terminal review material for a boundary grant. This is a
    /// read-only preview: it creates neither durable authority nor a challenge.
    pub async fn preview_boundary_authorization(
        &self,
        request: BoundaryAuthorizationRequest,
    ) -> Result<Value> {
        validate_boundary_request(&request)?;
        self.reject_plan_only_standalone(&request.operation)?;
        let (adapter, kind) = self.resolve(&request.operation)?;
        if kind != OperationKind::Write {
            return Err(boundary_error(
                "Boundary authorization is available only for financial writes.",
            ));
        }
        fin_domain::execution_progress::emit(fin_domain::execution_progress::Stage::Preparing);
        let mut prepared = bounded_evidence(
            &request.operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare(&request.operation, &request.input),
        )
        .await?;
        self.complete_cost_origin_prices(&mut prepared, &BTreeSet::new())
            .await?;
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        let spec = bounded_evidence(
            &request.operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare_boundaries(&prepared, &request.bounds),
        )
        .await?;
        // Scope metadata resolution may perform awaited network reads. The
        // concrete preview must still be fresh before review or authorization.
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        let bridge_scope = bridge_boundary_scope(&prepared, &request.input)?;
        let prospective_debit =
            self.bridge_boundary_prospective_debit(bridge_scope.as_ref(), &prepared, &spec)?;
        validate_boundary_spec(&prepared, &spec, prospective_debit.as_ref())?;
        let replacement = if let Some(id) = &request.replaces {
            let old = self.load_boundary_record(id)?;
            if old.authorization.operation != request.operation
                || old.authorization.status == BoundaryAuthorizationStatus::Superseded
                || old.active_attempt.is_some()
            {
                return Err(boundary_error(
                    "This authorization cannot be replaced while an attempt is unresolved or after it has already been superseded.",
                ));
            }
            if old.authorization.spec.fixed != spec.fixed {
                return Err(boundaries::violation_error(
                    "A replacement cannot change the intent's fixed account, asset, or destination scope.",
                    boundaries::BoundaryViolation {
                        constraint: "fixed_scope".into(),
                        approved_bound: old.authorization.spec.fixed.clone(),
                        candidate_value: spec.fixed.clone(),
                        evidence_time: Some(prepared.observed_at.clone()),
                        reauthorization_required: true,
                        reconciliation_required: false,
                    },
                ));
            }
            Some(old)
        } else {
            None
        };
        let new_remaining_debits = remaining_debits(
            &spec.maximum_debits,
            replacement
                .as_ref()
                .map(|old| &old.authorization.spent_debits),
        )?;
        let new_remaining_costs = remaining_costs(
            &spec.maximum_costs,
            replacement
                .as_ref()
                .map(|old| &old.authorization.spent_costs),
        )?;
        let grant_binding = boundaries::authorization_binding(
            &request.operation,
            replacement
                .as_ref()
                .map(|old| old.authorization.intent_id.as_str())
                .unwrap_or("new-intent"),
            &spec,
            request.valid_for_seconds,
        );
        Ok(json!({
            "operation": request.operation,
            "bounds": request.bounds,
            "spec": spec,
            "valid_for_seconds": request.valid_for_seconds,
            "binding": grant_binding,
            "summary": replacement.as_ref().map(|old| format!(
                "{} Replacing an authorization. Already used: {}. Remaining under these new bounds: {}. Limits are cumulative totals for this same intent.",
                spec.summary,
                format_boundary_usage(&old.authorization.spent_debits, &old.authorization.spent_costs),
                format_boundary_usage(&new_remaining_debits, &new_remaining_costs),
            )).unwrap_or_else(|| spec.summary.clone()),
            "replacement_progress": replacement.as_ref().map(|old| json!({
                "spent_debits":old.authorization.spent_debits,
                "remaining_debits":new_remaining_debits,
                "spent_costs":old.authorization.spent_costs,
                "remaining_costs":new_remaining_costs,
                "cumulative_total_caps":true,
            })),
        }))
    }

    /// Obtain fresh human approval for an adapter-validated boundary grant and
    /// persist it as protected broker state.
    pub async fn authorize_boundary(
        &self,
        request: BoundaryAuthorizationRequest,
    ) -> Result<BoundaryAuthorization> {
        let _guard = self.write_queue.lock().await;
        validate_boundary_request(&request)?;
        self.reject_plan_only_standalone(&request.operation)?;
        let fingerprint = binding_hash(&json!({
            "version": 1,
            "request_id": request.request_id,
            "operation": request.operation,
            "input": request.input,
            "bounds": request.bounds,
            "valid_for_seconds": request.valid_for_seconds,
            "replaces": request.replaces,
        }))?;
        if let Some(existing) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_authorization_by_request(&request.request_id)
            .map_err(map_store)?
        {
            let mut existing: BoundaryAuthorizationRecord = from_value(existing)?;
            if existing.request_fingerprint != fingerprint {
                return Err(FinError::new(
                    "REQUEST_ID_CONFLICT",
                    "authorization",
                    "This request identity already names different boundary parameters.",
                    "Use the original request identity for retries or choose a new identity for a deliberate authorization.",
                ));
            }
            if existing.authorization.status == BoundaryAuthorizationStatus::Active
                && parse_time(&existing.authorization.expires_at)? <= Utc::now()
            {
                let before = existing.clone();
                existing.authorization.status = BoundaryAuthorizationStatus::Expired;
                if let Err(error) = self.save_boundary_record(&before, &existing) {
                    if error.code == "RESOURCE_RESERVED" {
                        existing = self.load_boundary_record(&existing.authorization.id)?;
                    } else {
                        return Err(error);
                    }
                }
            }
            return Ok(existing.authorization);
        }
        let prior_request = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            store
                .boundary_authorization_request_status(&request.request_id)
                .map_err(map_store)?
        };
        if let Some((existing_fingerprint, status)) = prior_request {
            if existing_fingerprint != fingerprint {
                return Err(FinError::new(
                    "REQUEST_ID_CONFLICT",
                    "authorization",
                    "This request identity already names different boundary parameters.",
                    "Use the original request identity for retries or choose a new identity for a deliberate authorization.",
                ));
            }
            return Err(boundary_request_state_error(&status));
        }
        let (adapter, kind) = self.resolve(&request.operation)?;
        if kind != OperationKind::Write {
            return Err(boundary_error(
                "Boundary authorization is available only for financial writes.",
            ));
        }
        fin_domain::execution_progress::emit(fin_domain::execution_progress::Stage::Preparing);
        let mut prepared = bounded_evidence(
            &request.operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare(&request.operation, &request.input),
        )
        .await?;
        self.complete_cost_origin_prices(&mut prepared, &BTreeSet::new())
            .await?;
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        let spec = bounded_evidence(
            &request.operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare_boundaries(&prepared, &request.bounds),
        )
        .await?;
        // Scope metadata resolution may perform awaited network reads. The
        // concrete preview must still be fresh before review or authorization.
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        let bridge_scope = bridge_boundary_scope(&prepared, &request.input)?;
        let prospective_debit =
            self.bridge_boundary_prospective_debit(bridge_scope.as_ref(), &prepared, &spec)?;
        validate_boundary_spec(&prepared, &spec, prospective_debit.as_ref())?;
        let bridge_authorization_seconds = if let Some((_, _, _, selected_bounds, _)) =
            &bridge_scope
        {
            let selected = bridge_authorization_duration_ceiling(selected_bounds)?;
            let requested = bridge_authorization_duration_ceiling(&request.bounds)?;
            if selected != requested {
                return Err(boundaries::violation_error(
                    "The bridge authorization duration differs from the prepared selected bounds.",
                    BoundaryViolation {
                        constraint: "authorization_duration_scope".into(),
                        approved_bound: json!(selected),
                        candidate_value: json!(requested),
                        evidence_time: Some(prepared.observed_at.clone()),
                        reauthorization_required: false,
                        reconciliation_required: false,
                    },
                ));
            }
            if let Some(maximum) = selected
                && request.valid_for_seconds > maximum
            {
                return Err(boundaries::violation_error(
                    "The requested bridge authorization duration exceeds its selected maximum.",
                    BoundaryViolation {
                        constraint: "authorization_duration_seconds".into(),
                        approved_bound: json!(maximum),
                        candidate_value: json!(request.valid_for_seconds),
                        evidence_time: Some(prepared.observed_at.clone()),
                        reauthorization_required: false,
                        reconciliation_required: false,
                    },
                ));
            }
            selected
        } else {
            None
        };
        {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            ordinary_policy::validate_boundary_candidate_policy(
                &prepared,
                "boundary-authorization-check",
                &self.options,
                &store,
                self.clock.now(),
            )?;
        }

        let replaced = if let Some(id) = &request.replaces {
            let old = self.load_boundary_record(id)?;
            if old.active_attempt.is_some()
                || old.authorization.status == BoundaryAuthorizationStatus::Superseded
                || old.authorization.operation != request.operation
            {
                return Err(boundary_error(
                    "Only an authorization without an unresolved attempt can be replaced once; superseded grants cannot branch.",
                ));
            }
            if old.authorization.spec.fixed != spec.fixed {
                return Err(boundaries::violation_error(
                    "A replacement cannot change the intent's fixed account, asset, or destination scope.",
                    boundaries::BoundaryViolation {
                        constraint: "fixed_scope".into(),
                        approved_bound: old.authorization.spec.fixed.clone(),
                        candidate_value: spec.fixed.clone(),
                        evidence_time: Some(prepared.observed_at.clone()),
                        reauthorization_required: true,
                        reconciliation_required: false,
                    },
                ));
            }
            if bridge_scope.is_some()
                && bridge_authorization_duration_ceiling(&old.authorization.bounds)?
                    != bridge_authorization_seconds
            {
                return Err(boundaries::violation_error(
                    "A bridge authorization renewal must retain the transfer's selected duration ceiling.",
                    BoundaryViolation {
                        constraint: "authorization_duration_scope".into(),
                        approved_bound: json!(bridge_authorization_duration_ceiling(
                            &old.authorization.bounds
                        )?),
                        candidate_value: json!(bridge_authorization_seconds),
                        evidence_time: Some(prepared.observed_at.clone()),
                        reauthorization_required: false,
                        reconciliation_required: false,
                    },
                ));
            }
            Some(old)
        } else {
            None
        };
        let intent_id = replaced
            .as_ref()
            .map(|old| old.authorization.intent_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let id = Uuid::new_v4().to_string();
        let binding = boundaries::authorization_binding(
            &request.operation,
            &intent_id,
            &spec,
            request.valid_for_seconds,
        );
        let binding_hash = binding_hash(&binding)?;
        let prior = replaced.as_ref().map(|old| &old.authorization);
        let new_remaining_debits =
            remaining_debits(&spec.maximum_debits, prior.map(|old| &old.spent_debits))?;
        let new_remaining_costs =
            remaining_costs(&spec.maximum_costs, prior.map(|old| &old.spent_costs))?;
        // Approval describes lasting intent, not this informational quote.
        // Dispatch will obtain and validate its own fresh candidate evidence.
        let challenge_expiry = Utc::now() + chrono::Duration::seconds(90);
        let challenge = AuthorizationChallenge {
            purpose: "financial_boundary".into(),
            operation_id: id.clone(),
            binding: binding.clone(),
            binding_hash,
            summary: if let Some(previous) = &prior {
                format!(
                    "{} Replacing this authorization. Already used: {}. Remaining under these new bounds: {}. Limits are cumulative totals for this same intent. Authorization lasts {} seconds.",
                    spec.summary,
                    format_boundary_usage(&previous.spent_debits, &previous.spent_costs),
                    format_boundary_usage(&new_remaining_debits, &new_remaining_costs),
                    request.valid_for_seconds,
                )
            } else {
                format!(
                    "{} Authorization lasts {} seconds.",
                    spec.summary, request.valid_for_seconds
                )
            },
            nonce: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            expires_at: challenge_expiry.to_rfc3339(),
        };
        let challenge_started = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .begin_boundary_authorization_request(&request.request_id, &fingerprint, &now())
            .map_err(map_store)?;
        if !challenge_started {
            let status = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .boundary_authorization_request_status(&request.request_id)
                .map_err(map_store)?
                .map(|(_, status)| status)
                .unwrap_or_else(|| "interrupted".into());
            return Err(boundary_request_state_error(&status));
        }
        fin_domain::execution_progress::emit(
            fin_domain::execution_progress::Stage::AwaitingApproval,
        );
        let approval = match self.authorizer.authorize(&challenge).await {
            Ok(approval) => approval,
            Err(error) => {
                self.store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .fail_boundary_authorization_request(&request.request_id, &fingerprint, &now())
                    .map_err(map_store)?;
                return Err(error);
            }
        };
        if let Err(error) = validate_approval(&approval, &challenge, Utc::now()) {
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .fail_boundary_authorization_request(&request.request_id, &fingerprint, &now())
                .map_err(map_store)?;
            return Err(error);
        }
        let approved_at = parse_time(&approval.approved_at)?;
        let duration = i64::try_from(request.valid_for_seconds)
            .map_err(|_| boundary_error("Authorization duration is too large."))?;
        let duration = chrono::Duration::try_seconds(duration)
            .ok_or_else(|| boundary_error("Authorization duration is too large."))?;
        let expires_at = approved_at
            .checked_add_signed(duration)
            .ok_or_else(|| boundary_error("Authorization expiry cannot be represented."))?;
        let authorization = BoundaryAuthorization {
            id: id.clone(),
            intent_id: intent_id.clone(),
            operation: request.operation.clone(),
            bounds: request.bounds.clone(),
            spec: spec.clone(),
            approved_at: approval.approved_at.clone(),
            expires_at: expires_at.to_rfc3339(),
            valid_for_seconds: request.valid_for_seconds,
            status: BoundaryAuthorizationStatus::Active,
            remaining_debits: new_remaining_debits,
            remaining_costs: new_remaining_costs,
            spent_debits: prior.map(|a| a.spent_debits.clone()).unwrap_or_default(),
            spent_costs: prior.map(|a| a.spent_costs.clone()).unwrap_or_default(),
            replaces: request.replaces.clone(),
        };
        let record = BoundaryAuthorizationRecord {
            authorization: authorization.clone(),
            approval,
            request_fingerprint: fingerprint.clone(),
            attempts: Vec::new(),
            active_attempt: None,
        };
        let timestamp = now();
        let new_record = to_value(&record)?;
        let mut store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let persisted = if let Some(old) = replaced {
            let mut updated_old = old.clone();
            updated_old.authorization.status = BoundaryAuthorizationStatus::Superseded;
            if let Some((transfer_id, step_id, step_kind, bounds, expectation)) = &bridge_scope {
                store
                    .replace_boundary_bridge_authorization(
                        &old.authorization.id,
                        &to_value(&old)?,
                        &to_value(&updated_old)?,
                        &id,
                        &request.request_id,
                        &intent_id,
                        &record.request_fingerprint,
                        &new_record,
                        &timestamp,
                        transfer_id,
                        step_id,
                        step_kind,
                        bounds,
                        expectation,
                        prospective_debit.as_ref(),
                    )
                    .map_err(map_store)?
            } else {
                store
                    .replace_boundary_authorization(
                        &old.authorization.id,
                        &to_value(&old)?,
                        &to_value(&updated_old)?,
                        &id,
                        &request.request_id,
                        &intent_id,
                        &record.request_fingerprint,
                        &new_record,
                        &timestamp,
                    )
                    .map_err(map_store)?
            }
        } else if let Some((transfer_id, step_id, step_kind, bounds, expectation)) = &bridge_scope {
            store
                .create_boundary_bridge_authorization(
                    &id,
                    &request.request_id,
                    &intent_id,
                    &record.request_fingerprint,
                    &new_record,
                    &timestamp,
                    transfer_id,
                    step_id,
                    step_kind,
                    bounds,
                    expectation,
                    prospective_debit.as_ref(),
                )
                .map_err(map_store)?
        } else {
            store
                .create_boundary_authorization(
                    &id,
                    &request.request_id,
                    &intent_id,
                    &record.request_fingerprint,
                    &new_record,
                    &timestamp,
                )
                .map_err(map_store)?
        };
        if !persisted.1 {
            let existing: BoundaryAuthorizationRecord = from_value(persisted.0)?;
            return Ok(existing.authorization);
        }
        store
            .complete_boundary_authorization_request(
                &request.request_id,
                &fingerprint,
                &id,
                &timestamp,
            )
            .map_err(map_store)?;
        Ok(authorization)
    }

    /// Inspect original attempt bindings without reconciliation or new authority.
    pub fn boundary_rpc_read_scope(&self, id: &str) -> Result<bool> {
        let record = self.load_boundary_record(id)?;
        let operation_ids = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_intent_attempts(&record.authorization.intent_id)
            .map_err(map_store)?;
        for operation_id in operation_ids {
            if self.status_rpc_read_scope(&operation_id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn boundary_status(&self, id: &str) -> Result<BoundaryStatus> {
        let _guard = self.write_queue.lock().await;
        let mut record = self.load_boundary_record(id)?;
        if record.authorization.status == BoundaryAuthorizationStatus::Active
            && parse_time(&record.authorization.expires_at)? <= Utc::now()
        {
            let before = record.clone();
            record.authorization.status = BoundaryAuthorizationStatus::Expired;
            self.save_boundary_record(&before, &record)?;
        }
        let operation_ids = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_intent_attempts(&record.authorization.intent_id)
            .map_err(map_store)?;
        let mut attempts = Vec::new();
        for operation_id in operation_ids {
            let mut operation = self.load_record(&operation_id)?;
            if matches!(
                operation.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                operation = self
                    .reconcile_observation_serial(operation_id.clone())
                    .await?;
            }
            if operation.status.terminal() {
                let mut current = self.load_boundary_record(id)?;
                if current.active_attempt.as_deref() == Some(operation.id.as_str()) {
                    let before = current.clone();
                    current.active_attempt = None;
                    account_boundary_effects(
                        &mut current.authorization,
                        operation.effects.as_ref(),
                    )?;
                    self.save_boundary_record(&before, &current)?;
                }
            }
            {
                attempts.push(BoundaryAttemptSummary {
                    id: operation.id,
                    request_id: operation.request_id,
                    operation: operation.operation,
                    input: operation.input,
                    status: operation.status,
                    submission: operation.submission,
                    effects: operation.effects,
                    error: operation.error,
                });
            }
        }
        record = self.load_boundary_record(id)?;
        if record.authorization.status == BoundaryAuthorizationStatus::Active
            && parse_time(&record.authorization.expires_at)? <= Utc::now()
        {
            let before = record.clone();
            record.authorization.status = BoundaryAuthorizationStatus::Expired;
            self.save_boundary_record(&before, &record)?;
        }
        Ok(BoundaryStatus {
            authorization: record.authorization,
            attempts,
        })
    }

    /// Revocation is a durable broker decision and never asks for approval.
    pub async fn revoke_boundary(&self, id: &str) -> Result<BoundaryStatus> {
        let mut record = self.load_boundary_record(id)?;
        if record.authorization.status == BoundaryAuthorizationStatus::Active {
            let before = record.clone();
            record.authorization.status = BoundaryAuthorizationStatus::Revoked;
            self.save_boundary_record(&before, &record)?;
        }
        let operation_ids = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .boundary_attempts(id)
            .map_err(map_store)?;
        let mut attempts = Vec::new();
        for operation_id in operation_ids {
            let operation = self.load_record(&operation_id)?;
            attempts.push(BoundaryAttemptSummary {
                id: operation.id,
                request_id: operation.request_id,
                operation: operation.operation,
                input: operation.input,
                status: operation.status,
                submission: operation.submission,
                effects: operation.effects,
                error: operation.error,
            });
        }
        Ok(BoundaryStatus {
            authorization: record.authorization,
            attempts,
        })
    }

    pub async fn preview_boundary(
        &self,
        id: &str,
        operation: &str,
        input: &Value,
    ) -> Result<BoundaryPreview> {
        self.reject_plan_only_standalone(operation)?;
        let authorization = self.require_active_boundary(id)?;
        if authorization.operation != operation {
            return Err(boundary_error(
                "Candidate operation differs from the authorized intent.",
            ));
        }
        let (adapter, kind) = self.resolve(operation)?;
        if kind != OperationKind::Write {
            return Err(boundary_error(
                "Boundary authorization is available only for financial writes.",
            ));
        }
        fin_domain::execution_progress::emit(fin_domain::execution_progress::Stage::Preparing);
        let mut prepared = bounded_evidence(
            operation,
            "preflight",
            PREFLIGHT_TIMEOUT,
            adapter.prepare(operation, input),
        )
        .await?;
        self.complete_cost_origin_prices(&mut prepared, &BTreeSet::new())
            .await?;
        validate_prepared(&prepared, operation, Utc::now())?;
        super::validate_authority_expiry_for_adapter(
            adapter.as_ref(),
            operation,
            &prepared,
            &authorization.expires_at,
        )?;
        adapter.validate_boundary_candidate(&authorization.spec, &prepared)?;
        validate_candidate_caps(&authorization, &prepared)?;
        prepared.summary = authorization.spec.summary.clone();
        let attempt_binding_hash = binding_hash(&boundaries::attempt_binding(&prepared))?;
        inject_boundary_marker(&mut prepared, &authorization, attempt_binding_hash)?;
        Ok(BoundaryPreview {
            authorization_id: id.to_owned(),
            operation: operation.to_owned(),
            input: input.clone(),
            prepared,
            spec: authorization.spec.clone(),
            remaining_debits: authorization.remaining_debits,
            remaining_costs: authorization.remaining_costs,
        })
    }

    /// Execute one concrete request under an existing authorization. The
    /// request identity and attempt link are committed before adapter dispatch.
    pub async fn execute_boundary(
        &self,
        id: &str,
        request: OperationRequest,
    ) -> Result<OperationRecord> {
        self.execute_boundary_inner(id, request).await
    }

    /// Replace a specifically identified unsubmitted candidate. The durable
    /// cancellation wins atomically against submission dispatch before this
    /// method waits for the engine's local write queue.
    pub async fn execute_boundary_replacing(
        &self,
        id: &str,
        request: OperationRequest,
        previous_attempt_id: &str,
    ) -> Result<OperationRecord> {
        let fingerprint =
            binding_hash(&json!({"version": 1, "authorization_id": id, "request": request}))?;
        let existing = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            store.by_request(&request.request_id).map_err(map_store)?
        };
        if let Some(existing) = existing {
            if existing.fingerprint != fingerprint {
                return Err(FinError::new(
                    "REQUEST_ID_CONFLICT",
                    "validation",
                    "This request identity already names different parameters.",
                    "Inspect the original boundary attempt and use a fresh identity only for a deliberate new candidate.",
                ));
            }
            let belongs_to_authorization = {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                store
                    .boundary_attempts(id)
                    .map_err(map_store)?
                    .contains(&existing.operation_id)
            };
            if !belongs_to_authorization {
                return Err(boundary_error(
                    "This request identity belongs to a different authorization.",
                ));
            }
            let original: OperationRecord = from_value(existing.record)?;
            return if matches!(
                original.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                self.reconcile_serial(original.id.clone()).await
            } else {
                Ok(original)
            };
        }
        let mut authorization = self.load_boundary_record(id)?;
        if authorization.authorization.status != BoundaryAuthorizationStatus::Active
            || parse_time(&authorization.authorization.expires_at)? <= Utc::now()
        {
            return Err(boundary_error(
                "Only a live authorization can supersede an unsubmitted candidate.",
            ));
        }
        if authorization.active_attempt.as_deref() == Some(previous_attempt_id) {
            let before = authorization.clone();
            authorization.active_attempt = None;
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .cancel_boundary_candidate(
                    id,
                    &to_value(&before)?,
                    &to_value(&authorization)?,
                    previous_attempt_id,
                    &now(),
                )
                .map_err(map_store)?;
        } else if authorization.active_attempt.is_some() {
            return Err(FinError::new(
                "BOUNDARY_ATTEMPT_UNRESOLVED",
                "authorization",
                "The active attempt differs from the candidate being replaced.",
                "Reconcile the active attempt and name its exact identity before replacement.",
            ));
        } else {
            let old = self.load_record(previous_attempt_id)?;
            if old.status != Status::Cancelled || old.submission.is_some() {
                return Err(boundary_error(
                    "The specified prior attempt is not a previously superseded unsubmitted candidate.",
                ));
            }
        }
        self.execute_boundary_inner(id, request).await
    }

    async fn execute_boundary_inner(
        &self,
        id: &str,
        request: OperationRequest,
    ) -> Result<OperationRecord> {
        let _guard = self.write_queue.lock().await;
        let fingerprint =
            binding_hash(&json!({"version": 1, "authorization_id": id, "request": request}))?;
        let existing = {
            let store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            store.by_request(&request.request_id).map_err(map_store)?
        };
        if let Some(existing) = existing {
            if existing.fingerprint != fingerprint {
                return Err(FinError::new(
                    "REQUEST_ID_CONFLICT",
                    "validation",
                    "This request identity already names different parameters.",
                    "Inspect the original boundary attempt and use a fresh identity only for a deliberate new candidate.",
                ));
            }
            let linked = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .boundary_attempts(id)
                .map_err(map_store)?
                .contains(&existing.operation_id);
            if !linked {
                return Err(FinError::new(
                    "REQUEST_ID_CONFLICT",
                    "authorization",
                    "This request identity belongs to a different authorization.",
                    "Use the original authorization for recovery or a fresh request identity.",
                ));
            }
            let original: OperationRecord = from_value(existing.record)?;
            return if matches!(
                original.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                self.reconcile_serial(original.id.clone()).await
            } else {
                Ok(original)
            };
        }
        if request.run_id.is_some() {
            return Err(boundary_error(
                "A boundary authorization cannot be used for a historical scoped run.",
            ));
        }
        self.reject_plan_only_standalone(&request.operation)?;
        let mut authorization = self.load_boundary_record(id)?;
        if authorization.authorization.operation != request.operation {
            return Err(boundary_error(
                "Candidate operation differs from the authorized intent.",
            ));
        }
        if authorization.authorization.status != BoundaryAuthorizationStatus::Active
            || parse_time(&authorization.authorization.expires_at)? <= Utc::now()
            || authorization.active_attempt.is_some()
        {
            return Err(boundary_error(
                "Authorization is inactive or another attempt remains unresolved.",
            ));
        }
        let (adapter, kind) = self.resolve(&request.operation)?;
        if kind != OperationKind::Write {
            return Err(boundary_error(
                "Boundary authorization is available only for financial writes.",
            ));
        }
        let timestamp = now();
        let operation_id = Uuid::new_v4().to_string();
        let initial = OperationRecord {
            id: operation_id.clone(),
            request_id: request.request_id.clone(),
            fingerprint: fingerprint.clone(),
            fingerprint_version: 1,
            operation: request.operation.clone(),
            input: request.input.clone(),
            status: Status::AwaitingApproval,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
            preview: None,
            approval: None,
            submission: None,
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let previous = to_value(&authorization)?;
        authorization.attempts.push(operation_id.clone());
        authorization.active_attempt = Some(operation_id.clone());
        let bridge_step = if request.operation == "bridge.transfer" {
            bridge_step_claim(&request, Some(id), &operation_id, self)?
        } else {
            None
        };
        let created = {
            let mut store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            if let Some(step) = bridge_step.as_ref() {
                store.create_boundary_bridge_attempt(
                    id,
                    &previous,
                    &to_value(&authorization)?,
                    &operation_id,
                    &request.request_id,
                    &fingerprint,
                    &request.operation,
                    &to_value(&initial)?,
                    &timestamp,
                    step,
                )
            } else {
                store.create_boundary_attempt(
                    id,
                    &previous,
                    &to_value(&authorization)?,
                    &operation_id,
                    &request.request_id,
                    &fingerprint,
                    &request.operation,
                    &to_value(&initial)?,
                    &timestamp,
                )
            }
            .map_err(map_store)?
        };
        if !created.created {
            let existing: OperationRecord = from_value(created.record)?;
            return if matches!(
                existing.status,
                Status::Submitted | Status::Pending | Status::Unknown
            ) {
                self.reconcile_serial(existing.id.clone()).await
            } else {
                Ok(existing)
            };
        }
        let mut record = initial;
        let result = self
            .execute_new(
                adapter,
                &request,
                &mut record,
                Some(authorization.clone()),
                None,
                None,
                super::ExecutionRoute::Financial,
            )
            .await;
        let result_record = match result {
            Ok(done) => done,
            Err(error) => {
                if !record.status.terminal() {
                    record.status = if record.submission.is_some() {
                        Status::Unknown
                    } else {
                        status_for_error(&error)
                    };
                    record.error = Some(error);
                    record.updated_at = now();
                    if record.status.terminal() {
                        self.commit_terminal(record.clone(), json!(null), json!([]), None, None)?;
                    } else {
                        self.update_record(&record)?;
                    }
                }
                record
            }
        };
        if result_record.status.terminal() {
            let mut current = self.load_boundary_record(id)?;
            if current.active_attempt.as_deref() == Some(result_record.id.as_str()) {
                let before = current.clone();
                current.active_attempt = None;
                account_boundary_effects(
                    &mut current.authorization,
                    result_record.effects.as_ref(),
                )?;
                self.save_boundary_record(&before, &current)?;
            }
        }
        Ok(result_record)
    }

    /// A bridge allowance step may authorize the one source-principal debit
    /// which a later declared Deposit step will execute. The adapter's marker
    /// is only accepted when it exactly matches the durable neutral plan, and
    /// only if the Deposit is downstream of this still-unsubmitted step.
    pub(super) fn bridge_boundary_prospective_debit(
        &self,
        bridge_scope: Option<&BridgeBoundaryScope>,
        prepared: &PreparedOperation,
        spec: &BoundarySpec,
    ) -> Result<Option<fin_domain::BridgeProspectiveDebit>> {
        let missing = spec
            .maximum_debits
            .iter()
            .filter(|limit| {
                !prepared.intent.debits.iter().any(|debit| {
                    debit.resource == limit.resource && debit.amount.asset == limit.amount.asset
                })
            })
            .collect::<Vec<_>>();
        let Some((transfer_id, step_id, step_kind, _, _)) = bridge_scope else {
            return Ok(None);
        };
        if !missing.is_empty()
            && (!matches!(
                step_kind,
                fin_domain::BridgeStepKind::Approval | fin_domain::BridgeStepKind::ResetAllowance
            ) || missing.len() != 1)
        {
            return Ok(None);
        }
        let marker_value = prepared
            .binding
            .get(fin_domain::BRIDGE_STEP_METADATA_KEY)
            .and_then(|marker| marker.get("prospective_debit"));
        let Some(marker_value) = marker_value else {
            return if missing.is_empty() {
                Ok(None)
            } else {
                Err(boundary_error(
                    "An allowance boundary with future principal must carry its typed declared-deposit scope.",
                ))
            };
        };
        let marker: fin_domain::BridgeProspectiveDebit =
            serde_json::from_value(marker_value.clone())
                .map_err(|_| boundary_error("The bridge prospective debit scope is malformed."))?;
        if marker.deposit_step_id.trim().is_empty()
            || marker.maximum.atoms.is_zero()
            || !prepared.intent.locks.contains(&marker.resource)
        {
            return Err(boundary_error(
                "The prospective source debit does not identify the locked wallet and a positive finite amount.",
            ));
        }
        let store = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?;
        let transfer = store
            .bridge_transfer(transfer_id)
            .map_err(map_store)?
            .ok_or_else(|| boundary_error("The bridge transfer is not in the durable plan."))?;
        if transfer.prospective_debit.as_ref() != Some(&marker) {
            return Err(boundary_error(
                "The prepared prospective debit differs from the durable bridge plan.",
            ));
        }
        let current = transfer
            .steps
            .iter()
            .find(|step| step.step_id == *step_id)
            .ok_or_else(|| boundary_error("The current bridge step is not declared."))?;
        let deposit = transfer
            .steps
            .iter()
            .find(|step| step.step_id == marker.deposit_step_id)
            .ok_or_else(|| boundary_error("The prospective Deposit step is not declared."))?;
        let kind_matches_deposit = match step_kind {
            fin_domain::BridgeStepKind::Approval | fin_domain::BridgeStepKind::ResetAllowance => {
                deposit.step_id != *step_id
                    && bridge_step_is_downstream(&transfer.steps, &deposit.step_id, step_id)
            }
            fin_domain::BridgeStepKind::Deposit => deposit.step_id == *step_id,
            fin_domain::BridgeStepKind::Prove
            | fin_domain::BridgeStepKind::Finalize
            | fin_domain::BridgeStepKind::Mint => false,
        };
        if current.kind != *step_kind
            || current.operation_id.is_some()
            || current.state != fin_domain::BridgeStepState::Unsubmitted
            || deposit.kind != fin_domain::BridgeStepKind::Deposit
            || deposit.operation_id.is_some()
            || deposit.state != fin_domain::BridgeStepState::Unsubmitted
            || !kind_matches_deposit
        {
            return Err(boundary_error(
                "The prospective source debit is not tied to a ready allowance step and its declared downstream Deposit.",
            ));
        }
        let expected = fin_domain::ResourceAmount {
            resource: marker.resource.clone(),
            amount: marker.maximum.clone(),
        };
        if !missing.is_empty()
            && (missing[0].resource != expected.resource
                || missing[0].amount.asset != expected.amount.asset
                || missing[0].amount.atoms != expected.amount.atoms)
        {
            return Err(boundary_error(
                "The authorization's future principal cap differs from the exact durable Deposit scope.",
            ));
        }
        Ok(Some(marker))
    }
}

pub(super) type BridgeBoundaryScope = (
    String,
    String,
    fin_domain::BridgeStepKind,
    Value,
    fin_domain::BridgeSettlementExpectation,
);

fn bridge_step_is_downstream(
    steps: &[fin_domain::BridgeStepLink],
    dependent_step_id: &str,
    ancestor_step_id: &str,
) -> bool {
    if dependent_step_id == ancestor_step_id {
        return true;
    }
    let mut pending = steps
        .iter()
        .find(|step| step.step_id == dependent_step_id)
        .map(|step| step.depends_on.clone())
        .unwrap_or_default();
    let mut visited = std::collections::BTreeSet::new();
    while let Some(step_id) = pending.pop() {
        if step_id == ancestor_step_id {
            return true;
        }
        if visited.insert(step_id.clone())
            && let Some(step) = steps.iter().find(|step| step.step_id == step_id)
        {
            pending.extend(step.depends_on.iter().cloned());
        }
    }
    false
}

pub(super) fn bridge_boundary_scope(
    prepared: &PreparedOperation,
    input: &Value,
) -> Result<Option<BridgeBoundaryScope>> {
    let Some(marker) = prepared.binding.get(fin_domain::BRIDGE_STEP_METADATA_KEY) else {
        return Ok(None);
    };
    let text = |field: &str| {
        marker
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| boundary_error("Finite bridge grant marker is incomplete."))
    };
    let transfer_id = text("transfer_id")?;
    let step_id = text("step_id")?;
    let step_kind: fin_domain::BridgeStepKind = serde_json::from_value(
        marker
            .get("kind")
            .cloned()
            .ok_or_else(|| boundary_error("Finite bridge grant marker lacks its step kind."))?,
    )
    .map_err(|_| boundary_error("Finite bridge grant marker has an unknown step kind."))?;
    if super::bridge_input_field(input, "transfer_id", "transferId")?.and_then(Value::as_str)
        != Some(transfer_id.as_str())
        || super::bridge_input_field(input, "step_id", "stepId")?.and_then(Value::as_str)
            != Some(step_id.as_str())
        || super::bridge_input_field(input, "step_kind", "stepKind")?
            .cloned()
            .and_then(|value| serde_json::from_value::<fin_domain::BridgeStepKind>(value).ok())
            != Some(step_kind.clone())
    {
        return Err(boundary_error(
            "Finite bridge grant step differs from its typed public request.",
        ));
    }
    let bounds = marker
        .get("bounds")
        .cloned()
        .ok_or_else(|| boundary_error("Finite bridge grant marker lacks selected bounds."))?;
    let expectation = serde_json::from_value(
        marker
            .get("settlement_expectation")
            .cloned()
            .ok_or_else(|| boundary_error("Finite bridge grant marker lacks settlement scope."))?,
    )
    .map_err(|_| boundary_error("Finite bridge grant settlement scope is malformed."))?;
    Ok(Some((transfer_id, step_id, step_kind, bounds, expectation)))
}

/// Read only the neutral bridge bounds field needed by Engine's authorization
/// contract. BridgeBounds currently serializes this member in snake_case; the
/// camelCase alias is accepted for wire-compatible callers, with conflicts
/// rejected by the shared field reader.
fn bridge_authorization_duration_ceiling(bounds: &Value) -> Result<Option<u64>> {
    let value = super::bridge_input_field(bounds, "authorization_seconds", "authorizationSeconds")?;
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let seconds = value.as_u64().filter(|seconds| *seconds > 0).ok_or_else(|| {
        boundaries::violation_error(
            "The selected bridge authorization duration must be a positive integer number of seconds.",
            BoundaryViolation {
                constraint: "authorization_duration_seconds".into(),
                approved_bound: json!({"minimum":1,"type":"positive_integer"}),
                candidate_value: value.clone(),
                evidence_time: None,
                reauthorization_required: false,
                reconciliation_required: false,
            },
        )
    })?;
    Ok(Some(seconds))
}

pub(super) fn bridge_step_claim(
    request: &OperationRequest,
    grant_id: Option<&str>,
    operation_id: &str,
    engine: &Engine,
) -> Result<Option<fin_domain::BridgeStepLink>> {
    let input = &request.input;
    let transfer_id_value = super::bridge_input_field(input, "transfer_id", "transferId")?;
    let step_id_value = super::bridge_input_field(input, "step_id", "stepId")?;
    let step_kind_value = super::bridge_input_field(input, "step_kind", "stepKind")?;
    let present = [transfer_id_value, step_id_value, step_kind_value]
        .iter()
        .filter(|value| value.is_some())
        .count();
    if present == 0 {
        // Preserve legacy exact operation grants. Finite bridge transfers use
        // the complete typed triplet and take the durable claim path below.
        return Ok(None);
    }
    if present != 3 {
        return Err(boundary_error(
            "A finite bridge-step attempt requires transfer_id, step_id and step_kind together.",
        ));
    }
    let transfer_id = transfer_id_value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| boundary_error("The bridge transfer ID must be a nonempty string."))?;
    let step_id = step_id_value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| boundary_error("The bridge step ID must be a nonempty string."))?;
    let kind: fin_domain::BridgeStepKind =
        serde_json::from_value(step_kind_value.cloned().unwrap_or(Value::Null)).map_err(|_| {
            boundary_error("The bridge step kind is not one of the declared finite kinds.")
        })?;
    let store = engine
        .store
        .lock()
        .map_err(|_| internal("store lock poisoned"))?;
    let transfer = store
        .bridge_transfer(transfer_id)
        .map_err(map_store)?
        .ok_or_else(|| boundary_error("The bridge transfer is not present in the durable plan."))?;
    if transfer.grant_id.as_deref() != grant_id || transfer.plan_scope.is_some() {
        return Err(boundary_error(
            "The bridge transfer is not bound to this finite authorization.",
        ));
    }
    let planned = transfer
        .steps
        .iter()
        .find(|step| step.step_id == step_id)
        .ok_or_else(|| boundary_error("The bridge step is not declared in this transfer."))?;
    if planned.kind != kind
        || planned.grant_id.as_deref() != grant_id
        || planned.operation_id.is_some()
        || planned.state != fin_domain::BridgeStepState::Unsubmitted
        || planned.depends_on.iter().any(|dependency| {
            !transfer.steps.iter().any(|step| {
                step.step_id == *dependency && step.state == fin_domain::BridgeStepState::Settled
            })
        })
    {
        return Err(boundary_error(
            "The bridge step is outside the approved plan, already claimed, or has unsettled dependencies.",
        ));
    }
    let mut claimed = planned.clone();
    claimed.operation_id = Some(operation_id.to_owned());
    claimed.state = fin_domain::BridgeStepState::Submitted;
    Ok(Some(claimed))
}

fn boundary_request_state_error(status: &str) -> FinError {
    match status {
        "awaiting_approval" => FinError::new(
            "BOUNDARY_AUTHORIZATION_IN_PROGRESS",
            "authorization",
            "This boundary request already has an approval challenge in progress.",
            "Wait for the original challenge to finish; it will not prompt again for this request identity.",
        ),
        "declined" | "interrupted" => FinError::new(
            "BOUNDARY_AUTHORIZATION_RETRY_REQUIRED",
            "authorization",
            "The original boundary approval challenge did not complete.",
            "Start a deliberate fresh request with a new request identity to request another Touch ID challenge.",
        ),
        _ => FinError::new(
            "BOUNDARY_AUTHORIZATION_RECOVERY_REQUIRED",
            "authorization",
            "The boundary request is already terminal but has no recoverable authorization record.",
            "Inspect protected authorization state; do not replay this request or create another approval automatically.",
        ),
    }
}
