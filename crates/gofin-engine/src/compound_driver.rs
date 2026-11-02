//! Native fee admission for explicitly continued compound actions.
use super::*;

fn mark_unsubmitted_stage(record: &mut OperationRecord) {
    let evidence = record
        .last_reconciliation_evidence
        .get_or_insert_with(|| json!({}));
    if !evidence.is_object() {
        *evidence = json!({"previous":evidence.clone()});
    }
    evidence["engine_predispatch_failure"] = json!(true);
}

pub(super) fn validate_parent_stage(
    adapter: &dyn Adapter,
    parent: &PreparedOperation,
    stage_id: &str,
    candidate: &PreparedOperation,
) -> Result<()> {
    if adapter.is_compound_action(parent) {
        adapter.validate_stage_candidate(parent, stage_id, candidate)
    } else if stage_id == "standalone" {
        adapter.validate_approved_candidate(parent, candidate)
    } else {
        Err(GoFinError::validation(
            "Invalid standalone attempt identity.",
        ))
    }
}

/// Include observed costs for final stages and full maxima for unresolved
/// submissions. Missing proof cannot create room under the parent's cap.
pub(super) fn validate_stage_fee_caps(
    parent: &PreparedOperation,
    candidate: &PreparedOperation,
    prior: &[OperationRecord],
) -> Result<()> {
    let caps = aggregate_cost_bounds(&parent.intent.maximum_costs)?;
    let mut total = aggregate_cost_bounds(&candidate.intent.maximum_costs)?;
    for stage in prior {
        let costs = match stage.status {
            Status::Completed | Status::Failed => {
                let unknown = stage.effects.as_ref().is_none_or(|effects| {
                    effects
                        .evidence
                        .get("native_fee_observed")
                        .and_then(Value::as_bool)
                        == Some(false)
                });
                let mut actual = stage
                    .effects
                    .as_ref()
                    .map(|effects| aggregate_costs(&effects.effects))
                    .transpose()?
                    .unwrap_or_default();
                if unknown {
                    let preview = stage.preview.as_ref().ok_or_else(|| {
                        GoFinError::validation(
                            "A previous stage lacks its approved native fee bounds.",
                        )
                    })?;
                    for (key, maximum) in aggregate_cost_bounds(&preview.intent.maximum_costs)? {
                        let amount = actual.entry(key).or_default();
                        if *amount < maximum {
                            *amount = maximum;
                        }
                    }
                }
                actual
            }
            Status::Submitted | Status::Pending | Status::Unknown => {
                let preview = stage.preview.as_ref().ok_or_else(|| {
                    GoFinError::new(
                        "STAGE_CANDIDATE_MISSING",
                        "preflight",
                        "An admitted stage has no durable candidate.",
                        "Observe the original stage before continuing.",
                    )
                })?;
                aggregate_cost_bounds(&preview.intent.maximum_costs)?
            }
            Status::Rejected | Status::Expired | Status::Cancelled
                if stage.submission.is_none() =>
            {
                continue;
            }
            _ => {
                return Err(GoFinError::validation(
                    "A prior stage is not ready for continuation.",
                ));
            }
        };
        for (key, amount) in costs {
            *total.entry(key).or_default() += amount;
        }
    }
    if total
        .iter()
        .any(|(key, amount)| caps.get(key).is_none_or(|cap| amount > cap))
    {
        return Err(GoFinError::new(
            "STAGE_FEE_CAP_EXCEEDED",
            "preflight",
            "The candidate and prior stage costs exceed the approved native fee cap.",
            "Nothing was submitted; obtain approval for different terms.",
        ));
    }
    Ok(())
}

impl Engine {
    pub(super) fn current_admission_action(
        &self,
        action: &OperationRecord,
    ) -> Result<OperationRecord> {
        let current = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .current_action_authority(&action.id)
            .map_err(map_store)?;
        let Some((id, authority)) = current else {
            return Ok(action.clone());
        };
        let prepared: PreparedOperation = from_value(
            authority
                .get("prepared")
                .cloned()
                .ok_or_else(|| internal("approved intent missing"))?,
        )?;
        let audit = from_value(
            authority
                .get("audit")
                .cloned()
                .ok_or_else(|| internal("approval audit missing"))?,
        )?;
        let mut derived = action.clone();
        derived.input = prepared.input.clone();
        derived.preview = Some(prepared);
        derived.approval = Some(audit);
        let audit = derived
            .approval
            .as_ref()
            .ok_or_else(|| internal("approval missing"))?;
        if audit.challenge_nonce != id
            || binding_hash(&gofin_domain::native_authorization_binding(
                derived
                    .preview
                    .as_ref()
                    .ok_or_else(|| internal("approved preview missing"))?,
            ))? != audit.binding_hash
        {
            return Err(internal("current approval binding mismatch"));
        }
        derived.authorization_source = audit.source.clone();
        Ok(derived)
    }

    fn approved_action(&self, action: &OperationRecord) -> Result<PreparedOperation> {
        let audit = action
            .approval
            .as_ref()
            .ok_or_else(|| internal("approval missing"))?;
        let authority = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .action_authority(&action.id, &audit.challenge_nonce)
            .map_err(map_store)?
            .ok_or_else(|| internal("durable action authority missing"))?;
        let prepared: PreparedOperation = serde_json::from_value(
            authority
                .get("prepared")
                .cloned()
                .ok_or_else(|| internal("approved intent missing"))?,
        )
        .map_err(|_| internal("approved intent malformed"))?;
        if binding_hash(&gofin_domain::native_authorization_binding(&prepared))?
            != audit.binding_hash
        {
            return Err(GoFinError::validation(
                "Durable approved intent does not match its audit.",
            ));
        }
        Ok(prepared)
    }

