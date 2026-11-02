//! A fresh human decision can accept uncertainty. It cannot become venue proof.
use super::*;
use gofin_domain::operation_recovery::{OperationResolutionInput, RecoveryReference};

fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "OPERATION_RESOLUTION_UNAVAILABLE",
        "recovery",
        message,
        "Inspect operation recovery. Preserve the original submission; do not replay it.",
    )
}

impl Engine {
    pub fn operation_view(&self, record: &OperationRecord) -> Result<Value> {
        let mut value = to_value(record)?;
        let (observations, incomplete_observations) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .outcome_observations_isolated(&record.id)
            .map_err(map_store)?;
        let mut facts = outcome_facts(record);
        if let Some(first) = observations.first() {
            facts = first.facts.clone();
            for observation in &observations[1..] {
                let mut next = observation.facts.clone();
                use gofin_domain::outcome::{
                    Attention, Dispatch, OutcomeResult, Reporting, TransferState,
                };
                if facts.dispatch == Dispatch::Dispatched && next.dispatch == Dispatch::Uncertain {
                    next.dispatch = Dispatch::Dispatched;
                }
                if matches!(
                    facts.result,
                    OutcomeResult::Pending | OutcomeResult::Partial
                ) && next.result == OutcomeResult::Unknown
                {
                    next.result = facts.result.clone();
                }
                if matches!(
                    facts.result,
                    OutcomeResult::Executed | OutcomeResult::Failed | OutcomeResult::Cancelled
                ) && matches!(
                    next.result,
                    OutcomeResult::Pending | OutcomeResult::Partial | OutcomeResult::Unknown
                ) {
                    next.result = facts.result.clone();
                }
                if next.result == facts.result && facts.duplicate_execution_excluded {
                    next.duplicate_execution_excluded = true;
                }
                if next.transfer.is_none()
                    && !(next.result == OutcomeResult::Executed
                        && record.status == Status::Completed
                        && record.effects.is_some()
                        && matches!(
                            facts.transfer,
                            Some(
                                TransferState::Unknown
                                    | TransferState::SourcePending
                                    | TransferState::InTransit
                            )
                        ))
                {
                    next.transfer = facts.transfer.clone();
                }
                if matches!(next.reporting, Reporting::Unavailable)
                    && !matches!(facts.reporting, Reporting::Unavailable)
                    || matches!(next.reporting, Reporting::Partial)
                        && matches!(facts.reporting, Reporting::Complete)
                {
                    next.reporting = facts.reporting.clone();
                }
                if matches!(next.attention, Attention::None)
                    && (matches!(facts.attention, Attention::Discrepancy { .. })
                        || (matches!(facts.attention, Attention::ActionRequired { .. })
                            && !matches!(
                                observation.source.as_str(),
                                "adapter" | "adapter_live_order" | "bridge_destination"
                            )))
                {
                    next.attention = facts.attention.clone();
                }
                if matches!(
                    (&facts.transfer, &next.transfer),
                    (
                        Some(TransferState::SourcePending),
                        Some(TransferState::Unknown)
                    ) | (
                        Some(TransferState::InTransit),
                        Some(TransferState::Unknown | TransferState::SourcePending)
                    )
                ) {
                    next.transfer = facts.transfer.clone();
                }
                if matches!(
                    facts.transfer,
                    Some(
                        TransferState::Credited
                            | TransferState::Available
                            | TransferState::ReturnedToSource
                    )
                ) && matches!(
                    next.transfer,
                    Some(
                        TransferState::Unknown
                            | TransferState::SourcePending
                            | TransferState::InTransit
                    )
                ) {
                    next.transfer = facts.transfer.clone();
                }
                facts = next;
            }
        }
        if observations.len() > 1 {
            value["outcome_observations"] = to_value(&observations)?;
        }
        if incomplete_observations {
            value["history_incomplete"] = json!(true);
        }
        // Older derived observations treated durable admission or a pending
        // status as proof of acceptance. Recompute that conclusion from the
        // retained correlated evidence instead of preserving the old guess.
        let transport_accepted = observations.iter().any(|observation| {
            observation.source == "engine_transport_accepted"
                && observation.facts.dispatch == gofin_domain::outcome::Dispatch::Dispatched
        });
        if facts.dispatch == gofin_domain::outcome::Dispatch::Dispatched
            && !verified_dispatch(record)
            && !transport_accepted
        {
            facts.dispatch = gofin_domain::outcome::Dispatch::Uncertain;
        }
        // Older engine-generated discrepancy snapshots embed the complete
        // approved intent and proved effects. Recalculate only that exact
        // signature; independent adapter/provider attention remains durable.
        if let gofin_domain::outcome::Attention::Discrepancy { actual, approved } = &facts.attention
            && record
                .effects
                .as_ref()
                .and_then(|v| to_value(&v.effects).ok())
                .as_ref()
                == Some(actual)
            && record
                .preview
                .as_ref()
                .and_then(|v| to_value(&v.intent).ok())
                .as_ref()
                == Some(approved)
        {
            facts.attention = native_execution_discrepancy(record)
                .unwrap_or(gofin_domain::outcome::Attention::None);
        }
        if verified_legacy_binance_whitelist_rejection(record) {
            let verified = outcome_facts(record);
            facts.dispatch = verified.dispatch;
            facts.result = verified.result;
            facts.attention = verified.attention;
            facts.duplicate_execution_excluded = true;
        }
        if verified_venue_withdrawal_received(record) {
            facts.transfer = Some(if record.operation == "hyperliquid.account.withdraw" {
                gofin_domain::outcome::TransferState::Available
            } else {
                gofin_domain::outcome::TransferState::Credited
            });
            facts.result = gofin_domain::outcome::OutcomeResult::Executed;
            facts.dispatch = gofin_domain::outcome::Dispatch::Dispatched;
        }
        if record.submission.is_none()
            && !(record.operation == "bridge.execute" && record.input.get("intent").is_some())
            && matches!(
                record.status,
                Status::Failed | Status::Rejected | Status::Expired | Status::Cancelled
            )
            && record.approval.is_some()
            && record.preview.is_some()
            && self
                .store
                .lock()
                .ok()
                .is_some_and(|s| matches!(s.submission(&record.id), Ok(None)))
            && record
                .error
                .as_ref()
                .is_none_or(|e| !e.code.eq_ignore_ascii_case("history_unreadable"))
        {
            facts.dispatch = gofin_domain::outcome::Dispatch::NotDispatched;
            facts.duplicate_execution_excluded = true;
        }
        apply_kamino_non_dispatch_proof(record, &mut facts);
        let admission = self.current_admission_action(record)?;
        let approval_valid = admission.approval.as_ref().is_some_and(|audit| {
            let fresh = parse_time(&audit.expires_at).is_ok_and(|expiry| expiry > self.clock.now());
            if !fresh {
                return false;
            }
            if audit.source == Some(gofin_domain::AuthorizationSource::Yolo) {
                self.authorizer
                    .acquire_submission_authority(audit)
                    .ok()
                    .is_some_and(|lease| {
                        validate_submission_authority_lease(audit, lease.as_ref()).is_ok()
                    })
            } else {
                validate_financial_approval_source(audit).is_ok()
            }
        });
        let explicit_horizon = record
            .last_reconciliation_evidence
            .as_ref()
            .and_then(|e| e.get("observation_horizon_at"))
            .and_then(Value::as_str)
            .and_then(|h| parse_time(h).ok())
            .is_some_and(|h| h <= self.clock.now());
        let adapter_horizon = self
            .adapters
            .read()
            .ok()
            .and_then(|a| a.get(&record.operation).cloned())
            .and_then(|adapter| adapter.observation_horizon(record));
        let submitted_at = self
            .store
            .lock()
            .ok()
            .and_then(|s| s.submission_admitted_at(&record.id).ok().flatten())
            .unwrap_or_else(|| record.created_at.clone());
        let has_dispatch = record.submission.is_some()
            || self
                .store
                .lock()
                .ok()
                .and_then(|s| s.list_stage_attempts(&record.id).ok())
                .is_some_and(|stages| {
                    stages.iter().any(|s| {
                        matches!(
                            s.state.as_str(),
                            "admitted" | "uncertain" | "completed" | "failed"
                        )
                    })
                });
        let horizon_anchor = observation_horizon_anchor(&observations, &facts, &submitted_at);
        let heuristic_horizon = adapter_horizon
            .filter(|_| has_dispatch)
            .and_then(|h| chrono::Duration::from_std(h).ok())
            .and_then(|h| {
                parse_time(horizon_anchor)
                    .ok()
                    .and_then(|at| at.checked_add_signed(h))
            })
            .is_some_and(|at| at <= self.clock.now());
        if !incomplete_observations
            && !has_dispatch
            && record.approval.is_none()
            && record.submission.is_none()
            && record
                .last_reconciliation_evidence
                .as_ref()
                .and_then(|e| e.get("engine_predispatch_failure"))
                == Some(&Value::Bool(true))
            && facts.dispatch == gofin_domain::outcome::Dispatch::NotDispatched
            && matches!(
                facts.transfer,
                None | Some(gofin_domain::outcome::TransferState::SourcePending)
            )
            && facts.result == gofin_domain::outcome::OutcomeResult::Pending
        {
            facts = outcome_facts(record);
        }
        if !has_dispatch && admission.approval.is_some() && !approval_valid {
            facts.dispatch = gofin_domain::outcome::Dispatch::NotDispatched;
            facts.result = gofin_domain::outcome::OutcomeResult::Failed;
            facts.duplicate_execution_excluded = true;
        }
        if has_dispatch
            && facts.next_check_after.is_none()
            && matches!(
                facts.result,
                gofin_domain::outcome::OutcomeResult::Pending
                    | gofin_domain::outcome::OutcomeResult::Unknown
                    | gofin_domain::outcome::OutcomeResult::Partial
            )
        {
            // Polling guidance is advisory; it is not evidence of nonexecution.
            let now = self.clock.now();
            let next = (now.timestamp() / 30 + 2) * 30;
            facts.next_check_after =
                chrono::DateTime::from_timestamp(next, 0).map(|at| at.to_rfc3339());
        }
        let horizon_passed = explicit_horizon || heuristic_horizon;
        let mut projected = facts.project(horizon_passed, approval_valid);
        // A correlated verified terminal receipt with no outstanding transfer is
        // final even when it proves effects that make replay unsafe.
        if projected.phase == gofin_domain::outcome::Phase::Failed
            && !projected.replay_safe
            && matches!(
                projected.transfer,
                Some(
                    gofin_domain::outcome::TransferState::Credited
                        | gofin_domain::outcome::TransferState::Available
                        | gofin_domain::outcome::TransferState::ReturnedToSource
                )
            )
            && record.effects.as_ref().is_some_and(|proof| {
                record.submission.as_ref() == Some(&proof.submission)
                    && verified_effect_digest(proof).ok().as_deref()
                        == Some(proof.proof_digest.as_str())
            })
        {
            projected.next_action.code = "none".into();
            projected.next_action.message = "No further action is needed.".into();
        }
        if incomplete_observations {
            projected.replay_safe = false;
            projected.retry_approval = None;
        }
        if projected.replay_safe && verified_legacy_binance_whitelist_rejection(record) {
            projected.retry_approval = Some(gofin_domain::outcome::RetryApproval::Required);
        }
        let unreadable = record
            .error
            .as_ref()
            .is_some_and(|e| e.code.eq_ignore_ascii_case("history_unreadable"));
        if unreadable {
            projected.replay_safe = false;
            projected.retry_approval = None;
            projected.next_action = gofin_domain::outcome::NextAction {
                continuation_token: None,
                operation_id: Some(record.id.clone()),
                command: Some("status".into()),
                arguments: Some(json!({"operation_id":record.id})),
                code: "wait_and_check".into(),
                message: "Check the original operation again; do not submit it again without proof that it cannot execute.".into(),
            };
            if unreadable {
                projected.dispatch = gofin_domain::outcome::Dispatch::Uncertain;
            }
        }
        let attempts = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .list_stage_attempts(&record.id)
            .map_err(map_store)?;
        // The historical bridge parent identity is durable even if the
        // adapter is unavailable or replaced after a catalog reload.
        let compound = (record.operation == "bridge.execute"
            && record.input.get("intent").is_some())
            || record.preview.as_ref().is_some_and(|p| {
                self.adapters
                    .read()
                    .ok()
                    .and_then(|a| a.get(&record.operation).cloned())
                    .is_some_and(|a| a.is_compound_action(p))
            });
        let latest_child = if !compound {
            attempts
                .iter()
                .rev()
                .find_map(|a| a.operation_id.as_deref().filter(|id| *id != record.id))
                .and_then(|id| self.load_record(id).ok())
        } else {
            None
        };
        let latest_rejected = attempts
            .iter()
            .rev()
            .find(|a| a.operation_id.as_deref().is_some_and(|id| id != record.id))
            .is_some_and(|a| a.state == "rejected");
        let continuation = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .latest_action_continuation(&record.id)
            .map_err(map_store)?;
        let has_passive_continuation = continuation
            .as_ref()
            .is_some_and(|c| c.stage_token.is_none() && c.result.is_none());
        let unresolved_stage = attempts
            .iter()
            .any(|a| matches!(a.state.as_str(), "claimed" | "admitted" | "uncertain"));
        if unresolved_stage {
            projected.replay_safe = false;
            projected.retry_approval = None;
            if projected.next_action.code != "act_at_venue" {
                projected.next_action = gofin_domain::outcome::NextAction {
                    code: "wait_and_check".into(),
                    message: "Observe the original action and its admitted attempt.".into(),
                    continuation_token: None,
                    operation_id: Some(record.id.clone()),
                    command: Some("status".into()),
                    arguments: Some(json!({"operation_id":record.id})),
                };
            }
        }
        let pending_review = continuation.as_ref().is_some_and(|c| {
            c.result
                .as_ref()
                .and_then(|r| r.get("reauthorization"))
                .and_then(Value::as_str)
                == Some("pending")
        });
        let needs_revised_bounds = compound
            && record.error.as_ref().is_some_and(|error| {
                matches!(
                    error.code.as_str(),
                    "FEE_LIMIT" | "BRIDGE_CUMULATIVE_FEE_LIMIT"
                )
            });
        let failed_current_authority = attempts.iter().any(|attempt| {
            (attempt.state == "failed"
                || (attempt.state == "rejected"
                    && attempt
                        .operation_id
                        .as_deref()
                        .and_then(|id| self.load_record(id).ok())
                        .is_some_and(|r| r.submission.is_some())))
                && admission
                    .approval
                    .as_ref()
                    .is_some_and(|audit| attempt.authority_id == audit.challenge_nonce)
        });
        let exact_candidate_expired = !compound
            && admission.preview.as_ref().is_some_and(|preview| {
                validate_prepared(preview, &preview.operation, self.clock.now()).is_err()
                    && self
                        .adapters
                        .read()
                        .ok()
                        .and_then(|a| a.get(&record.operation).cloned())
                        .is_none_or(|a| !a.supports_approved_reprepare(preview))
            });
        if projected.replay_safe && exact_candidate_expired {
            projected.retry_approval = Some(gofin_domain::outcome::RetryApproval::Required);
        }
        let settled_compound = compound
            && matches!(
                projected.result,
                gofin_domain::outcome::OutcomeResult::Executed
                    | gofin_domain::outcome::OutcomeResult::Cancelled
            )
            && matches!(
                projected.transfer,
                Some(
                    gofin_domain::outcome::TransferState::Available
                        | gofin_domain::outcome::TransferState::ReturnedToSource
                )
            );
        if (projected.next_action.code != "act_at_venue" || (compound && failed_current_authority))
            && !incomplete_observations
            && ((has_passive_continuation
                && (!approval_valid
                    || needs_revised_bounds
                    || failed_current_authority
                    || projected.retry_approval
                        == Some(gofin_domain::outcome::RetryApproval::Required))
                && !unresolved_stage
                && (compound || projected.replay_safe))
                || pending_review)
            && !settled_compound
            && let Some(continuation) = continuation.as_ref()
        {
            projected.next_action = gofin_domain::outcome::NextAction {
                code: "reauthorize".into(),
                message: if pending_review {
                    "The previous review was interrupted; use this token to recover its status."
                        .into()
                } else {
                    "Review explicit revised terms before another attempt.".into()
                },
                continuation_token: Some(continuation.token.clone()),
                operation_id: Some(record.id.clone()),
                command: Some("operations reauthorize".into()),
                arguments: Some(
                    json!({"operation_id":record.id,"token":continuation.token,"input_required":true}),
                ),
            };
        }
        let reauthorized_failed = attempts.last().is_some_and(|attempt| {
            matches!(attempt.state.as_str(), "failed" | "rejected")
                && admission
                    .approval
                    .as_ref()
                    .is_some_and(|audit| attempt.authority_id != audit.challenge_nonce)
        });
        if reauthorized_failed && approval_valid && !unresolved_stage && projected.replay_safe {
            projected.retry_approval = Some(gofin_domain::outcome::RetryApproval::Covered);
        }
        if let Some(continuation) = continuation
            .filter(|_| {
                projected.next_action.code != "act_at_venue"
                    && !incomplete_observations
                    && approval_valid
                    && !needs_revised_bounds
                    && !settled_compound
                    && !failed_current_authority
                    && (compound
                        || projected.retry_approval
                            != Some(gofin_domain::outcome::RetryApproval::Required))
            })
            .filter(|c| c.stage_token.is_none() && c.result.is_none())
            .filter(|_| {
                compound
                    || reauthorized_failed
                    || ((record.submission.is_none()
                        || projected.dispatch == gofin_domain::outcome::Dispatch::Rejected)
                        && (latest_rejected
                            || latest_child
                                .as_ref()
                                .is_none_or(|child| child.submission.is_none())))
            })
        {
            projected.next_action = gofin_domain::outcome::NextAction {
                code: "continue".into(),
                message: "Continue this action when its next stage is ready.".into(),
                operation_id: Some(record.id.clone()),
                command: Some("operations continue".into()),
                arguments: Some(json!({"operation_id":record.id,"token":continuation.token})),
                continuation_token: Some(continuation.token),
            };
        }
        if !incomplete_observations
            && record.run_id.is_none()
            && record.approval.is_none()
            && record.submission.is_none()
            && projected.replay_safe
            && let Some(token) = self
                .store
                .lock()
                .map_err(|_| internal("store lock poisoned"))?
                .latest_preapproval_continuation(&record.id)
                .map_err(map_store)?
        {
            let expired_deadline = record.operation == "bridge.execute"
                && record
                    .input
                    .get("actionDeadline")
                    .and_then(Value::as_i64)
                    .is_some_and(|deadline| deadline <= self.clock.now().timestamp());
            let revised_request = OperationRequest {
                request_id: record.request_id.clone(),
                operation: record.operation.clone(),
                input: record.input.clone(),
                run_id: record.run_id.clone(),
            };
            let revised =
                request_fingerprint(&revised_request).ok().as_deref() != Some(&record.fingerprint);
            projected.next_action = if expired_deadline || revised {
                gofin_domain::outcome::NextAction {
                    code: "reauthorize".into(),
                    message: "Provide revised action input for a fresh approval.".into(),
                    operation_id: Some(record.id.clone()),
                    command: Some("operations reauthorize".into()),
                    arguments: Some(
                        json!({"operation_id":record.id,"token":token.token,"input_required":true}),
                    ),
                    continuation_token: Some(token.token),
                }
            } else {
                gofin_domain::outcome::NextAction {
                    code: "continue".into(),
                    message: "Continue this original action to prepare and request approval."
                        .into(),
                    operation_id: Some(record.id.clone()),
                    command: Some("operations continue".into()),
                    arguments: Some(json!({"operation_id":record.id,"token":token.token})),
                    continuation_token: Some(token.token),
                }
            };
        }
        value["outcome"] = to_value(&projected)?;
        if record
            .error
            .as_ref()
            .is_some_and(|error| error.code == "NONCE_CHANGED")
        {
            value["external_constraint"] = json!({"kind":"provider_pending_nonce",
                "message":"The provider's pending nonce differs from this candidate; its network nonce must be reconciled before this candidate can proceed."});
        }
        if let Some(child) = latest_child {
            if record.submission.is_some() {
                value["original_attempt"] = json!({"id":record.id,"status":record.status,
                    "submission":record.submission,"effects":record.effects,"preview":record.preview,
                    "approval":record.approval,"error":record.error});
            }
            let child_view = self.operation_view(&child)?;
            value["latest_attempt"] = child_view;
        }
        if matches!(
            record.operation.as_str(),
            "coinbase.deposit" | "coinbase.withdraw"
        ) && value
            .pointer("/outcome/next_action/code")
            .and_then(Value::as_str)
            == Some("wait_and_check")
        {
            value["outcome"]["next_action"]["message"] = json!(
                "Check this original operation again. For more information, consult the Coinbase App."
            );
        }
        if value
            .pointer("/outcome/next_action/code")
            .and_then(Value::as_str)
            == Some("wait_and_check")
        {
            let guidance = match record.operation.as_str() {
                "binance.withdraw" => Some(
                    "Check this original withdrawal again. For more information, consult Binance withdrawal history or support.",
                ),
                "hyperliquid.account.withdraw" => Some(
                    "Check this original withdrawal again. For more information, consult your Hyperliquid portfolio or support.",
                ),
                _ => None,
            };
            if let Some(message) = guidance {
                value["outcome"]["next_action"]["message"] = json!(message);
            }
        }
        if !attempts.is_empty() {
            value["stages"] = to_value(&attempts)?;
        }
        if self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .is_operation_dismissed(&record.id)
            .map_err(map_store)?
        {
            value["human_dismissed"] = json!(true);
        }
        if let Some(resolution) = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .operation_resolution(&record.id)
            .map_err(map_store)?
        {
            value["effective_outcome"] =
                json!({"provenance":"human_attested","outcome":resolution["outcome"]});
            value["manual_resolution"] = resolution;
        }
        Ok(value)
    }

