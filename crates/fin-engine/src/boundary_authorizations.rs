use super::*;

impl Engine {
    /// Build the terminal review material for a boundary grant. This is a
    /// read-only preview: it creates neither durable authority nor a challenge.
    pub async fn preview_boundary_authorization(
        &self,
        request: BoundaryAuthorizationRequest,
    ) -> Result<Value> {
        validate_boundary_request(&request)?;
        let (adapter, kind) = self.resolve(&request.operation)?;
        if kind != OperationKind::Write {
            return Err(boundary_error(
                "Boundary authorization is available only for financial writes.",
            ));
        }
        let mut prepared = adapter.prepare(&request.operation, &request.input).await?;
        self.complete_cost_origin_prices(&mut prepared, &BTreeSet::new())
            .await?;
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        let spec = adapter.prepare_boundaries(&prepared, &request.bounds)?;
        validate_boundary_spec(&prepared, &spec)?;
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
        let mut prepared = adapter.prepare(&request.operation, &request.input).await?;
        self.complete_cost_origin_prices(&mut prepared, &BTreeSet::new())
            .await?;
        validate_prepared(&prepared, &request.operation, Utc::now())?;
        let spec = adapter.prepare_boundaries(&prepared, &request.bounds)?;
        validate_boundary_spec(&prepared, &spec)?;
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
                operation = self.reconcile_serial(operation_id.clone()).await?;
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
        let mut prepared = adapter.prepare(operation, input).await?;
        self.complete_cost_origin_prices(&mut prepared, &BTreeSet::new())
            .await?;
        validate_prepared(&prepared, operation, Utc::now())?;
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
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let previous = to_value(&authorization)?;
        authorization.attempts.push(operation_id.clone());
        authorization.active_attempt = Some(operation_id.clone());
        let created = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .create_boundary_attempt(
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
            .map_err(map_store)?;
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
                        self.commit_terminal(record.clone(), json!(null), json!([]), None)?;
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