    /// Review explicit revised native terms for the unfinished part of an
    /// existing logical action. A token can cause at most one review.
    pub async fn reauthorize_operation(
        &self,
        id: &str,
        token: &str,
        input: Value,
    ) -> Result<Value> {
        if !gofin_domain::execution_progress::invocation_context_present() {
            return Err(GoFinError::validation(
                "Reauthorization requires an active foreground invocation.",
            ));
        }
        let lease = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .try_acquire_action_lease(id)
            .map_err(map_store)?;
        let input_hash = binding_hash(&input)?;
        let token_row = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .action_continuation(token)
            .map_err(map_store)?
            .ok_or_else(|| GoFinError::validation("Unknown reauthorization token."))?;
        if token_row.action_id != id {
            return Err(GoFinError::validation(
                "Reauthorization token belongs to a different action.",
            ));
        }
        let existing = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .reauthorization_claim(token)
            .map_err(map_store)?;
        if let Some(existing) = existing {
            if existing.input_hash != input_hash {
                return Err(GoFinError::validation(
                    "This reauthorization token was used with different input.",
                ));
            }
            if let Some(result) = existing.result.as_ref()
                && result.get("reauthorization").and_then(Value::as_str) != Some("abandoned")
            {
                let mut view = self.operation_view(&self.load_record(id)?)?;
                view["reauthorization"] = result["reauthorization"].clone();
                return Ok(view);
            }
            let (claim, next) = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .abandon_reauthorization(&lease, token, &input_hash)
                .map_err(map_store)?;
            let mut result = claim
                .result
                .unwrap_or_else(|| json!({"reauthorization":"interrupted"}));
            if let Some(next) = next {
                result["next_action"] = json!({"code":"reauthorize","operation_id":id,
                "command":"operations reauthorize","continuation_token":next.token,"arguments":{"operation_id":id,"token":next.token,"input_required":true}});
            }
            let mut view = self.operation_view(&self.load_record(id)?)?;
            view["reauthorization"] = result["reauthorization"].clone();
            if result.get("next_action").is_some() {
                view["next_action"] = result["next_action"].clone();
            }
            return Ok(view);
        }
        let action = self.load_record(id)?;
        if self
            .operation_view(&action)?
            .pointer("/outcome/phase")
            .and_then(Value::as_str)
            == Some("done")
        {
            return Err(GoFinError::validation(
                "A completed action cannot be reauthorized; use a new action.",
            ));
        }
        if token_row.stage_token.is_some() || token_row.result.is_some() {
            return Err(GoFinError::validation(
                "This token cannot start a new review.",
            ));
        }
        let attempts = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .list_stage_attempts(id)
            .map_err(map_store)?;
        if attempts
            .iter()
            .any(|s| matches!(s.state.as_str(), "claimed" | "admitted" | "uncertain"))
        {
            return Err(GoFinError::new(
                "ACTION_OUTCOME_UNRESOLVED",
                "authorization",
                "An admitted or claimed stage is unresolved.",
                "Observe the original stage before reviewing a new attempt.",
            ));
        }
        let mut prior = Vec::new();
        for stage in attempts
            .iter()
            .filter(|s| matches!(s.state.as_str(), "completed" | "failed"))
        {
            let Some(child_id) = stage.operation_id.as_deref() else {
                return Err(internal("terminal stage identity missing"));
            };
            prior.push(self.load_record(child_id)?);
        }
        let (adapter, definition) = self.resolve_definition(&action.operation)?;
        if definition.kind != OperationKind::Write {
            return Err(GoFinError::validation(
                "Only a financial action can be reauthorized.",
            ));
        }
        let original = self.approved_action(&action)?;
        let context = PreparationContext::default();
        let candidate = if adapter.is_compound_action(&original) {
            bounded_evidence(
                &action.operation,
                "remaining action preparation",
                PREFLIGHT_TIMEOUT,
                adapter.prepare_remaining_action(&action, &input, &prior, &context),
            )
            .await?
        } else {
            bounded_evidence(
                &action.operation,
                "reauthorization preparation",
                PREFLIGHT_TIMEOUT,
                adapter.prepare_with_context(&action.operation, &input, &context),
            )
            .await?
        };
        validate_prepared(&candidate, &action.operation, self.clock.now())?;
        let mut no_new_costs = candidate.clone();
        no_new_costs.intent.maximum_costs.clear();
        validate_stage_fee_caps(&candidate, &no_new_costs, &prior)?;
        let deadline = if adapter.is_compound_action(&candidate) {
            let resolved = candidate.action_deadline.as_ref().ok_or_else(|| {
                GoFinError::validation("A compound action needs an explicit deadline.")
            })?;
            DateTime::<Utc>::from_timestamp(
                i64::try_from(resolved.deadline_unix_seconds)
                    .map_err(|_| GoFinError::validation("Invalid deadline."))?,
                0,
            )
            .ok_or_else(|| GoFinError::validation("Invalid deadline."))?
        } else {
            self.clock.now() + chrono::Duration::seconds(90)
        };
        gofin_domain::execution_progress::ensure_invocation_active()?;
        let claim = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .claim_reauthorization(&lease, token, &input_hash, &to_value(&candidate)?)
            .map_err(map_store)?;
        if !claim.created {
            return Ok(claim
                .result
                .unwrap_or_else(|| json!({"reauthorization":"pending","operation_id":id})));
        }
        let mut review = if self.authorizer.requires_financial_review() {
            Some(gofin_domain::approval_review::approval_review(&candidate)?)
        } else {
            None
        };
        for old in &prior {
            if let (Some(effects), Some(review)) = (&old.effects, &mut review) {
                for effect in &effects.effects {
                    if let FinancialEffect::Cost { amount, .. } = effect {
                        review.terms.push(gofin_domain::approval_review::ApprovalTerm { label:"Previously incurred cost".into(),
                            value:format!("{} atoms {} on {}", amount.atoms, amount.asset.reference, amount.asset.network),
                            enforcement:gofin_domain::approval_review::ApprovalTermEnforcement::AfterExecution });
                    }
                }
            }
        }
        if let Some(review) = &mut review {
            for bound in &candidate.intent.maximum_costs {
                let mut spent = num_bigint::BigUint::default();
                for old in &prior {
                    if let Some(effects) = &old.effects {
                        for effect in &effects.effects {
                            if let FinancialEffect::Cost {
                                location,
                                amount,
                                treatment,
                                ..
                            } = effect
                                && *location == bound.location
                                && amount.asset == bound.amount.asset
                                && *treatment == bound.treatment
                            {
                                spent += amount.atoms.integer();
                            }
                        }
                    }
                }
                let maximum = bound.amount.atoms.integer();
                let remaining = if maximum >= spent {
                    maximum - spent
                } else {
                    return Err(GoFinError::validation(
                        "Prior costs exceed the revised fee cap.",
                    ));
                };
                review
                    .terms
                    .push(gofin_domain::approval_review::ApprovalTerm {
                        label: "Remaining fee allowance".into(),
                        value: format!(
                            "{} atoms {} on {}",
                            remaining, bound.amount.asset.reference, bound.amount.asset.network
                        ),
                        enforcement:
                            gofin_domain::approval_review::ApprovalTermEnforcement::Enforced,
                    });
            }
        }
        let binding = gofin_domain::native_authorization_binding(&candidate);
        let challenge = AuthorizationChallenge {
            review,
            purpose: "financial_write".into(),
            operation_id: id.into(),
            binding_hash: binding_hash(&binding)?,
            binding,
            summary: candidate.summary.clone(),
            nonce: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            expires_at: deadline.to_rfc3339(),
        };
        gofin_domain::execution_progress::emit(
            gofin_domain::execution_progress::Stage::AwaitingApproval,
        );
        let mut audit = self.authorizer.authorize(&challenge).await?;
        let approved_now = self.clock.now();
        validate_approval(&audit, &challenge, approved_now)?;
        if !adapter.is_compound_action(&candidate) {
            promote_standalone_touch_id_approval(&mut audit, &challenge, approved_now)?;
        }
        validate_financial_approval_source(&audit)?;
        gofin_domain::execution_progress::ensure_invocation_active()?;
        let mut updated = action.clone();
        updated.input = input;
        updated.preview = Some(candidate.clone());
        updated.approval = Some(audit.clone());
        updated.authorization_source = audit.source.clone();
        updated.updated_at = self.clock.now().to_rfc3339();
        if action.operation == "bridge.execute"
            && action.error.as_ref().is_some_and(|error| {
                matches!(
                    error.code.as_str(),
                    "FEE_LIMIT" | "BRIDGE_CUMULATIVE_FEE_LIMIT"
                )
            })
        {
            updated.error = None;
        }
        if action.submission.is_some() {
            updated = action.clone();
        }
        let authority = json!({"audit":audit,"intent":gofin_domain::native_authorization_binding(&candidate),"prepared":candidate});
        let result = json!({"reauthorization":"approved","operation_id":id,"next_action":{"code":"status","operation_id":id,"command":"status","arguments":{"operation_id":id}}});
        let (_, next) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .complete_reauthorization(
                &lease,
                token,
                &input_hash,
                &challenge.nonce,
                &authority,
                &audit.expires_at,
                &audit.expires_at,
                &to_value(&action)?,
                &to_value(&updated)?,
                &result,
            )
            .map_err(map_store)?;
        let mut advance_error = None;
        if let Some(next) = next
            && gofin_domain::execution_progress::invocation_active()
            && let Err(error) = Box::pin(self.continue_with_lease(id, &next.token, &lease)).await
        {
            advance_error = Some(error);
        }
        let mut view = self.operation_view(&self.load_record(id)?)?;
        view["reauthorization"] = json!("approved");
        if let Some(error) = advance_error {
            view["continuation_error"] =
                json!({"code":error.code,"stage":error.stage,"message":error.message});
        }
        Ok(view)
    }