    pub async fn status_side(&self, id: &str, side: &str) -> Result<Value> {
        if !matches!(side, "source" | "destination") {
            return Err(GoFinError::validation("Choose source or destination."));
        }
        let record = self.load_record(id)?;
        let expected = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .get_record(id)
            .map_err(map_store)?
            .ok_or_else(|| internal("operation disappeared"))?;
        let Ok(expected_typed) = from_value::<OperationRecord>(expected.clone()) else {
            return self.operation_view(&record);
        };
        if to_value(&record)? != to_value(&expected_typed)? {
            return self.operation_view(&self.load_record(id)?);
        }
        if record
            .error
            .as_ref()
            .is_some_and(|error| error.code.eq_ignore_ascii_case("history_unreadable"))
        {
            let mut view = self.operation_view(&record)?;
            view["side_observation"] = json!({"side":side,"error":{"code":"HISTORY_UNREADABLE","message":"The original operation could not be read safely."}});
            return Ok(view);
        }
        let adapter = self
            .adapters
            .read()
            .ok()
            .and_then(|a| a.get(&record.operation).cloned());
        let Some(adapter) = adapter else {
            let mut view = self.operation_view(&record)?;
            view["side_observation"] = json!({"side":side,"error":{"code":"ADAPTER_UNAVAILABLE","message":"The original adapter is unavailable."}});
            return Ok(view);
        };
        let observation = bounded_evidence(
            &record.operation,
            "transfer observation",
            FINANCIAL_EVIDENCE_TIMEOUT,
            adapter.observe_transfer_side(&record, side),
        )
        .await;
        let (record, metadata) = match observation {
            Ok(observation) => {
                let applied = if let Some(reconciliation) = observation.reconciliation {
                    self.apply_reconciliation_with_baselines(
                        record.clone(),
                        reconciliation,
                        vec![],
                        expected,
                    )
                } else if side == "destination"
                    && record
                        .preview
                        .as_ref()
                        .is_some_and(|p| adapter.is_compound_action(p))
                {
                    self.apply_compound_destination_observation(
                        record.clone(),
                        observation.metadata.clone(),
                    )
                } else {
                    self.load_record(id)
                };
                match applied {
                    Ok(updated) => (updated, observation.metadata),
                    Err(error) => (
                        self.load_record(id).unwrap_or(record),
                        json!({"side":side,"error":{"code":error.code,"stage":error.stage,"message":error.message}}),
                    ),
                }
            }
            Err(error) => (
                record,
                json!({"side":side,"error":{"code":error.code,"stage":error.stage,"message":error.message}}),
            ),
        };
        let mut view = self.operation_view(&record)?;
        view["side_observation"] = metadata;
        Ok(view)
    }

