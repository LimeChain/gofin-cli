use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gofin_domain::product_names::MetadataValueExt;
use gofin_domain::{
    Adapter, ConnectionMetadata, Definition, GoFinError, OperationKind, OperationRecord,
    PreparationContext, PreparedOperation, Reconciliation, ReturnRouteContext, ReturnRouteProof,
    RevalidatedOperation, StateSnapshot, Status, Submission, SubmissionJournal,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// A profile constructs the immutable durable identity and a transient request.
/// Only ExchangeAdapter may sequence persistence and dispatch through Adapter.
pub struct PlannedSubmission<R> {
    pub submission: Submission,
    pub request: R,
}

fn error(code: &str, stage: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        stage,
        message,
        "Inspect the supported operation and reconcile any persisted submission before retrying.",
    )
}

/// Exchange-specific mappings, financial interpretation and evidence. This is
/// deliberately not an Adapter: authorization-sensitive lifecycle is shared.
#[async_trait]
pub trait ExchangeProfile: Send + Sync {
    type Request: Send + Sync;
    fn attach_execution_state(
        &self,
        _state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        Ok(())
    }

    fn definitions(&self) -> Vec<Definition>;
    fn submission_kind(&self) -> &str;
    fn connections(&self) -> Vec<ConnectionMetadata> {
        Vec::new()
    }
    /// Stable protected identities used by exact financial-plan approval.
    /// Keep identities here even when they are omitted from public discovery
    /// metadata, and do not collapse entries by display name.
    fn authorization_identities(&self) -> Vec<ConnectionMetadata> {
        self.connections()
    }
    fn prepare_boundaries(
        &self,
        _prepared: &PreparedOperation,
        _bounds: &Value,
    ) -> Result<gofin_domain::boundaries::BoundarySpec, GoFinError> {
        Err(error(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "authorization",
            "This exchange profile has no implemented boundary authorization.",
        ))
    }
    /// Derive a balance-independent scope for one explicitly authorized plan
    /// step. Profiles opt in per operation and must not use projected balances
    /// or short-lived quote data to claim future funding.
    async fn prepare_plan_permission(
        &self,
        _operation: &str,
        _input: &Value,
        _bounds: &Value,
    ) -> Result<gofin_domain::financial_plans::PlanPermission, GoFinError> {
        Err(error(
            "PLAN_PERMISSION_UNSUPPORTED",
            "authorization",
            "This exchange profile does not support named financial plan permissions.",
        ))
    }
    /// Balance-independent plan admission. Protected read-only fee, permission,
    /// route and metadata queries are allowed; no mutations, reservations or
    /// checks requiring future funding. Warn explicitly about deferred checks.
    async fn validate_plan_feasibility(
        &self,
        _operation: &str,
        _input: &Value,
    ) -> Result<Vec<String>, GoFinError> {
        Ok(vec![
            "Exchange-specific feasibility checks are deferred until execution.".into(),
        ])
    }
    fn validate_boundary_candidate(
        &self,
        _spec: &gofin_domain::boundaries::BoundarySpec,
        _prepared: &PreparedOperation,
    ) -> Result<(), GoFinError> {
        Err(error(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "authorization",
            "This exchange profile has no implemented boundary authorization.",
        ))
    }
    async fn snapshot(&self) -> Result<StateSnapshot, GoFinError> {
        Err(error(
            "SNAPSHOT_UNAVAILABLE",
            "snapshot",
            "This exchange profile has no complete financial observer.",
        ))
    }
    async fn snapshot_scoped(
        &self,
        scope: &gofin_domain::SnapshotScope,
    ) -> Result<StateSnapshot, GoFinError> {
        if scope.is_full() {
            self.snapshot().await
        } else {
            Err(error(
                "SNAPSHOT_SCOPE_UNSUPPORTED",
                "snapshot",
                "This exchange profile has no scoped financial observer.",
            ))
        }
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value, GoFinError>;
    async fn prepare(
        &self,
        _operation: &str,
        _input: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "preview",
            "This profile supports public reads only.",
        ))
    }
    fn validate_recovery_child(
        &self,
        _prepared: &PreparedOperation,
        _parent: &OperationRecord,
    ) -> Result<Option<gofin_domain::RecoveryBinding>, GoFinError> {
        Ok(None)
    }
    async fn prepare_with_compound_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
        _linked: &gofin_domain::compound_mutations::CompoundAmendmentContext,
    ) -> Result<PreparedOperation, GoFinError> {
        self.prepare_with_linked_context(
            operation,
            input,
            context,
            &gofin_domain::amendments::AmendmentContext::default(),
        )
        .await
    }
    async fn prepare_with_linked_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
        _linked: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<PreparedOperation, GoFinError> {
        self.prepare_with_context(operation, input, context).await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &PreparationContext,
    ) -> Result<PreparedOperation, GoFinError> {
        self.prepare(operation, input).await
    }
    async fn refresh_prepared(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, GoFinError> {
        self.prepare(&prepared.operation, &prepared.input).await
    }
    async fn finalize_after_reservation(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, GoFinError> {
        Ok(prepared.clone())
    }
    async fn preflight_return_route(
        &self,
        _operation: &str,
        _input: &Value,
        _context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, GoFinError> {
        Err(error(
            "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE",
            "budget",
            "This exchange profile cannot prove this return route.",
        ))
    }
    fn valuation_matches(
        &self,
        approved: &PreparedOperation,
        refreshed: &PreparedOperation,
    ) -> bool {
        // Profiles with writes must explicitly define their exact valuation contract.
        approved.valuation.is_none() && refreshed.valuation.is_none()
    }
    async fn plan_submission(
        &self,
        _current: &PreparedOperation,
    ) -> Result<PlannedSubmission<Self::Request>, GoFinError> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "submission",
            "This profile has no supported write.",
        ))
    }
    /// Carry the journal to the transport so queued/throttled requests recheck
    /// authorization immediately before signing and network dispatch.
    async fn dispatch(
        &self,
        _request: &Self::Request,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value, GoFinError> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "submission",
            "This profile has no supported write.",
        ))
    }
    fn validate_response(
        &self,
        _submission: &Submission,
        _response: &Value,
    ) -> Result<(), GoFinError> {
        Err(error(
            "WRONG_OPERATION_KIND",
            "submission",
            "This profile has no supported write.",
        ))
    }
    async fn reconcile_submission(
        &self,
        _record: &OperationRecord,
        _submission: &Submission,
    ) -> Result<Reconciliation, GoFinError> {
        Ok(unknown(
            "This exchange profile has no supported reconciliation operation.",
        ))
    }
    async fn reconcile_submission_with_compound_context(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        _context: &gofin_domain::compound_mutations::CompoundAmendmentContext,
    ) -> Result<Reconciliation, GoFinError> {
        self.reconcile_submission(record, submission).await
    }
    async fn reconcile_submission_with_context(
        &self,
        record: &OperationRecord,
        submission: &Submission,
        _context: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<Reconciliation, GoFinError> {
        self.reconcile_submission(record, submission).await
    }
}