    /// Consume an action-level continuation exactly once. A repeated token
    /// observes its immutable result and never begins another stage.
    pub async fn continue_operation(&self, id: &str, token: &str) -> Result<Value> {
        if !gofin_domain::execution_progress::invocation_context_present() {
            return Err(GoFinError::validation(
                "Continuation requires an active foreground invocation.",
            ));
        }
        let lease = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .try_acquire_action_lease(id)
            .map_err(map_store)?;
        let preapproval = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .preapproval_continuation(token)
            .map_err(map_store)?;
        if let Some(preapproval) = preapproval {
            let mut drop_guard = gofin_domain::execution_progress::invocation_drop_guard()
                .ok_or_else(|| internal("foreground invocation missing"))?;
            let engine = self.clone();
            let action_id = id.to_owned();
            let token = token.to_owned();
            let work = async move {
                let (view, advance) =
                    Box::pin(engine.continue_preapproval(&action_id, &token, &lease, preapproval))
                        .await?;
                if !advance {
                    return Ok(view);
                }
                gofin_domain::execution_progress::ensure_invocation_active()?;
                let next = engine
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .latest_action_continuation(&action_id)
                    .map_err(map_store)?
                    .ok_or_else(|| internal("compound continuation missing"))?;
                let _ =
                    Box::pin(engine.continue_with_lease(&action_id, &next.token, &lease)).await?;
                engine.operation_view(&engine.load_record(&action_id)?)
            };
            let result = tokio::spawn(gofin_domain::execution_progress::inherit_current(work))
                .await
                .map_err(|_| internal("preapproval continuation task failed"))?;
            drop_guard.disarm();
            return result;
        }
        self.continue_with_lease(id, token, &lease).await
    }

    fn finish_interrupted_preapproval(
        &self,
        id: &str,
        token: &str,
        lease: &gofin_store::execution_admission::StageInvocationLease,
    ) -> Result<Value> {
        let mut record = self.load_record(id)?;
        let expected = record.clone();
        record.status = Status::Failed;
        record.error = Some(GoFinError::new(
            "PREAPPROVAL_INTERRUPTED",
            "approval",
            "The previous preparation or review was interrupted before authority was persisted.",
            "Continue the original action with the new token to request review again.",
        ));
        record.updated_at = self.clock.now().to_rfc3339();
        let next = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .preapproval_successor(token)
            .map_err(map_store)?
            .ok_or_else(|| internal("preapproval successor missing"))?;
        let mut view = self.operation_view(&record)?;
        view["outcome"]["next_action"] = json!({"code":"continue","message":"Continue this original action to prepare and request approval.","operation_id":id,"command":"operations continue","arguments":{"operation_id":id,"token":next.token},"continuation_token":next.token});
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .interrupt_preapproval_continuation(
                lease,
                token,
                &to_value(&expected)?,
                &to_value(&record)?,
                &view,
                &record.updated_at,
            )
            .map_err(map_store)?;
        Ok(view)
    }

    async fn continue_preapproval(
        &self,
        id: &str,
        token: &str,
        lease: &gofin_store::execution_admission::StageInvocationLease,
        preapproval: gofin_store::execution_admission::PreapprovalContinuation,
    ) -> Result<(Value, bool)> {
        if preapproval.action_id != id {
            return Err(GoFinError::validation(
                "Continuation token belongs to a different action.",
            ));
        }
        gofin_domain::execution_progress::ensure_invocation_active()?;
        let mut record = self.load_record(id)?;
        if record.run_id.is_some() {
            return Err(retired_execution_interface());
        }
        let (_, created) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .claim_preapproval_continuation(lease, token, &record.fingerprint)
            .map_err(map_store)?;
        if !created {
            if let Some(result) = preapproval.result {
                return Ok((result, false));
            }
            if preapproval.result.is_none()
                && record.approval.is_none()
                && record.submission.is_none()
            {
                return Ok((
                    self.finish_interrupted_preapproval(id, token, lease)?,
                    false,
                ));
            }
            if record.approval.is_some() && record.submission.is_none() {
                self.recover_approved_action(&record)?;
            }
            return Ok((self.operation_view(&record)?, false));
        }
        if let Err(error) = gofin_domain::execution_progress::ensure_invocation_active() {
            self.finish_interrupted_preapproval(id, token, lease)?;
            return Err(error);
        }
        let request = OperationRequest {
            request_id: record.request_id.clone(),
            operation: record.operation.clone(),
            input: record.input.clone(),
            run_id: record.run_id.clone(),
        };
        if request_fingerprint(&request)? != record.fingerprint
            || record.approval.is_some()
            || record.submission.is_some()
        {
            return Err(GoFinError::validation(
                "The original unsubmitted action no longer matches its continuation.",
            ));
        }
        let (adapter, definition) = self.resolve_definition(&request.operation)?;
        if definition.kind != OperationKind::Write
            || definition.execution_actor != ExecutionActor::AgentInvocable
        {
            return Err(GoFinError::validation(
                "The original action is no longer available for execution.",
            ));
        }
        record = self.load_record(id)?;
        let attempted = Box::pin(self.execute_new(
            adapter,
            &request,
            &mut record,
            None,
            ExecutionRoute::Financial,
            Some(lease),
        ))
        .await;
        let mut retryable = false;
        let record = match attempted {
            Ok(record) => record,
            Err(error) => {
                let unsubmitted = record.submission.is_none()
                    && self
                        .store
                        .lock()
                        .map_err(|_| internal("store lock poisoned"))?
                        .submission(id)
                        .map_err(map_store)?
                        .is_none();
                record.status = if unsubmitted {
                    status_for_error(&error)
                } else {
                    Status::Unknown
                };
                record.error = Some(error);
                record.updated_at = self.clock.now().to_rfc3339();
                if unsubmitted {
                    record.last_reconciliation_evidence =
                        Some(json!({"engine_predispatch_failure":true}));
                }
                retryable = unsubmitted && record.approval.is_none() && record.status.terminal();
                self.update_record(&record)?;
                record
            }
        };
        let view = self.operation_view(&record)?;
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .finish_preapproval_continuation(lease, token, &view, retryable)
            .map_err(map_store)?;
        let advance = record.operation == "bridge.execute"
            && record.status == Status::Pending
            && record.approval.is_some();
        Ok((self.operation_view(&record)?, advance))
    }