    pub fn open_operations(&self) -> Result<Vec<Value>> {
        let mut open = Vec::new();
        for record in self.operations()? {
            let is_internal_attempt = self
                .store
                .lock()
                .ok()
                .and_then(|s| s.stage_attempt_by_operation(&record.id).ok().flatten())
                .is_some_and(|stage| stage.action_id != record.id);
            if is_internal_attempt {
                continue;
            }
            let view = self.operation_view(&record)?;
            let phase = view.pointer("/outcome/phase").and_then(Value::as_str);
            let transfer_open = matches!(
                view.pointer("/outcome/transfer").and_then(Value::as_str),
                Some("source_pending" | "in_transit" | "unknown")
            );
            if (matches!(phase, Some("in_progress" | "needs_you" | "unconfirmed")) || transfer_open)
                && (view.get("human_dismissed") != Some(&json!(true)) || phase == Some("needs_you"))
            {
                open.push(view);
            }
        }
        Ok(open)
    }

    pub fn dismiss_operation(&self, id: &str) -> Result<Value> {
        let record = self.load_record(id)?;
        let view = self.operation_view(&record)?;
        if view.pointer("/outcome/phase").and_then(Value::as_str) != Some("unconfirmed") {
            return Err(GoFinError::validation(
                "Only an unconfirmed operation can be dismissed.",
            ));
        }
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .dismiss_operation(id, &self.clock.now().to_rfc3339())
            .map_err(map_store)?;
        self.operation_view(&record)
    }

    pub fn operation_recovery(&self, operation_id: &str) -> Result<Value> {
        let record = self.load_record(operation_id)?;
        let operation = self.operation_view(&record)?;
        Ok(
            json!({"operation":operation, "automatic_replay_allowed":false,
            "next_action":"Observe the original operation and follow its outcome. Do not submit it again without proof that a new attempt is safe."}),
        )
    }

    pub fn preview_operation_resolution(
        &self,
        _operation_id: &str,
        _input: &OperationResolutionInput,
    ) -> Result<Value> {
        Err(retired_execution_interface())
    }

    pub async fn resolve_operation(
        &self,
        _operation_id: &str,
        _request_id: &str,
        _expected_review_hash: &str,
        _input: &OperationResolutionInput,
    ) -> Result<Value> {
        Err(retired_execution_interface())
    }

    pub async fn recover_operation_receipt(
        &self,
        operation_id: &str,
        reference: &RecoveryReference,
    ) -> Result<Value> {
        if reference.reference.trim().is_empty() || reference.reference.len() > 512 {
            return Err(unavailable(
                "A bounded original receipt reference is required.",
            ));
        }
        let record = self.load_record(operation_id)?;
        let expected = self
            .store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .get_record(operation_id)
            .map_err(map_store)?
            .ok_or_else(|| internal("operation disappeared"))?;
        let Ok(expected_typed) = from_value::<OperationRecord>(expected.clone()) else {
            return self.operation_view(&record);
        };
        if to_value(&record)? != to_value(&expected_typed)? {
            return self.operation_view(&self.load_record(operation_id)?);
        }
        let adapter = self
            .adapters
            .read()
            .map_err(|_| internal("registry lock poisoned"))?
            .get(&record.operation)
            .cloned()
            .ok_or_else(|| unavailable("The original operation adapter is unavailable."))?;
        let reconciliation = bounded_evidence(
            &record.operation,
            "receipt observation",
            FINANCIAL_EVIDENCE_TIMEOUT,
            adapter.reconcile_with_reference(&record, reference),
        )
        .await?;
        let result =
            self.apply_reconciliation_with_baselines(record, reconciliation, vec![], expected)?;
        self.operation_view(&result)
    }
}

// Only an affirmative, correlated live-order observation renews the horizon.
// Generic pending polls may merely report a missing receipt.
fn observation_horizon_anchor<'a>(
    observations: &'a [gofin_domain::outcome::OutcomeObservation],
    facts: &gofin_domain::outcome::OutcomeFacts,
    submitted_at: &'a str,
) -> &'a str {
    if facts.dispatch == gofin_domain::outcome::Dispatch::Dispatched
        && matches!(
            facts.result,
            gofin_domain::outcome::OutcomeResult::Pending
                | gofin_domain::outcome::OutcomeResult::Partial
        )
    {
        observations
            .iter()
            .rev()
            .find(|observation| observation.source == "adapter_live_order")
            .map(|observation| observation.facts.observed_at.as_str())
            .unwrap_or(submitted_at)
    } else {
        submitted_at
    }
}