fn unknown(message: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":message}),
        verified: None,
    }
}

#[derive(Clone)]
pub struct ExchangeAdapter<P> {
    profile: P,
}
impl<P: ExchangeProfile> ExchangeAdapter<P> {
    pub fn new(profile: P) -> Self {
        Self { profile }
    }
    pub fn profile(&self) -> &P {
        &self.profile
    }
    fn require_kind(
        &self,
        operation: &str,
        kind: OperationKind,
        stage: &str,
    ) -> Result<(), GoFinError> {
        match self
            .profile
            .definitions()
            .into_iter()
            .find(|d| d.name == operation)
        {
            Some(definition) if definition.kind == kind => Ok(()),
            Some(_) => Err(error(
                "WRONG_OPERATION_KIND",
                stage,
                "The operation does not support this execution path.",
            )),
            None => Err(error(
                "UNSUPPORTED_OPERATION",
                stage,
                "The selected exchange operation is not implemented.",
            )),
        }
    }

    fn require_configuration(&self, operation: &str, stage: &str) -> Result<(), GoFinError> {
        match self
            .profile
            .definitions()
            .into_iter()
            .find(|definition| definition.name == operation)
        {
            Some(definition)
                if definition.kind == OperationKind::Configuration
                    && definition.execution_actor
                        == gofin_domain::ExecutionActor::HumanInteractive =>
            {
                Ok(())
            }
            Some(_) => Err(error(
                "WRONG_OPERATION_KIND",
                stage,
                "The operation is not a human-only configuration operation.",
            )),
            None => Err(error(
                "UNSUPPORTED_OPERATION",
                stage,
                "The selected exchange operation is not implemented.",
            )),
        }
    }
}