    pub(super) async fn continue_with_lease(
        &self,
        id: &str,
        token: &str,
        lease: &gofin_store::execution_admission::StageInvocationLease,
    ) -> Result<Value> {
        if !gofin_domain::execution_progress::invocation_context_present() {
            return Err(GoFinError::validation(
                "Continuation requires an active foreground invocation.",
            ));
        }
        let mut current_token = token.to_owned();
        'stages: loop {
            let token = current_token.as_str();
            let continuation = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .action_continuation(token)
                .map_err(map_store)?
                .ok_or_else(|| GoFinError::validation("Unknown continuation token."))?;
            if continuation.action_id != id {
                return Err(GoFinError::validation(
                    "Continuation token belongs to a different action.",
                ));
            }
            if let Some(result) = continuation.result {
                return Ok(result);
            }
            if let Some(stage_token) = continuation.stage_token {
                let stage = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .stage_attempt(&stage_token)
                    .map_err(map_store)?
                    .ok_or_else(|| internal("continuation stage missing"))?;
                let _action = self.load_record(id)?;
                let mut view = self.operation_view(&self.load_record(id)?)?;
                if let Some(child_id) = stage.operation_id {
                    view["stage"] = self.operation_view(&self.status(&child_id, true).await?)?;
                }
                return Ok(view);
            }
            gofin_domain::execution_progress::ensure_invocation_active()?;
            let action = self.current_admission_action(&self.load_record(id)?)?;
            let attempts = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .list_stage_attempts(id)
                .map_err(map_store)?;
            for stage in &attempts {
                if matches!(stage.state.as_str(), "not_dispatched" | "rejected")
                    && let Some(operation_id) = stage.operation_id.as_deref()
                    && self
                        .load_record(operation_id)
                        .is_ok_and(|r| r.submission.is_none())
                {
                    self.store
                        .lock()
                        .map_err(|_| internal("store lock poisoned"))?
                        .release_orphaned_evm_nonce(lease, operation_id)
                        .map_err(map_store)?;
                }
                if stage.state == "claimed" {
                    let (claim, _) = self
                        .store
                        .lock()
                        .map_err(|_| internal("store lock poisoned"))?
                        .bind_and_issue_action_continuation(lease, token, &stage.token)
                        .map_err(map_store)?;
                    if !claim.created {
                        return self.operation_view(&self.load_record(id)?);
                    }
                    let view = Box::pin(self.resume_claimed_stage(&action, stage, lease)).await?;
                    if view.pointer("/stage/status").and_then(Value::as_str) == Some("completed")
                        && gofin_domain::execution_progress::invocation_active()
                    {
                        let next = self
                            .store
                            .lock()
                            .map_err(|_| internal("store lock poisoned"))?
                            .latest_action_continuation(id)
                            .map_err(map_store)?;
                        if let Some(next) =
                            next.filter(|n| n.stage_token.is_none() && n.result.is_none())
                        {
                            current_token = next.token;
                            continue 'stages;
                        }
                    }
                    return Ok(view);
                }
                if stage.state == "issued" {
                    let draft = self
                        .store
                        .lock()
                        .ok()
                        .and_then(|s| {
                            s.by_request(&format!("compound-stage:{}", stage.token))
                                .ok()
                                .flatten()
                        })
                        .and_then(|r| from_value::<OperationRecord>(r.record).ok());
                    if let Some(mut draft) = draft.clone().filter(|r| r.submission.is_none()) {
                        draft.status = Status::Failed;
                        mark_unsubmitted_stage(&mut draft);
                        draft.updated_at = self.clock.now().to_rfc3339();
                        self.update_record(&draft)?;
                        let mut store = self
                            .store
                            .lock()
                            .map_err(|_| internal("store lock poisoned"))?;
                        store
                            .finish_stage_attempt(
                                &stage.token,
                                "not_dispatched",
                                &json!({"reason":"unsubmitted draft recovered"}),
                            )
                            .map_err(map_store)?;
                        let _ = store.release_evm_nonce_if_unsubmitted(&draft.id);
                    } else if draft.is_none() {
                        self.store
                            .lock()
                            .map_err(|_| internal("store lock poisoned"))?
                            .finish_stage_attempt(
                                &stage.token,
                                "not_dispatched",
                                &json!({"reason":"unclaimed stage recovered"}),
                            )
                            .map_err(map_store)?;
                    }
                }
            }
            let Some((_adapter, stage_id, _candidate, _prior)) =
                Box::pin(self.prepare_compound_continuation(&action, None)).await?
            else {
                // The last admitted child may have established destination credit.
                // Observe it before returning, without admitting another stage.
                let action = self.load_record(id)?;
                let all_stages_settled = action
                    .preview
                    .as_ref()
                    .and_then(|p| p.binding.get("transfer_id"))
                    .and_then(Value::as_str)
                    .and_then(|transfer_id| {
                        self.store
                            .lock()
                            .ok()?
                            .bridge_transfer(transfer_id)
                            .ok()
                            .flatten()
                    })
                    .is_some_and(|transfer| {
                        !transfer.steps.is_empty()
                            && transfer.steps.iter().all(|step| {
                                matches!(
                                    step.state,
                                    gofin_domain::BridgeStepState::Settled
                                        | gofin_domain::BridgeStepState::Superseded
                                )
                            })
                    });
                let observed = if all_stages_settled {
                    Box::pin(self.observe_compound_action(action)).await?
                } else {
                    action
                };
                let result = self.operation_view(&observed)?;
                let mut store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                store
                    .finish_and_issue_action_continuation(lease, token, &result)
                    .map_err(map_store)?;
                drop(store);
                return self.operation_view(&self.load_record(id)?);
            };
            let audit = action
                .approval
                .as_ref()
                .ok_or_else(|| internal("compound approval missing"))?;
            gofin_domain::execution_progress::ensure_invocation_active()?;
            let stage_token = {
                let mut store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                let stage = store
                    .issue_stage_continuation(id, &stage_id, &audit.challenge_nonce)
                    .map_err(map_store)?;
                let (claim, _next) = store
                    .bind_and_issue_action_continuation(lease, token, &stage.token)
                    .map_err(map_store)?;
                if !claim.created {
                    drop(store);
                    return self.operation_view(&self.load_record(id)?);
                }
                stage.token
            };
            match Box::pin(self.continue_stage(id, &stage_token)).await {
                Ok(view) => {
                    if view.pointer("/stage/status").and_then(Value::as_str) == Some("completed")
                        && gofin_domain::execution_progress::invocation_active()
                    {
                        let next = self
                            .store
                            .lock()
                            .map_err(|_| internal("store lock poisoned"))?
                            .latest_action_continuation(id)
                            .map_err(map_store)?;
                        if let Some(next) =
                            next.filter(|next| next.stage_token.is_none() && next.result.is_none())
                        {
                            current_token = next.token;
                            continue 'stages;
                        }
                    }
                    return Ok(view);
                }
                Err(error) => {
                    let mut store = self
                        .store
                        .lock()
                        .map_err(|_| internal("store lock poisoned"))?;
                    if let Ok(Some(stage)) = store.stage_attempt(&stage_token)
                        && stage.state == "issued"
                    {
                        let _ = store.finish_stage_attempt(
                            &stage_token,
                            "not_dispatched",
                            &json!({"reason":"continuation ended before claim"}),
                        );
                    }
                    let draft = store
                        .by_request(&format!("compound-stage:{stage_token}"))
                        .ok()
                        .flatten()
                        .and_then(|row| from_value::<OperationRecord>(row.record).ok());
                    drop(store);
                    if let Some(mut draft) =
                        draft.filter(|r| r.submission.is_none() && !r.status.terminal())
                    {
                        draft.status = Status::Failed;
                        mark_unsubmitted_stage(&mut draft);
                        draft.error = Some(error.clone());
                        draft.updated_at = self.clock.now().to_rfc3339();
                        if self.update_record(&draft).is_ok()
                            && let Ok(mut store) = self.store.lock()
                        {
                            let _ = store.release_evm_nonce_if_unsubmitted(&draft.id);
                        }
                    }
                    return Err(error);
                }
            }
        }
    }

    /// Admit one explicitly nominated stage under the original approval.
    async fn continue_stage(&self, id: &str, token: &str) -> Result<Value> {
        let attempt = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .stage_attempt(token)
            .map_err(map_store)?
            .ok_or_else(|| GoFinError::validation("Unknown continuation token."))?;
        if attempt.action_id != id {
            return Err(GoFinError::validation(
                "Continuation token belongs to a different action.",
            ));
        }
        let action = self.current_admission_action(&self.load_record(id)?)?;
        if let Some(operation_id) = attempt.operation_id.as_deref() {
            let child = self.status(operation_id, true).await?;
            let mut view = self.operation_view(&self.load_record(id)?)?;
            view["stage"] = self.operation_view(&child)?;
            return Ok(view);
        }
        if attempt.state != "issued" {
            return self.operation_view(&self.load_record(id)?);
        }
        gofin_domain::execution_progress::ensure_invocation_active()?;
        let (parent_adapter, stage_id, mut candidate, prior) =
            Box::pin(self.prepare_compound_continuation(&action, Some(token)))
                .await?
                .ok_or_else(|| {
                    GoFinError::new(
                        "STAGE_NOT_READY",
                        "preflight",
                        "No stage is ready for continuation.",
                        "Observe the original action and check again.",
                    )
                })?;
        if stage_id != attempt.stage_id {
            return Err(GoFinError::validation(
                "Continuation token does not identify the ready stage.",
            ));
        }
        let parent = self.approved_action(&action)?;
        let audit = action
            .approval
            .clone()
            .ok_or_else(|| internal("compound approval missing"))?;
        if parse_time(&audit.expires_at)? <= self.clock.now() {
            return Err(GoFinError::validation("The original approval expired."));
        }
        let (adapter, definition) = self.resolve_definition(&candidate.operation)?;
        if definition.kind != OperationKind::Write {
            return Err(GoFinError::validation("A stage must be a financial write."));
        }
        let request = OperationRequest {
            request_id: format!("compound-stage:{token}"),
            operation: candidate.operation.clone(),
            input: candidate.input.clone(),
            run_id: None,
        };
        let fingerprint = request_fingerprint(&request)?;
        let now = self.clock.now().to_rfc3339();
        let child_id = Uuid::new_v4().to_string();
        let mut child = OperationRecord {
            id: child_id.clone(),
            request_id: request.request_id.clone(),
            fingerprint: fingerprint.clone(),
            fingerprint_version: 1,
            operation: request.operation.clone(),
            input: request.input.clone(),
            status: Status::AwaitingApproval,
            created_at: now.clone(),
            updated_at: now.clone(),
            preview: Some(candidate.clone()),
            approval: Some(audit.clone()),
            authorization_source: audit.source.clone(),
            submission: None,
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let draft = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .create_request(
                &child_id,
                &request.request_id,
                &fingerprint,
                &request.operation,
                None,
                "awaiting_approval",
                &to_value(&child)?,
                &now,
            )
            .map_err(map_store)?;
        if !draft.created {
            let existing: OperationRecord = from_value(draft.record)?;
            let mut view = self.operation_view(&self.load_record(id)?)?;
            view["stage"] = self.operation_view(&existing)?;
            return Ok(view);
        }
        let guards = acquire_submission_resources(&candidate).await;
        let mut nonce_claimed = false;
        let mut nonce_candidate = candidate.clone();
        if let Some((scope, pending)) = bounded_evidence(
            &candidate.operation,
            "stage nonce observation",
            PREFLIGHT_TIMEOUT,
            adapter.evm_nonce_scope(&candidate),
        )
        .await?
        {
            gofin_domain::execution_progress::ensure_invocation_active()?;
            let nonce = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .claim_evm_nonce(&scope, &child_id, pending)
                .map_err(map_store)?;
            nonce_claimed = true;
            nonce_candidate = adapter.apply_claimed_evm_nonce(&candidate, nonce)?;
            validate_parent_stage(
                parent_adapter.as_ref(),
                &parent,
                &stage_id,
                &nonce_candidate,
            )?;
        }
        let current = bounded_evidence(
            &candidate.operation,
            "stage revalidation",
            PREFLIGHT_TIMEOUT,
            adapter.revalidate(&nonce_candidate),
        )
        .await?;
        validate_parent_stage(parent_adapter.as_ref(), &parent, &stage_id, &current)?;
        validate_stage_fee_caps(&parent, &current, &prior)?;
        validate_prepared(&current, &current.operation, self.clock.now())?;
        let finalized = bounded_evidence(
            &current.operation,
            "stage finalization",
            PREFLIGHT_TIMEOUT,
            adapter.finalize_after_reservation(&current),
        )
        .await?;
        validate_finalized_candidate(&current, &finalized)?;
        validate_parent_stage(parent_adapter.as_ref(), &parent, &stage_id, &finalized)?;
        validate_stage_fee_caps(&parent, &finalized, &prior)?;
        candidate = finalized;
        gofin_domain::execution_progress::ensure_invocation_active()?;
        child.preview = Some(candidate.clone());
        child.updated_at = self.clock.now().to_rfc3339();
        let bridge_link = if request.operation == "bridge.transfer" {
            Some(
                boundary_authorizations::bridge_step_claim(&request, None, &child_id, self)?
                    .ok_or_else(|| {
                        GoFinError::validation("Bridge stage lacks its durable transfer identity.")
                    })?,
            )
        } else {
            None
        };
        gofin_domain::execution_progress::ensure_invocation_active()?;
        let (created, claimed) = {
            let mut store = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?;
            if let Some(link) = bridge_link.as_ref() {
                store
                    .create_bridge_stage_attempt(
                        &child_id,
                        &request.request_id,
                        &fingerprint,
                        &request.operation,
                        None,
                        "awaiting_approval",
                        &to_value(&child)?,
                        &now,
                        link,
                        token,
                        &to_value(&candidate)?,
                    )
                    .map_err(map_store)?
            } else {
                drop(store);
                self.update_record(&child)?;
                let mut store = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?;
                let claimed = store
                    .claim_stage_attempt(token, &child_id, &to_value(&candidate)?)
                    .map_err(map_store)?;
                (draft, claimed)
            }
        };
        if !created.created || !claimed.created {
            let existing: OperationRecord = from_value(created.record)?;
            let mut view = self.operation_view(&self.load_record(id)?)?;
            view["stage"] = self.operation_view(&existing)?;
            return Ok(view);
        }
        let journal = Arc::new(OperationSubmissionJournal {
            operation_id: child_id.clone(),
            authorization_route: SubmissionAuthorizationRoute::Financial,
            expected_binding_hash: audit.binding_hash.clone(),
            approved_candidate: Some(candidate.clone()),
            resource_guards: Mutex::new(Some(guards)),
            release_resource_guards_on_admission: nonce_claimed
                || (candidate.input.get("requestId").and_then(Value::as_str)
                    == Some(request.request_id.as_str())
                    && adapter.supports_independent_submission(&candidate)),
            compound_stage: Some(CompoundJournalStage {
                token: token.into(),
                stage_id,
                parent,
                parent_adapter,
            }),
            approval: audit,
            authorizer: Some(self.authorizer.clone()),
            adapter: Some(adapter.clone()),
            clock: self.clock.clone(),
            consumed: AtomicBool::new(false),
            store: self.store.clone(),
            record: Mutex::new(child.clone()),
        });
        let validated =
            gofin_domain::RevalidatedOperation::after_revalidation(candidate, self.clock.clone())?;
        let result = adapter
            .submit_revalidated(&validated, journal.clone())
            .await;
        child = journal
            .record
            .lock()
            .map_err(|_| internal("journal record lock poisoned"))?
            .clone();
        if let Err(error) = result {
            drop(journal);
            if child.submission.is_none() {
                let _ = self
                    .store
                    .lock()
                    .map_err(|_| internal("store lock poisoned"))?
                    .finish_stage_attempt(
                        token,
                        "not_dispatched",
                        &json!({"reason":"submission was not admitted"}),
                    );
            }
            child.error = Some(error);
            child.status = if child.submission.is_some() {
                Status::Unknown
            } else {
                Status::Rejected
            };
            if child.submission.is_none() {
                mark_unsubmitted_stage(&mut child);
            }
            self.update_record(&child)?;
        } else if let Ok(submission) = result {
            if child.submission.as_ref() != Some(&submission) {
                return Err(GoFinError::new(
                    "SUBMISSION_IDENTITY_MISMATCH",
                    "submission",
                    "The stage response differs from the durable submission identity.",
                    "Observe the original stage; do not submit it again.",
                ));
            }
            journal.record_transport_acceptance(&submission)?;
            child = journal
                .record
                .lock()
                .map_err(|_| internal("journal record lock poisoned"))?
                .clone();
            drop(journal);
            child.status = Status::Pending;
            self.update_record(&child)?;
            child = Box::pin(self.reconcile_serial(child.id.clone())).await?;
        }
        let mut view = self.operation_view(&self.load_record(id)?)?;
        view["stage"] = self.operation_view(&child)?;
        Ok(view)
    }

    async fn resume_claimed_stage(
        &self,
        action: &OperationRecord,
        stage: &gofin_store::execution_admission::StageAttempt,
        lease: &gofin_store::execution_admission::StageInvocationLease,
    ) -> Result<Value> {
        let parent = self.approved_action(action)?;
        let audit = action
            .approval
            .clone()
            .ok_or_else(|| internal("approval missing"))?;
        if parse_time(&audit.expires_at)? <= self.clock.now() {
            return Err(GoFinError::validation("The original approval expired."));
        }
        let child_id = stage
            .operation_id
            .as_deref()
            .ok_or_else(|| internal("claimed stage has no operation"))?;
        let mut child = self.load_record(child_id)?;
        if child.submission.is_some() {
            let mut view = self.operation_view(&self.load_record(&action.id)?)?;
            view["stage"] = self.operation_view(&child)?;
            return Ok(view);
        }
        let old_value = stage
            .candidate
            .as_ref()
            .ok_or_else(|| internal("claimed stage candidate missing"))?;
        let mut candidate: PreparedOperation = from_value(old_value.clone())?;
        let parent_adapter = self.resolve_definition(&action.operation)?.0;
        let adapter = self.resolve_definition(&candidate.operation)?.0;
        let attempts = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .list_stage_attempts(&action.id)
            .map_err(map_store)?;
        let mut prior = Vec::new();
        for other in attempts.iter().filter(|a| a.state == "completed") {
            prior.push(
                self.load_record(
                    other
                        .operation_id
                        .as_deref()
                        .ok_or_else(|| internal("completed stage has no operation"))?,
                )?,
            );
        }
        let guards = acquire_submission_resources(&candidate).await;
        let mut nonce_claimed = false;
        if let Some((scope, pending)) = bounded_evidence(
            &candidate.operation,
            "stage nonce observation",
            PREFLIGHT_TIMEOUT,
            adapter.evm_nonce_scope(&candidate),
        )
        .await?
        {
            gofin_domain::execution_progress::ensure_invocation_active()?;
            let nonce = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .claim_evm_nonce(&scope, child_id, pending)
                .map_err(map_store)?;
            nonce_claimed = true;
            candidate = adapter.apply_claimed_evm_nonce(&candidate, nonce)?;
        }
        let current = bounded_evidence(
            &candidate.operation,
            "stage revalidation",
            PREFLIGHT_TIMEOUT,
            adapter.revalidate(&candidate),
        )
        .await?;
        validate_parent_stage(parent_adapter.as_ref(), &parent, &stage.stage_id, &current)?;
        validate_stage_fee_caps(&parent, &current, &prior)?;
        validate_prepared(&current, &current.operation, self.clock.now())?;
        let finalized = bounded_evidence(
            &current.operation,
            "stage finalization",
            PREFLIGHT_TIMEOUT,
            adapter.finalize_after_reservation(&current),
        )
        .await?;
        validate_finalized_candidate(&current, &finalized)?;
        validate_parent_stage(
            parent_adapter.as_ref(),
            &parent,
            &stage.stage_id,
            &finalized,
        )?;
        validate_stage_fee_caps(&parent, &finalized, &prior)?;
        gofin_domain::execution_progress::ensure_invocation_active()?;
        child.preview = Some(finalized.clone());
        child.updated_at = self.clock.now().to_rfc3339();
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .refresh_claimed_stage(
                lease,
                &stage.token,
                old_value,
                &to_value(&finalized)?,
                &to_value(&child)?,
                &child.updated_at,
            )
            .map_err(map_store)?;
        let journal = Arc::new(OperationSubmissionJournal {
            operation_id: child.id.clone(),
            authorization_route: SubmissionAuthorizationRoute::Financial,
            expected_binding_hash: audit.binding_hash.clone(),
            approved_candidate: Some(finalized.clone()),
            resource_guards: Mutex::new(Some(guards)),
            release_resource_guards_on_admission: nonce_claimed
                || (finalized.input.get("requestId").and_then(Value::as_str)
                    == Some(child.request_id.as_str())
                    && adapter.supports_independent_submission(&finalized)),
            compound_stage: Some(CompoundJournalStage {
                token: stage.token.clone(),
                stage_id: stage.stage_id.clone(),
                parent,
                parent_adapter,
            }),
            approval: audit,
            authorizer: Some(self.authorizer.clone()),
            adapter: Some(adapter.clone()),
            clock: self.clock.clone(),
            consumed: AtomicBool::new(false),
            store: self.store.clone(),
            record: Mutex::new(child.clone()),
        });
        let validated =
            gofin_domain::RevalidatedOperation::after_revalidation(finalized, self.clock.clone())?;
        let result = adapter
            .submit_revalidated(&validated, journal.clone())
            .await;
        child = journal
            .record
            .lock()
            .map_err(|_| internal("journal record lock poisoned"))?
            .clone();
        match result {
            Ok(submission) if child.submission.as_ref() == Some(&submission) => {
                journal.record_transport_acceptance(&submission)?;
                child = journal
                    .record
                    .lock()
                    .map_err(|_| internal("journal record lock poisoned"))?
                    .clone();
                drop(journal);
                child.status = Status::Pending;
                self.update_record(&child)?;
                child = Box::pin(self.reconcile_serial(child.id.clone())).await?;
            }
            Ok(_) => {
                drop(journal);
                return Err(GoFinError::validation(
                    "The stage response differs from its durable submission identity.",
                ));
            }
            Err(error) => {
                drop(journal);
                if child.submission.is_none() {
                    let _ = self
                        .store
                        .lock()
                        .map_err(|_| internal("store lock poisoned"))?
                        .finish_stage_attempt(
                            &stage.token,
                            "not_dispatched",
                            &json!({"reason":"submission was not admitted"}),
                        );
                }
                child.status = if child.submission.is_some() {
                    Status::Unknown
                } else {
                    Status::Rejected
                };
                if child.submission.is_none() {
                    mark_unsubmitted_stage(&mut child);
                }
                child.error = Some(error);
                self.update_record(&child)?;
                if child.submission.is_none()
                    && let Ok(mut store) = self.store.lock()
                {
                    let _ = store.release_evm_nonce_if_unsubmitted(&child.id);
                }
            }
        }
        let mut view = self.operation_view(&self.load_record(&action.id)?)?;
        view["stage"] = self.operation_view(&child)?;
        Ok(view)
    }

    pub(super) async fn observe_compound_action(
        &self,
        mut action: OperationRecord,
    ) -> Result<OperationRecord> {
        let attempts = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .list_stage_attempts(&action.id)
            .map_err(map_store)?;
        for attempt in &attempts {
            if attempt.state == "completed"
                && let Some(id) = attempt.operation_id.as_deref()
                && self
                    .load_record(id)
                    .is_ok_and(|child| Self::native_fee_needs_observation(&child))
            {
                let _ = self.reconcile_observation_serial(id.to_owned()).await;
            }
            if matches!(attempt.state.as_str(), "admitted" | "uncertain")
                && let Some(id) = attempt.operation_id.as_deref()
                && let Ok(child) = self.load_record(id)
            {
                let disposition = if child.effects.is_some() {
                    match child.status {
                        Status::Completed => Some("completed"),
                        Status::Failed => Some("failed"),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(disposition) = disposition {
                    if let Ok(facts) = to_value(&operation_resolutions::outcome_facts(&child))
                        && let Ok(mut store) = self.store.lock()
                    {
                        let _ = store.finish_stage_attempt(&attempt.token, disposition, &facts);
                    }
                } else {
                    let _ = self.reconcile_observation_serial(id.to_owned()).await;
                }
            }
        }
        let adapter = self
            .adapters
            .read()
            .map_err(|_| internal("registry lock poisoned"))?
            .get(&action.operation)
            .cloned();
        if let Some(adapter) = adapter
            && let Ok(observation) = bounded_evidence(
                &action.operation,
                "destination observation",
                FINANCIAL_EVIDENCE_TIMEOUT,
                adapter.observe_transfer_side(&action, "destination"),
            )
            .await
        {
            action = self.apply_compound_destination_observation(action, observation.metadata)?;
        }
        Ok(action)
    }

    pub(super) fn apply_compound_destination_observation(
        &self,
        mut action: OperationRecord,
        observation: Value,
    ) -> Result<OperationRecord> {
        let attempts = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .list_stage_attempts(&action.id)
            .map_err(map_store)?;
        let expected = action
            .preview
            .as_ref()
            .and_then(|p| p.binding.get("transfer_id"))
            .and_then(Value::as_str);
        if observation.get("transfer_id").and_then(Value::as_str) == expected && expected.is_some()
        {
            use gofin_domain::outcome::{
                Attention, Dispatch, OutcomeFacts, OutcomeResult, Reporting, TransferState,
            };
            let state = observation
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("");
            let settlement = observation.get("settlement").filter(|v| v.is_object());
            let proof = settlement
                .and_then(|s| s.get("evidence_id"))
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            let (result, transfer) = match (state, proof) {
                ("completed", true) => (OutcomeResult::Executed, TransferState::Available),
                ("refunded", true) => (OutcomeResult::Cancelled, TransferState::ReturnedToSource),
                ("failed", _) => (OutcomeResult::Failed, TransferState::SourcePending),
                ("in_transit" | "destination_pending" | "refund_pending", _) => {
                    (OutcomeResult::Pending, TransferState::InTransit)
                }
                _ => (OutcomeResult::Pending, TransferState::SourcePending),
            };
            let now = self.clock.now().to_rfc3339();
            let observed_at = settlement
                .and_then(|s| s.get("observed_at"))
                .and_then(Value::as_str)
                .unwrap_or(&now)
                .to_owned();
            let mut attention = if state == "needs_attention" {
                Attention::ActionRequired {
                    venue: "Bridge provider".into(),
                    reason: "The original bridge needs attention.".into(),
                    where_to_act:
                        "Inspect the original bridge status and follow its recovery guidance."
                            .into(),
                }
            } else {
                Attention::None
            };
            if result == OutcomeResult::Executed
                && let (Some(approved), Some(actual)) = (
                    action.preview.as_ref().and_then(|p| {
                        p.intent
                            .expected
                            .iter()
                            .find(|e| matches!(e, EffectConstraint::Transfer { .. }))
                    }),
                    settlement
                        .and_then(|s| s.get("cash_credits"))
                        .and_then(Value::as_array)
                        .and_then(|rows| rows.first()),
                )
                && let EffectConstraint::Transfer {
                    to,
                    credit_asset,
                    minimum_credit,
                    ..
                } = approved
            {
                let actual_amount = actual
                    .get("amount_atoms")
                    .and_then(Value::as_str)
                    .and_then(|v| parse_uint(v).ok());
                let actual_asset = actual
                    .get("asset")
                    .and_then(|v| serde_json::from_value::<gofin_domain::AssetId>(v.clone()).ok());
                if actual.get("resource").and_then(Value::as_str) != Some(to.0.as_str())
                    || actual_asset.as_ref() != Some(credit_asset)
                    || actual_amount.is_some_and(|amount| amount < minimum_credit.integer())
                {
                    attention = Attention::Discrepancy {
                        actual: actual.clone(),
                        approved: to_value(approved)?,
                    };
                }
            }
            if let (Some(preview), Some(rows)) = (
                action.preview.as_ref(),
                settlement
                    .and_then(|s| s.get("costs"))
                    .and_then(Value::as_array),
            ) {
                let mut bounds = aggregate_cost_bounds(&preview.intent.maximum_costs)?;
                // Older bridge approvals represented a destination-withheld fee
                // as included in debit. Preserve the signed terms, but interpret
                // that historical label against the exact original native cap.
                if let Some(caps) = action
                    .input
                    .pointer("/bounds/fee_caps")
                    .and_then(Value::as_array)
                {
                    for bound in &preview.intent.maximum_costs {
                        if bound.treatment != CostTreatment::IncludedInDebit {
                            continue;
                        }
                        let destination_matches = preview.intent.expected.iter().any(|effect| matches!(effect,
                            EffectConstraint::Transfer { to, credit_asset, .. }
                                if bound.location == (AccountingLocation::Cash { resource: to.clone() })
                                    && bound.amount.asset == *credit_asset));
                        let original_cap_matches = caps.iter().any(|cap| {
                            cap.get("kind").and_then(Value::as_str) == Some("withheld_from_output")
                                && cap
                                    .get("asset")
                                    .and_then(|v| {
                                        serde_json::from_value::<gofin_domain::AssetId>(v.clone())
                                            .ok()
                                    })
                                    .as_ref()
                                    == Some(&bound.amount.asset)
                                && cap
                                    .get("maximum")
                                    .and_then(|v| {
                                        serde_json::from_value::<gofin_domain::Quantity>(v.clone())
                                            .ok()
                                    })
                                    .is_some_and(|amount| amount == bound.amount.atoms)
                        });
                        let unique_bound = preview
                            .intent
                            .maximum_costs
                            .iter()
                            .filter(|other| {
                                other.location == bound.location
                                    && other.amount.asset == bound.amount.asset
                                    && other.treatment == bound.treatment
                            })
                            .count()
                            == 1;
                        if destination_matches && original_cap_matches && unique_bound {
                            let old = cost_key(
                                &bound.location,
                                &bound.amount.asset,
                                &CostTreatment::IncludedInDebit,
                            )?;
                            let corrected = cost_key(
                                &bound.location,
                                &bound.amount.asset,
                                &CostTreatment::DeductedFromCredit,
                            )?;
                            if let Some(amount) = bounds.remove(&old) {
                                *bounds.entry(corrected).or_default() += amount;
                            }
                        }
                    }
                }
                let mut actual = BTreeMap::<String, BigUint>::new();
                for row in rows {
                    let location: AccountingLocation = from_value(row["location"].clone())?;
                    let asset: gofin_domain::AssetId = from_value(row["asset"].clone())?;
                    let treatment: CostTreatment = from_value(row["treatment"].clone())?;
                    let amount = row["amount_atoms"]
                        .as_str()
                        .ok_or_else(|| internal("bridge settlement cost amount missing"))?;
                    *actual
                        .entry(cost_key(&location, &asset, &treatment)?)
                        .or_default() += parse_uint(amount)?;
                }
                if actual
                    .iter()
                    .any(|(key, amount)| amount > bounds.get(key).unwrap_or(&BigUint::zero()))
                {
                    attention = Attention::Discrepancy {
                        actual: to_value(rows)?,
                        approved: to_value(&preview.intent.maximum_costs)?,
                    };
                }
            }
            let facts = OutcomeFacts {
                dispatch: if attempts.iter().any(|s| {
                    matches!(
                        s.state.as_str(),
                        "admitted" | "uncertain" | "completed" | "failed"
                    )
                }) {
                    Dispatch::Dispatched
                } else {
                    Dispatch::NotDispatched
                },
                result: result.clone(),
                transfer: Some(transfer),
                attention,
                reporting: Reporting::Partial,
                duplicate_execution_excluded: false,
                next_check_after: None,
                observed_at,
            };
            if matches!(result, OutcomeResult::Executed | OutcomeResult::Cancelled) {
                action.status = if result == OutcomeResult::Cancelled {
                    Status::Cancelled
                } else {
                    Status::Completed
                };
                action.last_reconciliation_evidence = Some(observation.clone());
                action.updated_at = self.clock.now().to_rfc3339();
                self.update_record(&action)?;
            }
            self.store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .append_outcome_observation(&action.id, &facts, "bridge_destination")
                .map_err(map_store)?;
        }
        Ok(action)
    }

    /// Prepare a ready stage without admitting or dispatching it. The adapter
    /// receives only durable prior operations and must not submit from this hook.
    pub(super) async fn prepare_compound_continuation(
        &self,
        action: &OperationRecord,
        current_token: Option<&str>,
    ) -> Result<
        Option<(
            Arc<dyn Adapter>,
            String,
            PreparedOperation,
            Vec<OperationRecord>,
        )>,
    > {
        let approved = self.approved_action(action)?;
        let parent = &approved;
        let adapter = self
            .adapters
            .read()
            .map_err(|_| internal("registry lock poisoned"))?
            .get(&action.operation)
            .cloned()
            .ok_or_else(|| internal("compound adapter unavailable"))?;
        let compound = adapter.is_compound_action(parent);
        let attempts = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .list_stage_attempts(&action.id)
            .map_err(map_store)?;
        let authority_id = action
            .approval
            .as_ref()
            .ok_or_else(|| internal("action approval missing"))?
            .challenge_nonce
            .as_str();
        if !compound
            && action.submission.is_some()
            && attempts
                .iter()
                .rfind(|a| current_token != Some(a.token.as_str()))
                .is_none_or(|a| {
                    a.state != "rejected"
                        && !(a.state == "failed" && a.authority_id != authority_id)
                })
        {
            return Ok(None);
        }
        let mut records = Vec::new();
        for attempt in attempts {
            if current_token == Some(attempt.token.as_str()) && attempt.state == "issued" {
                continue;
            }
            if matches!(
                attempt.state.as_str(),
                "issued" | "claimed" | "admitted" | "uncertain"
            ) {
                return Ok(None);
            }
            if attempt.state == "failed" && attempt.authority_id == authority_id {
                return Err(GoFinError::new(
                    "STAGE_FAILED",
                    "preflight",
                    "A prior stage failed.",
                    "Observe it and obtain new approval before another attempt.",
                ));
            }
            if matches!(attempt.state.as_str(), "completed" | "failed") {
                let id = attempt
                    .operation_id
                    .as_deref()
                    .ok_or_else(|| internal("completed stage lacks an operation"))?;
                records.push(self.load_record(id)?);
            }
        }
        let context = PreparationContext::default();
        let (stage_id, candidate) = if compound {
            let first = bounded_evidence(
                &action.operation,
                "stage preparation",
                PREFLIGHT_TIMEOUT,
                adapter.prepare_next_stage(action, &records, &context),
            )
            .await;
            let stage = match first {
                Err(error) if error.code == "PREVIEW_EXPIRED" => {
                    gofin_domain::execution_progress::ensure_invocation_active()?;
                    bounded_evidence(
                        &action.operation,
                        "fresh stage preparation",
                        PREFLIGHT_TIMEOUT,
                        adapter.prepare_next_stage(action, &records, &context),
                    )
                    .await?
                }
                result => result?,
            };
            let Some(stage) = stage else {
                return Ok(None);
            };
            stage
        } else {
            let candidate = bounded_evidence(
                &action.operation,
                "retry preparation",
                PREFLIGHT_TIMEOUT,
                adapter.prepare_with_context(&action.operation, &action.input, &context),
            )
            .await?;
            ("standalone".to_owned(), candidate)
        };
        validate_parent_stage(adapter.as_ref(), parent, &stage_id, &candidate)?;
        validate_stage_fee_caps(parent, &candidate, &records)?;
        validate_prepared(&candidate, &candidate.operation, self.clock.now())?;
        Ok(Some((adapter, stage_id, candidate, records)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prepared(atoms: u64, asset: &str) -> PreparedOperation {
        PreparedOperation {
            operation: "synthetic.stage".into(),
            input: json!({}),
            binding: json!({}),
            summary: "stage".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
            valuation: None,
            operation_version: 1,
            action_deadline: None,
            intent: FinancialIntent {
                debits: vec![],
                expected: vec![],
                locks: vec![],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash {
                        resource: ResourceId("wallet".into()),
                    },
                    amount: Amount {
                        asset: gofin_domain::AssetId {
                            network: "test:1".into(),
                            reference: asset.into(),
                            decimals: 6,
                        },
                        atoms: gofin_domain::Quantity::from_u64(atoms),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
            },
        }
    }
    #[test]
    fn parent_native_fee_cap_is_enforced_by_exact_asset() {
        let parent = prepared(10, "A");
        assert!(validate_stage_fee_caps(&parent, &prepared(10, "A"), &[]).is_ok());
        assert!(validate_stage_fee_caps(&parent, &prepared(11, "A"), &[]).is_err());
        assert!(validate_stage_fee_caps(&parent, &prepared(1, "B"), &[]).is_err());
        let mut pending: OperationRecord = serde_json::from_value(json!({
            "id":"s", "request_id":"r", "fingerprint":"f", "fingerprint_version":1,
            "operation":"synthetic.stage", "input":{}, "status":"pending",
            "created_at":now(), "updated_at":now(), "preview":null, "approval":null,
            "submission":null, "effects":null, "last_reconciliation_evidence":null,
            "error":null, "run_id":null
        }))
        .unwrap();
        pending.preview = Some(prepared(6, "A"));
        assert!(validate_stage_fee_caps(&parent, &prepared(4, "A"), &[pending.clone()]).is_ok());
        assert!(validate_stage_fee_caps(&parent, &prepared(5, "A"), &[pending.clone()]).is_err());
        // Inclusion can be proven before the actual native fee is available.
        // Reserve its approved maximum until the correlated fee is observed.
        pending.status = Status::Completed;
        assert!(validate_stage_fee_caps(&parent, &prepared(4, "A"), &[pending.clone()]).is_ok());
        assert!(validate_stage_fee_caps(&parent, &prepared(5, "A"), &[pending]).is_err());
    }
}