// Legacy records are projected conservatively. In particular an unavailable
// submission identity is never treated as evidence that dispatch did not occur.
pub(super) fn outcome_facts(record: &OperationRecord) -> gofin_domain::outcome::OutcomeFacts {
    use gofin_domain::outcome::{Attention, Dispatch, OutcomeFacts, OutcomeResult, Reporting};
    let legacy_whitelist = verified_legacy_binance_whitelist_rejection(record);
    let dispatch = if legacy_whitelist {
        Dispatch::Rejected
    } else {
        evidence_dispatch(record).unwrap_or_else(|| match record.status {
            Status::Failed | Status::Expired
                if record.submission.is_none()
                    && record
                        .last_reconciliation_evidence
                        .as_ref()
                        .and_then(|e| e.get("engine_predispatch_failure"))
                        .and_then(Value::as_bool)
                        == Some(true) =>
            {
                Dispatch::NotDispatched
            }
            Status::Expired
                if record.submission.is_none()
                    && record.error.as_ref().is_some_and(|error| {
                        matches!(error.stage.as_str(), "prepare" | "preview" | "validation")
                    }) =>
            {
                Dispatch::NotDispatched
            }
            Status::AwaitingApproval | Status::Cancelled if record.submission.is_none() => {
                Dispatch::NotDispatched
            }
            Status::Rejected if record.submission.is_none() => Dispatch::Rejected,
            _ if record.submission.is_some() && verified_dispatch(record) => Dispatch::Dispatched,
            _ if record.submission.is_some() => Dispatch::Uncertain,
            _ => Dispatch::Uncertain,
        })
    };
    let result = if legacy_whitelist {
        OutcomeResult::Failed
    } else {
        match record.status {
            Status::Completed => OutcomeResult::Executed,
            Status::Failed | Status::Expired => OutcomeResult::Failed,
            Status::Cancelled => OutcomeResult::Cancelled,
            Status::Unknown => OutcomeResult::Unknown,
            _ if record.effects.is_some() => OutcomeResult::Partial,
            _ => OutcomeResult::Pending,
        }
    };
    let evidence = record.last_reconciliation_evidence.as_ref();
    let attention = if legacy_whitelist {
        Attention::ActionRequired {
            venue: "Binance".into(),
            reason: "The destination is not on the withdrawal allowlist.".into(),
            where_to_act:
                "Update Binance Address Management, then obtain new approval for a new action."
                    .into(),
        }
    } else {
        match evidence
            .and_then(|e| e.get("outcome_attention"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            Some(Attention::None) | None => {
                native_execution_discrepancy(record).unwrap_or(Attention::None)
            }
            Some(attention) => attention,
        }
    };
    let transfer = evidence
        .and_then(|e| e.get("outcome_transfer"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .or_else(|| {
            (record.operation == "hyperliquid.account.withdraw" && record.submission.is_some())
                .then_some(gofin_domain::outcome::TransferState::Unknown)
        });
    let reporting = evidence
        .and_then(|e| e.get("outcome_reporting"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or(if record.effects.is_some() {
            Reporting::Partial
        } else {
            Reporting::Unavailable
        });
    let mut facts = OutcomeFacts {
        dispatch,
        result,
        transfer,
        attention,
        reporting,
        duplicate_execution_excluded: legacy_whitelist
            || !record.operation.starts_with("bridge.")
                && evidence
                    .and_then(|e| e.get("outcome_replay_safe"))
                    .and_then(Value::as_bool)
                    == Some(true),
        next_check_after: evidence
            .and_then(|e| e.get("next_check_after"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        observed_at: record.updated_at.clone(),
    };
    apply_kamino_non_dispatch_proof(record, &mut facts);
    facts
}

fn apply_kamino_non_dispatch_proof(
    record: &OperationRecord,
    facts: &mut gofin_domain::outcome::OutcomeFacts,
) {
    let is_kamino = matches!(
        record.operation.as_str(),
        "kamino.deposit"
            | "kamino.withdraw"
            | "kamino.lending.supply"
            | "kamino.lending.borrow"
            | "kamino.lending.repay"
            | "kamino.lending.withdraw"
    );
    let evidence_claim = [
        record.last_reconciliation_evidence.as_ref(),
        record.effects.as_ref().map(|e| &e.evidence),
    ]
    .into_iter()
    .flatten()
    .any(|e| {
        e.get("source").and_then(Value::as_str) == Some("kamino_verified_pre_send_rejection")
            || is_kamino && e.get("state").and_then(Value::as_str) == Some("not_dispatched")
    });
    let dispatch_claim = is_kamino
        && record
            .dispatch_evidence
            .as_ref()
            .is_some_and(|e| e.get("no_network_send").and_then(Value::as_bool) == Some(true));
    if !evidence_claim && !dispatch_claim {
        return;
    }
    use gofin_domain::outcome::{Dispatch, OutcomeResult};
    if verified_kamino_non_dispatch(record) {
        facts.dispatch = Dispatch::Rejected;
        facts.result = OutcomeResult::Failed;
        facts.duplicate_execution_excluded = true;
    } else {
        // Neither unverified tags nor an older positive outcome observation may
        // replace the original proof. Other Kamino settlement outcomes are untouched.
        facts.dispatch = if record.submission.is_some() {
            Dispatch::Dispatched
        } else {
            Dispatch::Uncertain
        };
        facts.duplicate_execution_excluded = false;
    }
}

// Older Kamino records proved non-dispatch before emitting the standard outcome
// tags. Reproject only the original empty proof and matching durable no-send
// evidence; a missing receipt, generic failure, or changed digest proves nothing.
fn verified_kamino_non_dispatch(record: &OperationRecord) -> bool {
    if record.status != Status::Failed
        || !matches!(
            record.operation.as_str(),
            "kamino.deposit"
                | "kamino.withdraw"
                | "kamino.lending.supply"
                | "kamino.lending.borrow"
                | "kamino.lending.repay"
                | "kamino.lending.withdraw"
        )
    {
        return false;
    }
    let (Some(submission), Some(verified), Some(dispatch), Some(preview)) = (
        &record.submission,
        &record.effects,
        &record.dispatch_evidence,
        &record.preview,
    ) else {
        return false;
    };
    let Some(hash) = submission
        .data
        .get("message_sha256")
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Some(reason) = dispatch
        .get("reason_code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    submission.kind == "solana.transaction"
        && hash.len() == 64
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        && submission.reference == format!("solana:message:{hash}")
        && preview.operation == record.operation
        && preview
            .binding
            .get("message_sha256")
            .and_then(Value::as_str)
            == Some(hash)
        && dispatch.get("no_network_send").and_then(Value::as_bool) == Some(true)
        && dispatch.get("message_sha256").and_then(Value::as_str) == Some(hash)
        && dispatch.get("chain_signature").is_none()
        && verified.evidence_version == 1
        && verified.submission == *submission
        && verified.effects.is_empty()
        && verified_effect_digest(verified).ok().as_deref() == Some(verified.proof_digest.as_str())
        && verified.evidence.get("state").and_then(Value::as_str) == Some("not_dispatched")
        && verified.evidence.get("reason_code").and_then(Value::as_str) == Some(reason)
        && verified
            .evidence
            .get("financial_effects")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
}

fn verified_dispatch(record: &OperationRecord) -> bool {
    let Some(submission) = record.submission.as_ref() else {
        return false;
    };
    if record.effects.as_ref().is_some_and(|proof| {
        &proof.submission == submission
            && verified_effect_digest(proof).ok().as_deref() == Some(proof.proof_digest.as_str())
    }) {
        return true;
    }
    let Some(evidence) = record.dispatch_evidence.as_ref() else {
        return false;
    };
    if evidence.get("no_network_send").and_then(Value::as_bool) == Some(true)
        || evidence.get("outcome").and_then(Value::as_str) == Some("not_submitted")
    {
        return false;
    }
    if evidence
        .get("engine_transport_accepted")
        .and_then(Value::as_bool)
        == Some(true)
        && evidence.get("submission_reference").and_then(Value::as_str)
            == Some(submission.reference.as_str())
    {
        return true;
    }
    evidence
        .get("chain_signature")
        .and_then(Value::as_str)
        .is_some_and(|signature| {
            submission
                .data
                .get("chain_signature")
                .and_then(Value::as_str)
                == Some(signature)
        })
}

fn verified_legacy_binance_whitelist_rejection(record: &OperationRecord) -> bool {
    let (Some(submission), Some(verified)) = (&record.submission, &record.effects) else {
        return false;
    };
    let evidence = &verified.evidence;
    record.operation == "binance.withdraw"
        && submission.kind == "binance"
        && !submission.reference.is_empty()
        && submission
            .data
            .pointer("/operation")
            .and_then(Value::as_str)
            == Some("binance.withdraw")
        && submission
            .data
            .pointer("/binding/withdrawOrderId")
            .and_then(Value::as_str)
            == Some(submission.reference.as_str())
        && verified.submission == *submission
        && verified.effects.is_empty()
        && verified_effect_digest(verified).ok().as_deref() == Some(verified.proof_digest.as_str())
        && evidence.get("method").and_then(Value::as_str)
            == Some("recorded-binance-withdrawal-whitelist-rejection-v1")
        && evidence.get("withdrawOrderId").and_then(Value::as_str)
            == Some(submission.reference.as_str())
        && evidence.get("submissionIdentity").and_then(Value::as_str)
            == Some(submission.reference.as_str())
        && evidence.get("httpStatus").and_then(Value::as_i64) == Some(400)
        && evidence.get("binanceCode").and_then(Value::as_i64) == Some(-4035)
        && evidence.get("historyComplete").and_then(Value::as_bool) == Some(true)
        && evidence.get("matchingHistoryRows").and_then(Value::as_u64) == Some(0)
        && evidence.get("sourceDebitObserved").and_then(Value::as_str) == Some("0")
        && evidence.get("payoutEffects").and_then(Value::as_str) == Some("none")
        && evidence.get("noEffects").and_then(Value::as_bool) == Some(true)
}

fn verified_venue_withdrawal_received(record: &OperationRecord) -> bool {
    if !matches!(
        record.operation.as_str(),
        "hyperliquid.account.withdraw" | "binance.withdraw" | "coinbase.withdraw"
    ) || record.status != Status::Completed
    {
        return false;
    }
    let (Some(submission), Some(verified), Some(preview)) =
        (&record.submission, &record.effects, &record.preview)
    else {
        return false;
    };
    if preview.operation != record.operation
        || verified.evidence_version != 1
        || verified.submission != *submission
        || verified_effect_digest(verified).ok().as_deref() != Some(verified.proof_digest.as_str())
    {
        return false;
    }
    preview.intent.expected.iter().any(|bound| {
        let EffectConstraint::Transfer { from, to, debit_asset, credit_asset, maximum_debit, minimum_credit } = bound else { return false; };
        verified.effects.iter().any(|effect| matches!(effect,
            FinancialEffect::Transfer { from: actual_from, to: actual_to, debit, credit }
            if actual_from == from && actual_to == to && &debit.asset == debit_asset && &credit.asset == credit_asset
                && debit.atoms.integer() <= maximum_debit.integer() && credit.atoms.integer() >= minimum_credit.integer()))
    })
}

fn evidence_dispatch(record: &OperationRecord) -> Option<gofin_domain::outcome::Dispatch> {
    let evidence = record.last_reconciliation_evidence.as_ref()?;
    if evidence.get("outcome_dispatch").and_then(Value::as_str) != Some("rejected")
        || evidence
            .get("definitive_nonexecution")
            .and_then(Value::as_bool)
            != Some(true)
        || evidence
            .get("source")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || evidence.get("submission_reference").and_then(Value::as_str)
            != record.submission.as_ref().map(|s| s.reference.as_str())
    {
        return None;
    }
    Some(gofin_domain::outcome::Dispatch::Rejected)
}

fn settlement_discrepancy(
    expected: &[EffectConstraint],
    effects: &[FinancialEffect],
    hl_order: bool,
) -> bool {
    expected.iter().any(|bound| {
        let EffectConstraint::Settlement { resource, asset, category, .. } = bound else { return false; };
        let mut found = false;
        let mut total = num_bigint::BigInt::default();
        let mut gross_loss = num_bigint::BigInt::default();
        for effect in effects {
            if let FinancialEffect::Settlement { resource: actual_resource, asset: actual_asset, category: actual_category, delta } = effect
                && actual_resource == resource && actual_asset == asset && actual_category == category {
                found = true;
                total += delta.integer();
                if delta.integer() < num_bigint::BigInt::from(0) { gross_loss += delta.integer(); }
            }
        }
        // Historical Hyperliquid ordinary perpetual orders split realized PnL
        // at zero and used notional as an accounting estimate for upside. It
        // was never a profit limit; retain the independent gross-loss bound.
        if hl_order && *category == gofin_domain::SettlementCategory::RealizedPnl {
            let loss = expected.iter().find_map(|candidate| match candidate {
                EffectConstraint::Settlement { resource: r, asset: a, category: c, minimum_delta, maximum_delta }
                    if r == resource && a == asset && c == category && minimum_delta.integer() <= num_bigint::BigInt::from(0) && maximum_delta.integer() == num_bigint::BigInt::from(0) => Some(minimum_delta.integer()),
                _ => None,
            });
            let upside = expected.iter().any(|candidate| matches!(candidate,
                EffectConstraint::Settlement { resource: r, asset: a, category: c, minimum_delta, maximum_delta }
                if r == resource && a == asset && c == category && minimum_delta.integer() == num_bigint::BigInt::from(0) && maximum_delta.integer() > num_bigint::BigInt::from(0)));
            if let (Some(loss), true) = (loss, upside) { return found && gross_loss < loss; }
        }
        found && !expected.iter().any(|candidate| matches!(candidate,
            EffectConstraint::Settlement { resource: other_resource, asset: other_asset, category: other_category, minimum_delta, maximum_delta }
            if other_resource == resource && other_asset == asset && other_category == category
                && total >= minimum_delta.integer() && total <= maximum_delta.integer()))
    })
}

// A discrepancy is reporting, never an execution veto. Compare only explicit
// native bounds for which the correlated proof supplies a matching actual.
fn native_execution_discrepancy(
    record: &OperationRecord,
) -> Option<gofin_domain::outcome::Attention> {
    use gofin_domain::outcome::Attention;
    let terminal = record.status == Status::Completed;
    if !terminal
        && !matches!(
            record.status,
            Status::Pending | Status::Unknown | Status::Failed
        )
    {
        return None;
    }
    let intent = &record.preview.as_ref()?.intent;
    let effects = &record.effects.as_ref()?.effects;
    let costs = aggregate_costs(effects).ok()?;
    let caps = aggregate_cost_bounds(&intent.maximum_costs).ok()?;
    let uncapped = intent
        .maximum_costs
        .iter()
        .filter(|c| c.treatment == CostTreatment::VenueCalculatedAdditionalDebit)
        .filter_map(|c| cost_key(&c.location, &c.amount.asset, &c.treatment).ok())
        .collect::<BTreeSet<_>>();
    let cost_breach = costs.iter().any(|(key, amount)| {
        !uncapped.contains(key) && amount > caps.get(key).unwrap_or(&BigUint::zero())
    });
    let scoped_cost_breach = (|| {
        let proof = record.effects.as_ref()?;
        if record.submission.as_ref() != Some(&proof.submission)
            || verified_effect_digest(proof).ok()?.as_str() != proof.proof_digest
        {
            return None;
        }
        let partition = proof.evidence.get("newFinancialScope")?;
        let scope_value = record
            .preview
            .as_ref()?
            .binding
            .get("mutationNewWriteScope")?;
        if partition.get("scope")? != scope_value {
            return None;
        }
        let scope: gofin_domain::amendments::AmendmentScope =
            from_value(scope_value.clone()).ok()?;
        let scoped: Vec<FinancialEffect> = from_value(partition.get("effects")?.clone()).ok()?;
        let mut available = proof.effects.clone();
        for effect in &scoped {
            let index = available.iter().position(|actual| actual == effect)?;
            available.remove(index);
        }
        let actual = aggregate_costs(&scoped).ok()?;
        let approved = aggregate_cost_bounds(&scope.intent.maximum_costs).ok()?;
        actual
            .iter()
            .any(|(key, amount)| amount > approved.get(key).unwrap_or(&BigUint::zero()))
            .then(|| (to_value(&scoped).ok(), to_value(&scope.intent).ok()))
    })();
    let earn_cost_breach = (|| {
        let proof = record.effects.as_ref()?;
        let submission = record.submission.as_ref()?;
        if submission != &proof.submission
            || verified_effect_digest(proof).ok()?.as_str() != proof.proof_digest
        {
            return None;
        }
        let partition = proof.evidence.get("earnFeeScope")?;
        let binding = record.preview.as_ref()?.binding.get("earn")?;
        if partition.get("binding")? != binding
            || submission.data.pointer("/binding/earn")? != binding
        {
            return None;
        }
        let asset: gofin_domain::AssetId = from_value(binding.get("asset")?.clone()).ok()?;
        let maximum: gofin_domain::Quantity = from_value(binding.get("max_fee")?.clone()).ok()?;
        let scoped: Vec<FinancialEffect> = from_value(partition.get("effects")?.clone()).ok()?;
        let mut available = proof.effects.clone();
        let mut total = BigUint::zero();
        for effect in &scoped {
            let index = available.iter().position(|actual| actual == effect)?;
            available.remove(index);
            let FinancialEffect::Cost {
                amount, category, ..
            } = effect
            else {
                return None;
            };
            if amount.asset != asset || category != "kraken_earn_fee" {
                return None;
            }
            total += amount.atoms.integer();
        }
        (total > maximum.integer()).then(|| (to_value(&scoped).ok(), binding.clone()))
    })();
    let transfer_breach = terminal
        && intent.expected.iter().any(|bound| {
            let EffectConstraint::Transfer {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } = bound
            else {
                return false;
            };
            effects.iter().any(|effect| match effect {
                FinancialEffect::Transfer {
                    from: actual_from,
                    to: actual_to,
                    debit,
                    credit,
                } if actual_from == from
                    && actual_to == to
                    && &debit.asset == debit_asset
                    && &credit.asset == credit_asset =>
                {
                    debit.atoms.integer() > maximum_debit.integer()
                        || credit.atoms.integer() < minimum_credit.integer()
                }
                _ => false,
            })
        });
    let native_breach = terminal
        && intent.expected.iter().any(|bound| match bound {
            EffectConstraint::Debit {
                resource,
                asset,
                maximum_debit,
            } => {
                let mut found = false;
                let total = effects
                    .iter()
                    .filter_map(|effect| match effect {
                        FinancialEffect::CashDebit {
                            resource: r,
                            amount,
                        } if r == resource && &amount.asset == asset => {
                            found = true;
                            Some(amount.atoms.integer())
                        }
                        _ => None,
                    })
                    .fold(BigUint::zero(), |sum, amount| sum + amount);
                found && total > maximum_debit.integer()
            }
            EffectConstraint::Conversion {
                resource,
                input,
                maximum_input,
                output,
                minimum_output,
            } => {
                let mut found = false;
                let mut spent = BigUint::zero();
                let mut received = BigUint::zero();
                for effect in effects {
                    if let FinancialEffect::Conversion {
                        resource: r,
                        spent: s,
                        received: d,
                    } = effect
                        && r == resource
                        && &s.asset == input
                        && &d.asset == output
                    {
                        found = true;
                        spent += s.atoms.integer();
                        received += d.atoms.integer();
                    }
                }
                found && (spent > maximum_input.integer() || received < minimum_output.integer())
            }
            EffectConstraint::Position {
                resource,
                position,
                asset,
                direction,
                maximum_change,
            }
            | EffectConstraint::PositionOfKind {
                resource,
                position,
                asset,
                direction,
                maximum_change,
                ..
            } => {
                let mut total = num_bigint::BigInt::from(0);
                let mut wrong_direction = false;
                for effect in effects {
                    match effect {
                        FinancialEffect::PositionChange {
                            resource: r,
                            position: p,
                            asset: a,
                            delta,
                            anchor,
                            ..
                        } if r == resource && p == position && a == asset => {
                            let kind_ok = match bound {
                                EffectConstraint::PositionOfKind { position_kind, .. } => anchor
                                    .as_ref()
                                    .is_some_and(|a| &a.position_kind == position_kind),
                                _ => true,
                            };
                            let value = delta.integer();
                            let zero = num_bigint::BigInt::from(0);
                            if kind_ok {
                                match direction {
                                    gofin_domain::Direction::Increase => {
                                        wrong_direction |= value < zero;
                                        total += value;
                                    }
                                    gofin_domain::Direction::Decrease => {
                                        wrong_direction |= value > zero;
                                        total -= value;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                wrong_direction || total > num_bigint::BigInt::from(maximum_change.integer())
            }
            _ => false,
        });
    let settlement_breach = terminal
        && settlement_discrepancy(
            &intent.expected,
            effects,
            matches!(
                record.operation.as_str(),
                "hyperliquid.orders.place"
                    | "hyperliquid.orders.batch"
                    | "hyperliquid.orders.edit"
                    | "hyperliquid.orders.edit_batch"
            ),
        );
    let denomination_breach = terminal
        && intent.expected.iter().any(|bound| {
            let EffectConstraint::CostDenomination {
                location,
                cost_asset,
                category,
                treatment,
                denomination_asset,
                maximum_amount,
            } = bound
            else {
                return false;
            };
            let mut total = BigUint::zero();
            for effect in effects {
                if let FinancialEffect::CostDenomination {
                    location: actual_location,
                    cost_asset: actual_cost_asset,
                    category: actual_category,
                    treatment: actual_treatment,
                    denomination,
                    ..
                } = effect
                    && actual_location == location
                    && actual_cost_asset == cost_asset
                    && actual_category == category
                    && actual_treatment == treatment
                    && &denomination.asset == denomination_asset
                {
                    total += denomination.atoms.integer();
                }
            }
            total > maximum_amount.integer()
        });
    if let Some((Some(actual), Some(approved))) = scoped_cost_breach {
        return Some(Attention::Discrepancy { actual, approved });
    }
    if let Some((Some(actual), approved)) = earn_cost_breach {
        return Some(Attention::Discrepancy { actual, approved });
    }
    if cost_breach || transfer_breach || native_breach || settlement_breach || denomination_breach {
        Some(Attention::Discrepancy {
            actual: to_value(effects).ok()?,
            approved: to_value(intent).ok()?,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod outcome_projection_tests {
    use super::*;
    #[test]
    fn native_discrepancies_compare_aggregate_debits_conversions_and_positions() {
        let a = json!({"network":"synthetic","reference":"A","decimals":6});
        let b = json!({"network":"synthetic","reference":"B","decimals":6});
        let submission = json!({"kind":"synthetic","reference":"one","data":{}});
        let mut record: OperationRecord = serde_json::from_value(json!({
            "id":"o","request_id":"r","fingerprint":"f","fingerprint_version":1,
            "operation":"synthetic","input":{},"status":"completed","created_at":now(),"updated_at":now(),
            "preview":{"operation":"synthetic","input":{},"binding":{},"summary":"s","checks":[],"warnings":[],"observed_at":now(),"expires_at":"2099-01-01T00:00:00Z","operation_version":1,
                "intent":{"debits":[],"locks":[],"maximum_costs":[],"expected":[]}},
            "approval":null,"submission":submission,"effects":{"evidence_version":1,"submission":submission,"effects":[],"evidence":{},"observed_at":now(),"proof_digest":"x"},
            "last_reconciliation_evidence":null,"error":null,"run_id":null
        })).unwrap();
        let cases = [
            (
                json!({"kind":"debit","resource":"wallet","asset":a,"maximum_debit":"10"}),
                json!([{"kind":"cash_debit","resource":"wallet","amount":{"asset":a,"atoms":"6"}},{"kind":"cash_debit","resource":"wallet","amount":{"asset":a,"atoms":"5"}}]),
            ),
            (
                json!({"kind":"conversion","resource":"wallet","input":a,"maximum_input":"10","output":b,"minimum_output":"9"}),
                json!([{"kind":"conversion","resource":"wallet","spent":{"asset":a,"atoms":"10"},"received":{"asset":b,"atoms":"8"}}]),
            ),
            (
                json!({"kind":"position","resource":"wallet","position":"p","asset":a,"direction":"increase","maximum_change":"10"}),
                json!([{"kind":"position_change","resource":"wallet","position":"p","asset":a,"delta":"6","scaled_receipt":null,"anchor":null},{"kind":"position_change","resource":"wallet","position":"p","asset":a,"delta":"5","scaled_receipt":null,"anchor":null}]),
            ),
        ];
        for (bound, effects) in cases {
            record.preview.as_mut().unwrap().intent.expected =
                serde_json::from_value(json!([bound])).unwrap();
            record.effects.as_mut().unwrap().effects = serde_json::from_value(effects).unwrap();
            assert!(native_execution_discrepancy(&record).is_some());
        }
        let cost = json!({"kind":"cost","location":{"kind":"cash","resource":"wallet"},"amount":{"asset":a,"atoms":"6"},"category":"fee","treatment":"additional_debit"});
        let bound = |atoms: &str| json!({"location":{"kind":"cash","resource":"wallet"},"amount":{"asset":a,"atoms":atoms},"treatment":"additional_debit"});
        let preview = record.preview.as_mut().unwrap();
        preview.intent.expected.clear();
        preview.intent.maximum_costs = serde_json::from_value(json!([bound("10")])).unwrap();
        let scope = json!({"intent":{"debits":[],"locks":[],"expected":[],"maximum_costs":[bound("5")]},"binding":{},"valuation":null});
        preview.binding["mutationNewWriteScope"] = scope.clone();
        let proof = record.effects.as_mut().unwrap();
        proof.effects = serde_json::from_value(json!([cost])).unwrap();
        proof.evidence = json!({"newFinancialScope":{"scope":scope,"effects":[cost]}});
        proof.proof_digest = verified_effect_digest(proof).unwrap();
        assert!(
            native_execution_discrepancy(&record).is_some(),
            "sub-scope fee overrun must be visible even within the parent cap"
        );
        record.effects.as_mut().unwrap().evidence["newFinancialScope"]["scope"]["binding"] =
            json!({"changed":true});
        let proof = record.effects.as_mut().unwrap();
        proof.proof_digest = verified_effect_digest(proof).unwrap();
        assert!(
            native_execution_discrepancy(&record).is_none(),
            "an unrelated scope must not create a discrepancy"
        );
        let earn = json!({"asset":a,"max_fee":"10"});
        let fees = json!([
            {"kind":"cost","location":{"kind":"cash","resource":"wallet"},"amount":{"asset":a,"atoms":"6"},"category":"kraken_earn_fee","treatment":"additional_debit"},
            {"kind":"cost","location":{"kind":"position","resource":"earn","position":"p","position_kind":"supply"},"amount":{"asset":a,"atoms":"6"},"category":"kraken_earn_fee","treatment":"additional_debit"}
        ]);
        record.preview.as_mut().unwrap().binding = json!({"earn":earn});
        record.preview.as_mut().unwrap().intent.maximum_costs = serde_json::from_value(json!([bound("20"),{"location":{"kind":"position","resource":"earn","position":"p","position_kind":"supply"},"amount":{"asset":a,"atoms":"20"},"treatment":"additional_debit"}])).unwrap();
        record.submission.as_mut().unwrap().data = json!({"binding":{"earn":earn}});
        let proof = record.effects.as_mut().unwrap();
        proof.submission = record.submission.clone().unwrap();
        proof.effects = serde_json::from_value(fees.clone()).unwrap();
        proof.evidence = json!({"earnFeeScope":{"binding":earn,"effects":fees}});
        proof.proof_digest = verified_effect_digest(proof).unwrap();
        assert!(
            native_execution_discrepancy(&record).is_some(),
            "the aggregate Earn fee cap applies across locations"
        );
        record.effects.as_mut().unwrap().evidence = json!({});
        let proof = record.effects.as_mut().unwrap();
        proof.proof_digest = verified_effect_digest(proof).unwrap();
        assert!(
            native_execution_discrepancy(&record).is_none(),
            "the separate costs each fit their parent bounds"
        );
    }
    struct NoApproval;
    #[async_trait]
    impl gofin_domain::Authorizer for NoApproval {
        async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            panic!("read-only")
        }
    }
    #[test]
    fn ambiguous_polls_do_not_extend_the_observation_horizon() {
        use gofin_domain::outcome::{Dispatch, OutcomeObservation, OutcomeResult};
        let record: OperationRecord = serde_json::from_value(json!({"id":"order","request_id":"r","fingerprint":"f","fingerprint_version":1,"operation":"test.order","input":{},"status":"pending","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-20T00:00:00Z","preview":null,"approval":null,"submission":null,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null})).unwrap();
        let mut facts = outcome_facts(&record);
        facts.dispatch = Dispatch::Dispatched;
        facts.result = OutcomeResult::Pending;
        let start = "2026-01-01T00:00:00Z";
        let observations = vec![OutcomeObservation {
            facts: facts.clone(),
            source: "adapter".into(),
        }];
        assert_eq!(
            observation_horizon_anchor(&observations, &facts, start),
            start
        );
        let mut live = facts.clone();
        live.observed_at = "2026-01-10T00:00:00Z".into();
        let observations = vec![
            OutcomeObservation {
                facts: live.clone(),
                source: "adapter_live_order".into(),
            },
            OutcomeObservation {
                facts,
                source: "adapter".into(),
            },
        ];
        assert_eq!(
            observation_horizon_anchor(&observations, &live, start),
            live.observed_at
        );
        live.result = OutcomeResult::Unknown;
        assert_eq!(
            observation_horizon_anchor(&observations, &live, start),
            start
        );
    }
    #[test]
    fn later_adapter_observation_clears_venue_action_required() {
        use gofin_domain::outcome::{Attention, Dispatch, OutcomeFacts, OutcomeResult, Reporting};
        let record: OperationRecord = serde_json::from_value(json!({
            "id":"attention", "request_id":"r", "fingerprint":"f", "fingerprint_version":1,
            "operation":"test.withdraw", "input":{}, "status":"pending", "created_at":now(),
            "updated_at":now(), "preview":null, "approval":null, "submission":null,
            "effects":null, "last_reconciliation_evidence":null, "error":null, "run_id":null
        }))
        .unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                &record.id,
                &record.request_id,
                &record.fingerprint,
                &record.operation,
                None,
                "pending",
                &to_value(&record).unwrap(),
                &record.created_at,
            )
            .unwrap();
        let mut facts = OutcomeFacts {
            dispatch: Dispatch::Uncertain,
            result: OutcomeResult::Pending,
            transfer: None,
            attention: Attention::ActionRequired {
                venue: "venue".into(),
                reason: "confirm".into(),
                where_to_act: "email".into(),
            },
            reporting: Reporting::Unavailable,
            duplicate_execution_excluded: false,
            next_check_after: None,
            observed_at: now(),
        };
        store
            .append_outcome_observation(&record.id, &facts, "adapter")
            .unwrap();
        facts.attention = Attention::None;
        store
            .append_outcome_observation(&record.id, &facts, "adapter")
            .unwrap();
        let engine = Engine::new(
            store,
            vec![],
            Arc::new(NoApproval),
            EngineOptions::default(),
        )
        .unwrap();
        let outcome = &engine.operation_view(&record).unwrap()["outcome"];
        assert_eq!(outcome["attention"]["kind"], "none");
        assert_eq!(outcome["phase"], "in_progress");
    }
    #[test]
    fn persisted_engine_discrepancy_is_recomputed_without_erasing_proof() {
        use gofin_domain::outcome::{Attention, Dispatch, OutcomeFacts, OutcomeResult, Reporting};
        let asset = json!({"network":"synthetic","reference":"USD","decimals":6});
        let preview: PreparedOperation = serde_json::from_value(json!({"operation":"hyperliquid.close","input":{},"binding":{},"summary":"close","checks":[],"warnings":[],"observed_at":"2026-01-01T00:00:00Z","expires_at":"2099-01-01T00:00:00Z","operation_version":1,"intent":{"debits":[],"locks":[],"maximum_costs":[],"expected":[{"kind":"settlement","resource":"wallet","asset":asset,"category":"realized_pnl","minimum_delta":"-100000","maximum_delta":"0"},{"kind":"settlement","resource":"wallet","asset":asset,"category":"realized_pnl","minimum_delta":"0","maximum_delta":"11000000"}]}})).unwrap();
        let submission = Submission {
            kind: "synthetic".into(),
            reference: "original".into(),
            data: json!({}),
        };
        let effects: Vec<FinancialEffect> = serde_json::from_value(json!([{"kind":"settlement","resource":"wallet","asset":asset,"category":"realized_pnl","delta":"5720"}])).unwrap();
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: json!({}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified).unwrap();
        let record: OperationRecord = serde_json::from_value(json!({"id":"close","request_id":"r","fingerprint":"f","fingerprint_version":1,"operation":"hyperliquid.close","input":{},"status":"completed","created_at":now(),"updated_at":now(),"preview":preview,"approval":null,"submission":submission,"effects":verified,"last_reconciliation_evidence":null,"error":null,"run_id":null})).unwrap();
        let mut pending_record = record.clone();
        pending_record.status = Status::Pending;
        pending_record.effects = None;
        pending_record.last_reconciliation_evidence =
            Some(json!({"canonical_confirmed":false,"next_check_after":now()}));
        assert_eq!(outcome_facts(&pending_record).dispatch, Dispatch::Uncertain);
        pending_record.effects = record.effects.clone();
        assert_eq!(
            outcome_facts(&pending_record).dispatch,
            Dispatch::Dispatched
        );
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                &record.id,
                &record.request_id,
                &record.fingerprint,
                &record.operation,
                None,
                "completed",
                &to_value(&record).unwrap(),
                &record.created_at,
            )
            .unwrap();
        let facts = OutcomeFacts {
            dispatch: Dispatch::Dispatched,
            result: OutcomeResult::Executed,
            transfer: None,
            attention: Attention::Discrepancy {
                actual: to_value(&record.effects.as_ref().unwrap().effects).unwrap(),
                approved: to_value(&record.preview.as_ref().unwrap().intent).unwrap(),
            },
            reporting: Reporting::Partial,
            duplicate_execution_excluded: false,
            next_check_after: None,
            observed_at: now(),
        };
        let mut pending = facts.clone();
        pending.result = OutcomeResult::Pending;
        pending.transfer = Some(gofin_domain::outcome::TransferState::SourcePending);
        pending.reporting = Reporting::Complete;
        store
            .append_outcome_observation(&record.id, &pending, "adapter")
            .unwrap();
        store
            .append_outcome_observation(&record.id, &facts, "adapter")
            .unwrap();
        let engine = Engine::new(
            store,
            vec![],
            Arc::new(NoApproval),
            EngineOptions::default(),
        )
        .unwrap();
        pending_record.effects = None;
        assert_eq!(
            engine.operation_view(&pending_record).unwrap()["outcome"]["dispatch"],
            "uncertain"
        );
        let view = engine.operation_view(&record).unwrap();
        assert_eq!(view["outcome"]["attention"]["kind"], "none");
        assert_eq!(view["outcome"]["phase"], "done");
        assert_eq!(view["outcome"]["reporting"], "complete");
        assert_eq!(
            view["effects"],
            to_value(record.effects.as_ref().unwrap()).unwrap()
        );
        let mut outside = record.clone();
        if let FinancialEffect::Settlement { delta, .. } =
            &mut outside.effects.as_mut().unwrap().effects[0]
        {
            *delta = gofin_domain::SignedQuantity::new("-100001").unwrap();
        }
        let proof = outside.effects.as_mut().unwrap();
        proof.proof_digest = verified_effect_digest(proof).unwrap();
        let mut stored = facts;
        stored.attention = Attention::Discrepancy {
            actual: to_value(&outside.effects.as_ref().unwrap().effects).unwrap(),
            approved: to_value(&outside.preview.as_ref().unwrap().intent).unwrap(),
        };
        engine
            .store
            .lock()
            .unwrap()
            .append_outcome_observation(&record.id, &stored, "adapter")
            .unwrap();
        assert_eq!(
            engine.operation_view(&outside).unwrap()["outcome"]["attention"]["kind"],
            "discrepancy"
        );
        stored.dispatch = Dispatch::Uncertain;
        stored.result = OutcomeResult::Unknown;
        engine
            .store
            .lock()
            .unwrap()
            .append_outcome_observation(&record.id, &stored, "adapter")
            .unwrap();
        assert_eq!(
            engine.operation_view(&outside).unwrap()["outcome"]["dispatch"],
            "dispatched"
        );
        let mut transfer_record = record.clone();
        transfer_record.id = "pending-transfer".into();
        transfer_record.request_id = "pending-request".into();
        let mut pending = stored.clone();
        pending.dispatch = Dispatch::Dispatched;
        pending.result = OutcomeResult::Pending;
        pending.transfer = Some(gofin_domain::outcome::TransferState::InTransit);
        pending.attention = Attention::None;
        let mut unknown = pending.clone();
        unknown.dispatch = Dispatch::Uncertain;
        unknown.result = OutcomeResult::Unknown;
        unknown.transfer = Some(gofin_domain::outcome::TransferState::SourcePending);
        {
            let mut store = engine.store.lock().unwrap();
            store
                .create_request(
                    &transfer_record.id,
                    &transfer_record.request_id,
                    &transfer_record.fingerprint,
                    &transfer_record.operation,
                    None,
                    "pending",
                    &to_value(&transfer_record).unwrap(),
                    &transfer_record.created_at,
                )
                .unwrap();
            store
                .append_outcome_observation(&transfer_record.id, &pending, "adapter")
                .unwrap();
            store
                .append_outcome_observation(&transfer_record.id, &unknown, "adapter")
                .unwrap();
        }
        let transfer_view = engine.operation_view(&transfer_record).unwrap();
        assert_eq!(transfer_view["outcome"]["result"], "pending");
        assert_eq!(transfer_view["outcome"]["transfer"], "in_transit");
    }
    #[test]
    fn settlement_discrepancy_accepts_either_matching_signed_interval() {
        let resource = ResourceId("wallet".into());
        let asset = gofin_domain::AssetId {
            network: "synthetic".into(),
            reference: "USD".into(),
            decimals: 6,
        };
        let bound = |min: &str, max: &str| EffectConstraint::Settlement {
            resource: resource.clone(),
            asset: asset.clone(),
            category: gofin_domain::SettlementCategory::RealizedPnl,
            minimum_delta: gofin_domain::SignedQuantity::new(min).unwrap(),
            maximum_delta: gofin_domain::SignedQuantity::new(max).unwrap(),
        };
        let effects = |amount: &str| {
            vec![FinancialEffect::Settlement {
                resource: resource.clone(),
                asset: asset.clone(),
                category: gofin_domain::SettlementCategory::RealizedPnl,
                delta: gofin_domain::SignedQuantity::new(amount).unwrap(),
            }]
        };
        let both = vec![bound("-100000", "0"), bound("0", "11000000")];
        assert!(!settlement_discrepancy(&both, &effects("5720"), false));
        assert!(!settlement_discrepancy(&both, &effects("-100000"), false));
        assert!(settlement_discrepancy(&both, &effects("-100001"), false));
        assert!(settlement_discrepancy(&both, &effects("11000001"), false));
        assert!(settlement_discrepancy(
            &[bound("-100000", "0")],
            &effects("5720"),
            false
        ));
        assert!(!settlement_discrepancy(&both, &effects("11000001"), true));
        let mut gain_and_loss = effects("11000001");
        gain_and_loss.extend(effects("-100001"));
        assert!(settlement_discrepancy(&both, &gain_and_loss, true));
    }
    #[test]
    fn historical_kamino_non_dispatch_requires_original_empty_proof() {
        let hash = "a".repeat(64);
        let mut record: OperationRecord = serde_json::from_value(json!({
            "id":"kamino-no-send", "request_id":"r", "fingerprint":"f", "fingerprint_version":1,
            "operation":"kamino.lending.borrow", "input":{}, "status":"failed",
            "created_at":"2026-11-01T00:00:00Z", "updated_at":"2026-11-01T00:00:00Z",
            "preview":{"operation":"kamino.lending.borrow","input":{},"binding":{"message_sha256":hash},
                "summary":"borrow","checks":[],"warnings":[],"observed_at":"2026-11-01T00:00:00Z",
                "expires_at":"2026-11-01T00:01:00Z","operation_version":1,"intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":[]}},
            "approval":null,
            "submission":{"kind":"solana.transaction","reference":format!("solana:message:{hash}"),"data":{"message_sha256":hash,"chain_signature":"precomputed-wallet-signature","wallet_signature":"precomputed-wallet-signature"}},
            "dispatch_evidence":{"no_network_send":true,"message_sha256":hash,"reason_code":"SOLANA_RPC_RATE_LIMITED"},
            "effects":null, "last_reconciliation_evidence":null, "error":null, "run_id":null
        })).unwrap();
        let mut proof = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: vec![],
            evidence: json!({"state":"not_dispatched","reason_code":"SOLANA_RPC_RATE_LIMITED","reason":"no send","financial_effects":[]}),
            observed_at: record.updated_at.clone(),
            proof_digest: String::new(),
        };
        proof.proof_digest = verified_effect_digest(&proof).unwrap();
        record.effects = Some(proof);
        let facts = outcome_facts(&record);
        assert_eq!(facts.dispatch, gofin_domain::outcome::Dispatch::Rejected);
        assert!(facts.project(false, false).replay_safe);

        // Persisted older outcome observations must not conceal the stronger
        // original proof after upgrading; projection does not mutate or submit.
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![],
            Arc::new(NoApproval),
            EngineOptions::default(),
        )
        .unwrap();
        {
            let mut store = engine.store.lock().unwrap();
            store
                .create_request(
                    &record.id,
                    &record.request_id,
                    &record.fingerprint,
                    &record.operation,
                    None,
                    "failed",
                    &to_value(&record).unwrap(),
                    &record.created_at,
                )
                .unwrap();
            let mut old = outcome_facts(&record);
            old.dispatch = gofin_domain::outcome::Dispatch::Dispatched;
            old.duplicate_execution_excluded = false;
            store
                .append_outcome_observation(&record.id, &old, "adapter")
                .unwrap();
        }
        let view = engine.operation_view(&record).unwrap();
        assert_eq!(view["outcome"]["dispatch"], "rejected");
        assert_eq!(view["outcome"]["replay_safe"], true);
        assert_eq!(view["outcome"]["retry_approval"], "required");

        // New records carry optimistic outcome tags as well as a proof. Neither
        // those tags nor a stored positive observation can substitute for it.
        let mut tagged = record.clone();
        let proof = tagged.effects.as_mut().unwrap();
        proof.evidence["outcome_dispatch"] = json!("rejected");
        proof.evidence["outcome_replay_safe"] = json!(true);
        proof.evidence["definitive_nonexecution"] = json!(true);
        proof.evidence["source"] = json!("kamino_verified_pre_send_rejection");
        proof.evidence["submission_reference"] = json!(proof.submission.reference);
        proof.proof_digest = verified_effect_digest(proof).unwrap();
        tagged.last_reconciliation_evidence = Some(proof.evidence.clone());
        engine
            .store
            .lock()
            .unwrap()
            .append_outcome_observation(&record.id, &outcome_facts(&tagged), "adapter")
            .unwrap();
        assert_eq!(
            engine.operation_view(&tagged).unwrap()["outcome"]["replay_safe"],
            true
        );
        for (path, value) in [
            ("/effects/proof_digest", json!("tampered")),
            ("/effects", Value::Null),
            ("/dispatch_evidence", Value::Null),
            ("/preview", Value::Null),
            ("/submission/reference", json!("other")),
            ("/operation", json!("wallet.transfer")),
        ] {
            let mut changed = to_value(&tagged).unwrap();
            *changed.pointer_mut(path).unwrap() = value;
            let changed: OperationRecord = serde_json::from_value(changed).unwrap();
            assert!(
                !outcome_facts(&changed).project(false, false).replay_safe,
                "tagged {path}"
            );
            let view = engine.operation_view(&changed).unwrap();
            assert_eq!(view["outcome"]["dispatch"], "dispatched", "stored {path}");
            assert_eq!(view["outcome"]["replay_safe"], false, "stored {path}");
            assert_eq!(
                view["outcome"]["next_action"]["code"], "wait_and_check",
                "stored {path}"
            );
        }
        let mut other_failure = tagged;
        other_failure.dispatch_evidence = None;
        other_failure.effects.as_mut().unwrap().evidence = json!({"state":"finalized_failure"});
        let proof = other_failure.effects.as_mut().unwrap();
        proof.proof_digest = verified_effect_digest(proof).unwrap();
        other_failure.last_reconciliation_evidence =
            Some(json!({"source":"finalized_receipt","outcome_replay_safe":true}));
        let other = outcome_facts(&other_failure);
        assert_eq!(other.dispatch, gofin_domain::outcome::Dispatch::Dispatched);
        assert!(other.project(false, false).replay_safe);

        for (path, value) in [
            ("/effects", Value::Null),
            ("/dispatch_evidence", Value::Null),
            ("/preview", Value::Null),
            ("/status", json!("unknown")),
            ("/operation", json!("wallet.transfer")),
            ("/submission/kind", json!("other")),
            ("/submission/reference", json!("solana:message:other")),
            ("/dispatch_evidence/no_network_send", json!(false)),
            ("/dispatch_evidence/message_sha256", json!("b".repeat(64))),
            ("/dispatch_evidence/reason_code", json!("PREVIEW_EXPIRED")),
            ("/preview/binding/message_sha256", json!("b".repeat(64))),
            (
                "/effects/evidence/state",
                json!("expired_without_inclusion"),
            ),
            ("/effects/evidence/financial_effects", json!([{}])),
            ("/effects/evidence_version", json!(0)),
            (
                "/effects/effects",
                json!([{"kind":"cost","location":{"kind":"cash","resource":"wallet"},"amount":{"asset":{"network":"solana:mainnet-beta","reference":"native","decimals":9},"atoms":"1"},"category":"network_fee","treatment":"additional_debit"}]),
            ),
            (
                "/effects/submission/reference",
                json!("solana:message:other"),
            ),
        ] {
            let mut changed = to_value(&record).unwrap();
            *changed.pointer_mut(path).unwrap() = value;
            let mut changed: OperationRecord = serde_json::from_value(changed).unwrap();
            if let Some(proof) = changed.effects.as_mut() {
                proof.proof_digest = verified_effect_digest(proof).unwrap();
            }
            assert!(
                !outcome_facts(&changed).project(false, false).replay_safe,
                "{path}"
            );
        }
        let mut changed = record.clone();
        changed.effects.as_mut().unwrap().proof_digest = "tampered".into();
        assert!(!outcome_facts(&changed).project(false, false).replay_safe);
        let mut changed = record.clone();
        changed.dispatch_evidence.as_mut().unwrap()["chain_signature"] = json!("a recorded send");
        assert!(!outcome_facts(&changed).project(false, false).replay_safe);
        let mut changed = record;
        changed.submission.as_mut().unwrap().data["chain_signature"] = json!("a recorded send");
        assert!(!outcome_facts(&changed).project(false, false).replay_safe);
    }
    #[test]
    fn legacy_binance_whitelist_rejection_requires_exact_verified_evidence() {
        let mut record: OperationRecord = serde_json::from_value(json!({
            "id":"withdraw", "request_id":"r", "fingerprint":"f", "fingerprint_version":1,
            "operation":"binance.withdraw", "input":{}, "status":"failed",
            "created_at":"2026-11-01T00:00:00Z", "updated_at":"2026-11-01T00:00:00Z",
            "preview":null, "approval":null,
            "submission":{"kind":"binance","reference":"order-1","data":{"operation":"binance.withdraw","binding":{"withdrawOrderId":"order-1"}}},
            "effects":null, "last_reconciliation_evidence":null, "error":null, "run_id":null
        })).unwrap();
        let mut proof = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: vec![],
            evidence: json!({"method":"recorded-binance-withdrawal-whitelist-rejection-v1","withdrawOrderId":"order-1","submissionIdentity":"order-1","httpStatus":400,"binanceCode":-4035,"historyComplete":true,"matchingHistoryRows":0,"sourceDebitObserved":"0","payoutEffects":"none","noEffects":true}),
            observed_at: record.updated_at.clone(),
            proof_digest: String::new(),
        };
        proof.proof_digest = verified_effect_digest(&proof).unwrap();
        record.effects = Some(proof);
        let outcome = outcome_facts(&record).project(false, false);
        assert_eq!(outcome.dispatch, gofin_domain::outcome::Dispatch::Rejected);
        assert!(outcome.replay_safe);
        assert!(matches!(
            outcome.attention,
            gofin_domain::outcome::Attention::ActionRequired { .. }
        ));
        record.effects.as_mut().unwrap().evidence["withdrawOrderId"] = json!("different");
        assert!(!outcome_facts(&record).project(false, false).replay_safe);
    }
    fn assert_historical_venue_withdrawal_projection(operation: &str) {
        let asset = json!({"network":"synthetic","reference":"USD","decimals":6});
        let preview: PreparedOperation = serde_json::from_value(json!({"operation":operation,"input":{},"binding":{},"summary":"withdraw","checks":[],"warnings":[],"observed_at":"2026-01-01T00:00:00Z","expires_at":"2099-01-01T00:00:00Z","operation_version":1,"intent":{"debits":[],"locks":[],"maximum_costs":[],"expected":[{"kind":"transfer","from":"source","to":"destination","debit_asset":asset,"credit_asset":asset,"maximum_debit":"100","minimum_credit":"90"}]}})).unwrap();
        let submission = Submission {
            kind: "synthetic".into(),
            reference: "original".into(),
            data: json!({}),
        };
        let effects: Vec<FinancialEffect> = serde_json::from_value(json!([{"kind":"transfer","from":"source","to":"destination","debit":{"asset":asset,"atoms":"100"},"credit":{"asset":asset,"atoms":"95"}}])).unwrap();
        let mut proof = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: json!({}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        proof.proof_digest = verified_effect_digest(&proof).unwrap();
        let mut record: OperationRecord = serde_json::from_value(json!({"id":"withdraw","request_id":"r","fingerprint":"f","fingerprint_version":1,"operation":operation,"input":{},"status":"completed","created_at":now(),"updated_at":now(),"preview":preview,"approval":null,"submission":submission,"effects":proof,"last_reconciliation_evidence":null,"error":null,"run_id":null})).unwrap();
        assert!(verified_venue_withdrawal_received(&record));
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![],
            Arc::new(NoApproval),
            EngineOptions::default(),
        )
        .unwrap();
        {
            let mut store = engine.store.lock().unwrap();
            store
                .create_request(
                    &record.id,
                    &record.request_id,
                    &record.fingerprint,
                    &record.operation,
                    None,
                    "completed",
                    &to_value(&record).unwrap(),
                    &record.created_at,
                )
                .unwrap();
            let mut pending = outcome_facts(&record);
            pending.result = gofin_domain::outcome::OutcomeResult::Pending;
            pending.transfer = Some(gofin_domain::outcome::TransferState::SourcePending);
            store
                .append_outcome_observation(&record.id, &pending, "adapter")
                .unwrap();
            store
                .append_outcome_observation(&record.id, &outcome_facts(&record), "adapter")
                .unwrap();
        }
        let view = engine.operation_view(&record).unwrap();
        assert_eq!(
            view["outcome"]["transfer"],
            if operation == "hyperliquid.account.withdraw" {
                "available"
            } else {
                "credited"
            }
        );
        assert_eq!(view["outcome"]["phase"], "done");
        assert_eq!(view["outcome"]["replay_safe"], false);
        // The refactor clears stale progress on completed records. Invalid
        // receipt proof must not manufacture a credited/available transfer tag.
        for (path, value) in [
            ("/effects/effects/0/to", json!("different-recipient")),
            ("/effects/effects/0/credit/atoms", json!("89")),
            (
                "/effects/effects/0/credit/asset/reference",
                json!("different-asset"),
            ),
            (
                "/effects/submission/reference",
                json!("different-submission"),
            ),
        ] {
            let mut changed = to_value(&record).unwrap();
            *changed.pointer_mut(path).unwrap() = value;
            let mut changed: OperationRecord = serde_json::from_value(changed).unwrap();
            let proof = changed.effects.as_mut().unwrap();
            proof.proof_digest = verified_effect_digest(proof).unwrap();
            assert!(
                !verified_venue_withdrawal_received(&changed),
                "{operation}: {path}"
            );
            assert_eq!(
                engine.operation_view(&changed).unwrap()["outcome"]["transfer"],
                if operation == "hyperliquid.account.withdraw" {
                    json!("source_pending")
                } else {
                    Value::Null
                },
                "{operation}: {path}"
            );
        }
        record.effects.as_mut().unwrap().submission.reference = "different".into();
        assert!(!verified_venue_withdrawal_received(&record));
        assert_eq!(
            engine.operation_view(&record).unwrap()["outcome"]["transfer"],
            if operation == "hyperliquid.account.withdraw" {
                json!("source_pending")
            } else {
                Value::Null
            }
        );
    }
    #[test]
    fn historical_hyperliquid_withdrawal_requires_correlated_verified_transfer() {
        assert_historical_venue_withdrawal_projection("hyperliquid.account.withdraw");
        assert_historical_venue_withdrawal_projection("binance.withdraw");
    }
    #[test]
    fn historical_coinbase_withdrawal_requires_correlated_verified_transfer() {
        assert_historical_venue_withdrawal_projection("coinbase.withdraw");
    }
    #[test]
    fn expired_preparation_is_failed_and_legacy_expiry_remains_uncertain() {
        let mut record: OperationRecord = serde_json::from_value(json!({
            "id":"expired", "request_id":"r", "fingerprint":"f", "fingerprint_version":1,
            "operation":"swap.execute", "input":{}, "status":"expired",
            "created_at":"2026-11-01T00:00:00Z", "updated_at":"2026-11-01T00:00:00Z",
            "preview":null, "approval":null, "submission":null, "effects":null,
            "last_reconciliation_evidence":null,
            "error":{"code":"SOLANA_TX_EXPIRED","stage":"prepare","message":"expired","next_action":"prepare again"}, "run_id":null
        })).unwrap();
        let outcome = outcome_facts(&record).project(false, false);
        assert_eq!(outcome.phase, gofin_domain::outcome::Phase::Failed);
        assert_eq!(
            outcome.dispatch,
            gofin_domain::outcome::Dispatch::NotDispatched
        );
        assert_eq!(outcome.result, gofin_domain::outcome::OutcomeResult::Failed);
        assert!(outcome.replay_safe);
        assert_eq!(outcome.next_action.code, "retry");
        assert_eq!(
            outcome.retry_approval,
            Some(gofin_domain::outcome::RetryApproval::Required)
        );
        record.error = None;
        let legacy = outcome_facts(&record).project(false, false);
        assert_eq!(legacy.dispatch, gofin_domain::outcome::Dispatch::Uncertain);
        assert!(!legacy.replay_safe);
    }
    #[test]
    fn journal_attested_predispatch_failure_is_replay_safe_but_legacy_failure_is_not() {
        let mut record: OperationRecord = serde_json::from_value(json!({
            "id":"failed", "request_id":"r", "fingerprint":"f", "fingerprint_version":1,
            "operation":"kraken.withdraw-funds", "input":{}, "status":"failed",
            "created_at":"2026-11-01T00:00:00Z", "updated_at":"2026-11-01T00:00:00Z",
            "preview":null, "approval":null, "submission":null, "effects":null,
            "last_reconciliation_evidence":{"engine_predispatch_failure":true},
            "error":{"code":"KRAKEN_DUPLICATE_CUSTODY_ALIAS","stage":"kraken","message":"failed","next_action":"fix account"}, "run_id":null
        })).unwrap();
        let outcome = outcome_facts(&record).project(false, false);
        assert_eq!(
            outcome.dispatch,
            gofin_domain::outcome::Dispatch::NotDispatched
        );
        assert!(outcome.replay_safe);
        assert_eq!(
            outcome.retry_approval,
            Some(gofin_domain::outcome::RetryApproval::Required)
        );
        record.last_reconciliation_evidence = None;
        let legacy = outcome_facts(&record).project(false, false);
        assert_eq!(legacy.dispatch, gofin_domain::outcome::Dispatch::Uncertain);
        assert!(!legacy.replay_safe);
    }
    #[test]
    fn missing_submission_in_legacy_unknown_is_not_replay_safe() {
        let record: OperationRecord = serde_json::from_value(json!({
            "id":"legacy", "request_id":"r", "fingerprint":"f", "fingerprint_version":1,
            "operation":"test.write", "input":{}, "status":"unknown",
            "created_at":"2026-11-01T00:00:00Z", "updated_at":"2026-11-01T00:00:00Z",
            "preview":null, "approval":null, "submission":null, "effects":null,
            "last_reconciliation_evidence":null, "error":null, "run_id":null
        }))
        .unwrap();
        let projected = outcome_facts(&record).project(false, false);
        assert!(!projected.replay_safe);
        assert_eq!(
            projected.dispatch,
            gofin_domain::outcome::Dispatch::Uncertain
        );
        let aged = outcome_facts(&record).project(true, false);
        assert_eq!(aged.phase, gofin_domain::outcome::Phase::Unconfirmed);
        assert!(!aged.replay_safe);
    }
}