#[async_trait]
impl<P: ExchangeProfile> Adapter for ExchangeAdapter<P> {
    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        self.profile.attach_execution_state(state)
    }
    async fn prepare_with_compound_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
        linked: &gofin_domain::compound_mutations::CompoundAmendmentContext,
    ) -> Result<PreparedOperation, GoFinError> {
        self.require_kind(operation, OperationKind::Write, "preview")?;
        self.profile
            .prepare_with_compound_context(operation, input, context, linked)
            .await
    }
    async fn reconcile_with_compound_context(
        &self,
        record: &OperationRecord,
        context: &gofin_domain::compound_mutations::CompoundAmendmentContext,
    ) -> Result<Reconciliation, GoFinError> {
        let Some(submission) = &record.submission else {
            return Ok(unknown("No persisted exchange submission identity exists."));
        };
        if submission.kind != self.profile.submission_kind()
            || self
                .require_kind(&record.operation, OperationKind::Write, "reconciliation")
                .is_err()
        {
            return Ok(unknown(
                "Original exchange identity is not supported by this profile.",
            ));
        }
        self.profile
            .reconcile_submission_with_compound_context(record, submission, context)
            .await
    }
    fn definitions(&self) -> Vec<Definition> {
        self.profile.definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.profile.connections()
    }
    fn authorization_identities(&self) -> Vec<ConnectionMetadata> {
        self.profile.authorization_identities()
    }
    async fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<gofin_domain::boundaries::BoundarySpec, GoFinError> {
        self.require_kind(&prepared.operation, OperationKind::Write, "authorization")?;
        self.profile.prepare_boundaries(prepared, bounds)
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<gofin_domain::financial_plans::PlanPermission, GoFinError> {
        self.require_kind(operation, OperationKind::Write, "authorization")?;
        self.profile
            .prepare_plan_permission(operation, input, bounds)
            .await
    }
    async fn validate_plan_feasibility(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<Vec<String>, GoFinError> {
        self.require_kind(operation, OperationKind::Write, "planning")?;
        self.profile
            .validate_plan_feasibility(operation, input)
            .await
    }
    fn validate_boundary_candidate(
        &self,
        spec: &gofin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<(), GoFinError> {
        self.profile.validate_boundary_candidate(spec, prepared)
    }
    fn snapshot_identities(&self) -> Vec<ConnectionMetadata> {
        // Routing destinations belong to authorization, not venue observation.
        self.profile.connections()
    }

    async fn snapshot(&self) -> Result<StateSnapshot, GoFinError> {
        self.profile.snapshot().await
    }
    async fn snapshot_scoped(
        &self,
        scope: &gofin_domain::SnapshotScope,
    ) -> Result<StateSnapshot, GoFinError> {
        self.profile.snapshot_scoped(scope).await
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value, GoFinError> {
        self.require_kind(operation, OperationKind::Read, "read")?;
        self.profile.read(operation, input).await
    }
    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        self.require_kind(operation, OperationKind::Write, "preview")?;
        self.profile.prepare(operation, input).await
    }
    fn validate_recovery_child(
        &self,
        prepared: &PreparedOperation,
        parent: &OperationRecord,
    ) -> Result<Option<gofin_domain::RecoveryBinding>, GoFinError> {
        self.profile.validate_recovery_child(prepared, parent)
    }
    async fn prepare_configuration(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        self.require_configuration(operation, "configuration")?;
        self.profile.prepare(operation, input).await
    }
    async fn prepare_with_linked_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
        linked: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<PreparedOperation, GoFinError> {
        self.require_kind(operation, OperationKind::Write, "preview")?;
        self.profile
            .prepare_with_linked_context(operation, input, context, linked)
            .await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> Result<PreparedOperation, GoFinError> {
        self.require_kind(operation, OperationKind::Write, "preview")?;
        self.profile
            .prepare_with_context(operation, input, context)
            .await
    }
    async fn preflight_return_route(
        &self,
        operation: &str,
        input: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, GoFinError> {
        self.require_kind(operation, OperationKind::Write, "budget")?;
        self.profile
            .preflight_return_route(operation, input, context)
            .await
    }
    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, GoFinError> {
        self.require_kind(&prepared.operation, OperationKind::Write, "submission")?;
        if prepared.binding.get("preparationOnly") == Some(&json!(true)) {
            return Err(error(
                "PREPARATION_ONLY",
                "submission",
                "Conditional funding preparation cannot be revalidated or submitted.",
            ));
        }
        let expires = DateTime::parse_from_rfc3339(&prepared.expires_at).map_err(|_| {
            error(
                "PREVIEW_INVALID",
                "submission",
                "Prepared operation expiry is invalid.",
            )
        })?;
        if expires.with_timezone(&Utc) <= Utc::now() {
            if let Some(metadata) = prepared
                .binding
                .product_get(gofin_domain::boundaries::BOUNDARY_METADATA_KEY)?
            {
                return Err(GoFinError::new("PREVIEW_EXPIRED", "submission",
                    "The candidate preview expired; the financial authorization has its own lifetime.",
                    "Prepare a fresh candidate under the same authorization if it is still active; nothing was submitted.")
                    .with_details(json!({"constraint":"candidate_freshness","authorization_id":metadata["authorization_id"],"reauthorization_required":false,"reconciliation_required":false})));
            }
            return Err(error(
                "PREVIEW_EXPIRED",
                "submission",
                "Prepared operation expired; obtain a fresh preview and approval.",
            ));
        }
        let mut current = self.profile.refresh_prepared(prepared).await?;
        gofin_domain::product_names::preserve_approved_spelling(
            &prepared.binding,
            &mut current.binding,
        )?;
        if current.operation != prepared.operation
            || current.binding != prepared.binding
            || !self.profile.valuation_matches(prepared, &current)
        {
            if let Some(metadata) = prepared
                .binding
                .product_get(gofin_domain::boundaries::BOUNDARY_METADATA_KEY)?
            {
                return Err(GoFinError::new("PREVIEW_CHANGED", "submission",
                    "Revalidation changed this candidate's exact execution or evidence.",
                    "Prepare a new candidate under the same authorization; fresh Touch ID is needed only if its boundaries must change.")
                    .with_details(json!({"constraint":"candidate_binding","authorization_id":metadata["authorization_id"],"reauthorization_required":false,"reconciliation_required":false})));
            }
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "Material exchange parameters or valuation changed; obtain fresh authorization.",
            ));
        }
        Ok(current)
    }
    async fn finalize_after_reservation(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, GoFinError> {
        self.profile.finalize_after_reservation(prepared).await
    }
    async fn revalidate_configuration(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, GoFinError> {
        self.require_configuration(&prepared.operation, "configuration")?;
        if prepared.binding.get("preparationOnly") == Some(&json!(true)) {
            return Err(error(
                "PREPARATION_ONLY",
                "configuration",
                "A conditional funding preparation cannot be submitted as configuration.",
            ));
        }
        let expires = DateTime::parse_from_rfc3339(&prepared.expires_at).map_err(|_| {
            error(
                "PREVIEW_INVALID",
                "configuration",
                "Prepared configuration expiry is invalid.",
            )
        })?;
        if expires.with_timezone(&Utc) <= Utc::now() {
            return Err(error(
                "PREVIEW_EXPIRED",
                "configuration",
                "Prepared configuration expired; review a fresh candidate.",
            ));
        }
        let mut current = self.profile.refresh_prepared(prepared).await?;
        gofin_domain::product_names::preserve_approved_spelling(
            &prepared.binding,
            &mut current.binding,
        )?;
        if current.operation != prepared.operation
            || current.binding != prepared.binding
            || !self.profile.valuation_matches(prepared, &current)
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "configuration",
                "Configuration parameters changed after human review.",
            ));
        }
        Ok(current)
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        let current = self.revalidate(prepared).await?;
        let validated = RevalidatedOperation::after_revalidation(
            current,
            Arc::new(gofin_domain::clock::SystemClock),
        )?;
        self.submit_revalidated(&validated, journal).await
    }
    async fn submit_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        let current = validated.prepared();
        self.require_kind(&current.operation, OperationKind::Write, "submission")?;
        validated.assert_fresh()?;
        journal.assert_candidate(current)?;
        let plan = self.profile.plan_submission(current).await?;
        if plan.submission.kind != self.profile.submission_kind()
            || plan.submission.reference.is_empty()
        {
            return Err(error(
                "SUBMISSION_IDENTITY_INVALID",
                "submission",
                "The exchange profile did not produce a durable submission identity.",
            ));
        }
        validated.assert_fresh()?;
        journal.assert_authorized()?;
        journal.persist_submission(&plan.submission)?;
        // No retries here: any response loss leaves this exact persisted identity
        // available for profile-specific reconciliation through the shared engine.
        let response = self.profile.dispatch(&plan.request, journal).await?;
        self.profile
            .validate_response(&plan.submission, &response)?;
        Ok(plan.submission)
    }
    async fn submit_configuration(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        self.require_configuration(&prepared.operation, "configuration")?;
        let current = self.revalidate_configuration(prepared).await?;
        let validated = RevalidatedOperation::after_revalidation(
            current,
            Arc::new(gofin_domain::clock::SystemClock),
        )?;
        self.submit_configuration_revalidated(&validated, journal)
            .await
    }
    async fn submit_configuration_revalidated(
        &self,
        validated: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, GoFinError> {
        let current = validated.prepared();
        self.require_configuration(&current.operation, "configuration")?;
        validated.assert_fresh()?;
        journal.assert_candidate(current)?;
        let plan = self.profile.plan_submission(current).await?;
        if plan.submission.kind != self.profile.submission_kind()
            || plan.submission.reference.is_empty()
        {
            return Err(error(
                "SUBMISSION_IDENTITY_INVALID",
                "configuration",
                "The exchange profile did not produce a durable configuration submission identity.",
            ));
        }
        validated.assert_fresh()?;
        journal.assert_authorized()?;
        journal.persist_submission(&plan.submission)?;
        let response = self.profile.dispatch(&plan.request, journal).await?;
        self.profile
            .validate_response(&plan.submission, &response)?;
        Ok(plan.submission)
    }
    async fn reconcile_with_context(
        &self,
        record: &OperationRecord,
        context: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<Reconciliation, GoFinError> {
        let Some(submission) = &record.submission else {
            return Ok(unknown("No persisted exchange submission identity exists."));
        };
        if submission.kind != self.profile.submission_kind()
            || self
                .require_kind(&record.operation, OperationKind::Write, "reconciliation")
                .is_err()
        {
            return Ok(unknown(
                "Original exchange identity is not supported by this profile.",
            ));
        }
        self.profile
            .reconcile_submission_with_context(record, submission, context)
            .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, GoFinError> {
        let Some(submission) = &record.submission else {
            return Ok(unknown("No persisted exchange submission identity exists."));
        };
        if submission.kind != self.profile.submission_kind() {
            return Ok(unknown(
                "Persisted submission kind does not match this exchange profile.",
            ));
        }
        if self
            .require_kind(&record.operation, OperationKind::Write, "reconciliation")
            .is_err()
        {
            return Ok(unknown("Unsupported exchange reconciliation operation."));
        }
        self.profile.reconcile_submission(record, submission).await
    }
    async fn reconcile_configuration(
        &self,
        record: &OperationRecord,
    ) -> Result<Reconciliation, GoFinError> {
        self.require_configuration(&record.operation, "configuration")?;
        let Some(submission) = &record.submission else {
            return Ok(unknown(
                "No persisted configuration submission identity exists.",
            ));
        };
        if submission.kind != self.profile.submission_kind() {
            return Ok(unknown(
                "Persisted submission kind does not match this exchange profile.",
            ));
        }
        self.profile.reconcile_submission(record, submission).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use gofin_domain::{ConnectionMetadata, ExecutionActor, FinancialIntent, ResourceId, now};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    struct Fixture {
        events: Arc<Mutex<Vec<&'static str>>>,
        changed: AtomicBool,
        timeout: bool,
    }
    fn preview() -> PreparedOperation {
        PreparedOperation {
            operation: "fixture.transfer".into(),
            input: json!({"amount":"0.000000000000000001"}),
            binding: json!({"amount":"0.000000000000000001","clientId":"legacy-request-id"}),
            summary: "Synthetic only".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: (Utc::now() + Duration::seconds(90)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        }
    }
    #[async_trait]
    impl ExchangeProfile for Fixture {
        type Request = ();
        fn submission_kind(&self) -> &str {
            "fixture"
        }
        fn definitions(&self) -> Vec<Definition> {
            vec![
                Definition {
                    name: "fixture.transfer".into(),
                    description: "Synthetic".into(),
                    kind: OperationKind::Write,
                    execution_actor: ExecutionActor::AgentInvocable,
                    input_schema: json!({}),
                    output_schema: json!({}),
                    error_schema: json!({}),
                    examples: vec![],
                    coverage: "synthetic".into(),
                    completion: "evidence".into(),
                    semantics: json!({}),
                },
                Definition {
                    name: "fixture.configure".into(),
                    description: "Synthetic human configuration".into(),
                    kind: OperationKind::Configuration,
                    execution_actor: ExecutionActor::HumanInteractive,
                    input_schema: json!({}),
                    output_schema: json!({}),
                    error_schema: json!({}),
                    examples: vec![],
                    coverage: "synthetic".into(),
                    completion: "verified synthetic configuration".into(),
                    semantics: json!({}),
                },
            ]
        }
        fn connections(&self) -> Vec<ConnectionMetadata> {
            vec![ConnectionMetadata {
                name: "fixture account".into(),
                provider: "fixture".into(),
                resource: ResourceId("fixture:account:1".into()),
                role: Some("exchange-account".into()),
                parent: None,
                network: Some("fixture-net".into()),
                address: Some("account-1".into()),
                credential_ref: Some("fixture:key".into()),
                signing_enabled: true,
                details: json!({"stable_uid":"fixture-uid-1"}),
            }]
        }
        async fn read(&self, _: &str, _: &Value) -> Result<Value, GoFinError> {
            panic!("write profile cannot be read")
        }
        async fn prepare(
            &self,
            operation: &str,
            _: &Value,
        ) -> Result<PreparedOperation, GoFinError> {
            self.events.lock().unwrap().push("prepare");
            let mut prepared = preview();
            prepared.operation = operation.into();
            if self.changed.load(Ordering::SeqCst) {
                prepared.binding["amount"] = json!("2");
            }
            Ok(prepared)
        }
        async fn plan_submission(
            &self,
            current: &PreparedOperation,
        ) -> Result<PlannedSubmission<()>, GoFinError> {
            self.events.lock().unwrap().push("plan");
            Ok(PlannedSubmission {
                submission: Submission {
                    kind: "fixture".into(),
                    reference: current.binding["clientId"].as_str().unwrap().into(),
                    data: current.binding.clone(),
                },
                request: (),
            })
        }
        async fn dispatch(
            &self,
            _: &(),
            journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Value, GoFinError> {
            journal.assert_authorized()?;
            self.events.lock().unwrap().push("dispatch");
            if self.timeout {
                Err(error(
                    "VENUE_TIMEOUT",
                    "submission",
                    "Synthetic lost response",
                ))
            } else {
                Ok(json!({"accepted":true}))
            }
        }
        fn validate_response(&self, _: &Submission, _: &Value) -> Result<(), GoFinError> {
            Ok(())
        }
        async fn reconcile_submission(
            &self,
            _: &OperationRecord,
            submission: &Submission,
        ) -> Result<Reconciliation, GoFinError> {
            self.events.lock().unwrap().push("reconcile");
            Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"identity":submission.reference}),
                verified: None,
            })
        }
    }
    struct Journal {
        events: Arc<Mutex<Vec<&'static str>>>,
        persisted: Mutex<Vec<Submission>>,
        reject: bool,
        fail_persist: bool,
        expire_after_persist: bool,
    }
    impl SubmissionJournal for Journal {
        fn assert_authorized(&self) -> Result<(), GoFinError> {
            self.events.lock().unwrap().push("authorize");
            if self.reject
                || (self.expire_after_persist && !self.persisted.lock().unwrap().is_empty())
            {
                Err(error(
                    "AUTHORIZATION_EXPIRED",
                    "submission",
                    "Synthetic expired approval",
                ))
            } else {
                Ok(())
            }
        }
        fn persist_submission(&self, submission: &Submission) -> Result<(), GoFinError> {
            self.events.lock().unwrap().push("persist");
            if self.fail_persist {
                return Err(error(
                    "JOURNAL_UNAVAILABLE",
                    "submission",
                    "Synthetic journal failure",
                ));
            }
            self.persisted.lock().unwrap().push(submission.clone());
            Ok(())
        }
    }
    fn fixture(
        timeout: bool,
        reject: bool,
        fail_persist: bool,
    ) -> (ExchangeAdapter<Fixture>, Arc<Journal>) {
        let events = Arc::new(Mutex::new(vec![]));
        (
            ExchangeAdapter::new(Fixture {
                events: events.clone(),
                changed: AtomicBool::new(false),
                timeout,
            }),
            Arc::new(Journal {
                events,
                persisted: Mutex::new(vec![]),
                reject,
                fail_persist,
                expire_after_persist: false,
            }),
        )
    }

    #[test]
    fn exchange_adapter_forwards_profile_default_authorization_identities() {
        let (adapter, _) = fixture(false, false, false);
        assert_eq!(
            adapter.authorization_identities(),
            adapter.connections(),
            "the profile default should use its stable enrolled identities"
        );
    }

    #[tokio::test]
    async fn common_lifecycle_persists_exact_identity_before_one_dispatch() {
        let (adapter, journal) = fixture(false, false, false);
        let result = adapter.submit(&preview(), journal.clone()).await.unwrap();
        assert_eq!(
            &*journal.events.lock().unwrap(),
            &[
                "prepare",
                "authorize",
                "plan",
                "authorize",
                "persist",
                "authorize",
                "dispatch"
            ]
        );
        assert_eq!(result.reference, "legacy-request-id");
        assert_eq!(result.data["amount"], "0.000000000000000001");
        assert_eq!(&*journal.persisted.lock().unwrap(), &[result]);
    }
    #[tokio::test]
    async fn revalidated_handoff_refreshes_once_and_preserves_dispatch_gates() {
        for (reject, fail_persist) in [(false, false), (true, false), (false, true)] {
            let (adapter, journal) = fixture(false, reject, fail_persist);
            let fresh = adapter.revalidate(&preview()).await.unwrap();
            let validated = RevalidatedOperation::after_revalidation(
                fresh,
                Arc::new(gofin_domain::clock::SystemClock),
            )
            .unwrap();
            let result = adapter
                .submit_revalidated(&validated, journal.clone())
                .await;
            let events = journal.events.lock().unwrap();
            assert_eq!(events.iter().filter(|e| **e == "prepare").count(), 1);
            assert_eq!(events.contains(&"dispatch"), !reject && !fail_persist);
            assert_eq!(result.is_ok(), !reject && !fail_persist);
            if result.is_ok() {
                assert!(
                    events.iter().position(|e| *e == "persist")
                        < events.iter().position(|e| *e == "dispatch")
                );
            }
        }
    }
    #[tokio::test]
    async fn approval_and_journal_failures_never_dispatch() {
        for (reject, fail_persist, expected) in [
            (true, false, "AUTHORIZATION_EXPIRED"),
            (false, true, "JOURNAL_UNAVAILABLE"),
        ] {
            let (adapter, journal) = fixture(false, reject, fail_persist);
            assert_eq!(
                adapter
                    .submit(&preview(), journal.clone())
                    .await
                    .unwrap_err()
                    .code,
                expected
            );
            assert!(!journal.events.lock().unwrap().contains(&"dispatch"));
        }
    }

    #[tokio::test]
    async fn configuration_lifecycle_is_explicitly_kind_gated_and_journaled() {
        let (adapter, journal) = fixture(false, false, false);
        let mut candidate = preview();
        candidate.operation = "fixture.configure".into();

        assert_eq!(
            adapter
                .prepare("fixture.configure", &json!({}))
                .await
                .unwrap_err()
                .code,
            "WRONG_OPERATION_KIND"
        );
        assert_eq!(
            adapter
                .prepare_configuration("fixture.transfer", &json!({}))
                .await
                .unwrap_err()
                .code,
            "WRONG_OPERATION_KIND"
        );

        let prepared = adapter
            .prepare_configuration("fixture.configure", &json!({}))
            .await
            .unwrap();
        assert_eq!(prepared.operation, "fixture.configure");
        let submitted = adapter
            .submit_configuration(&candidate, journal.clone())
            .await
            .unwrap();
        assert_eq!(submitted.reference, "legacy-request-id");
        assert_eq!(journal.persisted.lock().unwrap().as_slice(), &[submitted]);
        assert!(adapter.submit(&candidate, journal.clone()).await.is_err());
        assert_eq!(
            &*journal.events.lock().unwrap(),
            &[
                "prepare",
                "prepare",
                "authorize",
                "plan",
                "authorize",
                "persist",
                "authorize",
                "dispatch"
            ]
        );
    }
    #[tokio::test]
    async fn authorization_expiring_after_persistence_blocks_dispatch_and_preserves_identity() {
        let (adapter, mut journal) = fixture(false, false, false);
        Arc::get_mut(&mut journal).unwrap().expire_after_persist = true;
        assert_eq!(
            adapter
                .submit(&preview(), journal.clone())
                .await
                .unwrap_err()
                .code,
            "AUTHORIZATION_EXPIRED"
        );
        assert!(!journal.events.lock().unwrap().contains(&"dispatch"));
        let persisted = journal.persisted.lock().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].reference, "legacy-request-id");
    }
    #[tokio::test]
    async fn expired_changed_and_conditional_previews_never_dispatch() {
        for (mode, expected) in [
            (0, "PREVIEW_EXPIRED"),
            (1, "PREVIEW_CHANGED"),
            (2, "PREPARATION_ONLY"),
        ] {
            let (adapter, journal) = fixture(false, false, false);
            let mut prepared = preview();
            if mode == 0 {
                prepared.expires_at = (Utc::now() - Duration::seconds(1)).to_rfc3339();
            }
            if mode == 1 {
                adapter.profile.changed.store(true, Ordering::SeqCst);
            }
            if mode == 2 {
                prepared.binding["preparationOnly"] = json!(true);
            }
            assert_eq!(
                adapter
                    .submit(&prepared, journal.clone())
                    .await
                    .unwrap_err()
                    .code,
                expected
            );
            assert!(!journal.events.lock().unwrap().contains(&"dispatch"));
            assert!(journal.persisted.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn uncertain_write_reconciles_persisted_record_after_restart_without_resubmission() {
        let (adapter, journal) = fixture(true, false, false);
        assert_eq!(
            adapter
                .submit(&preview(), journal.clone())
                .await
                .unwrap_err()
                .code,
            "VENUE_TIMEOUT"
        );
        let original = journal.persisted.lock().unwrap()[0].clone();
        let record = OperationRecord {
            id: "old-operation-id".into(),
            request_id: "old-request-id".into(),
            fingerprint: "unchanged-fingerprint".into(),
            fingerprint_version: 1,
            operation: "fixture.transfer".into(),
            input: preview().input,
            status: Status::Unknown,
            created_at: now(),
            updated_at: now(),
            preview: Some(preview()),
            approval: None,
            submission: Some(original.clone()),
            dispatch_evidence: None,
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: Some("old-run-id".into()),
        };
        let serialized = serde_json::to_vec(&record).unwrap();
        let restored: OperationRecord = serde_json::from_slice(&serialized).unwrap();
        let (restarted, restart_journal) = fixture(false, false, false);
        let result = restarted.reconcile(&restored).await.unwrap();
        assert_eq!(result.status, Status::Pending);
        assert_eq!(result.evidence["identity"], original.reference);
        assert_eq!(serde_json::to_vec(&restored).unwrap(), serialized);
        assert_eq!(&*restart_journal.events.lock().unwrap(), &["reconcile"]);
        assert_eq!(
            journal
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|v| **v == "dispatch")
                .count(),
            1
        );
    }
}
