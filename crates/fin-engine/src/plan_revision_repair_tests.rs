#[test]
fn plan_revision_repair_allows_failed_unsubmitted_zero_effect_input_and_window_update() {
    let mut prior = prior_record();
    prior.revision.spec.steps[0].action_deadline_seconds = Some(60);
    prior.revision.spec.steps[0].input = json!({"request_id":"expired-request","deadline":1});
    for status in [Status::Failed, Status::Expired, Status::Cancelled, Status::Rejected] {
    let mut store = Store::in_memory().unwrap();
    persist_plan_attempt(&mut store, "attempt-1", status, None, None);

    let mut next = prior.revision.spec.clone();
    next.steps[0].input = json!({"request_id":"fresh-request","deadline":2});
    next.steps[0].action_deadline_seconds = Some(300);
    let permissions = vec![permission(
        json!({"account":"a"}),
        json!({"qty":"1..50"}),
        "30",
    )];
    validate_revision_transition(Some(&prior), &next, &permissions, &prior.usage, &store).unwrap();
    }
}

#[test]
fn plan_revision_repair_preserves_completed_exact_prefix_permission_metadata() {
    let mut prior = prior_record();
    prior.revision.spec.steps[0].input = json!({"account":"protected-account"});
    let archived = FinancialPlanStepPermission {
        step_id: "step-1".into(),
        generated_input_fields: vec![],
        exact: Some(FinancialPlanExactPermission {
            scope: FinancialPlanExactScope {
                mode: financial_plans::FinancialPlanExactMode::Exact,
                maximum_debits: vec![resource_amount("acct", "USDT", "30")],
                maximum_costs: vec![],
                possible_credits: vec![],
            },
            definition_hash: "historical-definition".into(),
            connections_hash: "historical-connections".into(),
        }),
        permission: None,
    };
    prior.revision.permissions[0] = archived.clone();
    let mut store = Store::in_memory().unwrap();
    persist_plan_attempt(&mut store, "attempt-1", Status::Completed, None, None);

    let mut suffix = step_spec();
    suffix.step_id = "step-2".into();
    suffix.input = json!({"request_id":"suffix"});
    let mut next = prior.revision.spec.clone();
    next.steps.push(suffix);
    let mut suffix_permission = permission(json!({"account":"a"}), json!({"qty":"1..50"}), "10");
    suffix_permission.step_id = "step-2".into();
    let replacement_permissions = vec![archived.clone(), suffix_permission];

    validate_revision_transition_with_completed(
        Some(&prior),
        &next,
        &replacement_permissions,
        &prior.usage,
        &store,
        &["step-1".into()],
    )
    .unwrap();
    assert_eq!(replacement_permissions[0], archived);
}

#[test]
fn plan_revision_partial_success_is_not_treated_as_archived_completion() {
    let prior = prior_record();
    let in_progress_status = FinancialPlanStatus {
        plan_id: prior.revision.plan_id.clone(),
        revision: prior.revision.clone(),
        usage: prior.usage.clone(),
        steps: vec![financial_plans::FinancialPlanStepProgress {
            step_id: "step-1".into(),
            state: FinancialPlanStepProgressState::InProgress,
            attempts: vec![],
            consumed_debits: vec![],
            consumed_costs: vec![],
            verified_receipts: vec![],
            blockers: vec![],
        }],
        remaining_debits: vec![],
        remaining_costs: vec![],
        next_eligible_step_id: Some("step-1".into()),
        blockers: vec![],
        authorization_timing: None,
    };
    assert!(completed_plan_step_ids(in_progress_status).is_empty());

    let changed_permission = permission(json!({"changed":"metadata"}), json!({}), "30");
    let mut next_permissions = prior.revision.permissions.clone();
    next_permissions[0] = changed_permission;
    let store = Store::in_memory().unwrap();
    assert!(
        validate_revision_transition_with_completed(
            Some(&prior),
            &prior.revision.spec,
            &next_permissions,
            &prior.usage,
            &store,
            &[],
        )
        .is_err()
    );
}

#[test]
fn plan_revision_repair_rejects_ambiguous_or_submitted_failures_and_low_usage_caps() {
    let prior = prior_record();
    let mut prior = prior;
    prior.revision.spec.steps[0].action_deadline_seconds = Some(60);
    let mut store = Store::in_memory().unwrap();
    // Missing durable attempt evidence is ambiguous and must keep the old
    // started-step restrictions in force.
    let mut changed = prior.revision.spec.clone();
    changed.steps[0].input["request_id"] = json!("different");
    changed.steps[0].action_deadline_seconds = Some(300);
    let permissions = vec![permission(
        json!({"account":"a"}),
        json!({"qty":"1..50"}),
        "30",
    )];
    assert!(
        validate_revision_transition(Some(&prior), &changed, &permissions, &prior.usage, &store)
            .is_err()
    );

    // A terminal failure that records a submission is still financially
    // unresolved for revision purposes, even if it has no observed effects.
    persist_plan_attempt(
        &mut store,
        "attempt-1",
        Status::Failed,
        Some(fin_domain::Submission {
            kind: "synthetic".into(),
            reference: "submitted".into(),
            data: json!({}),
        }),
        None,
    );
    assert!(!repairable_zero_effect_attempt(
        &prior,
        &prior.attempts[0],
        &store,
    ));
    assert!(
        validate_revision_transition(Some(&prior), &changed, &permissions, &prior.usage, &store)
            .is_err()
    );

    // A stale operation snapshot can omit the embedded submission while the
    // durable journal still has one. The durable row independently vetoes repair.
    let mut stale_store = Store::in_memory().unwrap();
    persist_plan_attempt(&mut stale_store, "attempt-1", Status::Failed, None, None);
    let mut stale_operation: OperationRecord =
        from_value(stale_store.get_record("attempt-1").unwrap().unwrap()).unwrap();
    stale_operation.submission = None;
    assert!(!repairable_zero_effect_record(
        &prior,
        &prior.attempts[0],
        &stale_operation,
        true,
    ));

    let mut repairable_store = Store::in_memory().unwrap();
    persist_plan_attempt(
        &mut repairable_store,
        "attempt-1",
        Status::Failed,
        None,
        None,
    );
    let mut below_use = changed;
    below_use.aggregate_bounds.maximum_debits = vec![resource_amount("acct", "USDT", "19")];
    assert!(
        validate_revision_transition(
            Some(&prior),
            &below_use,
            &permissions,
            &prior.usage,
            &repairable_store,
        )
        .is_err()
    );
}

fn persist_plan_attempt(
    store: &mut Store,
    id: &str,
    status: Status,
    submission: Option<fin_domain::Submission>,
    effects: Option<fin_domain::VerifiedEffects>,
) {
    let record = OperationRecord {
        id: id.into(),
        request_id: format!("{id}-request"),
        fingerprint: format!("{id}-fingerprint"),
        fingerprint_version: 1,
        operation: "synthetic.write".into(),
        input: json!({}),
        status: status.clone(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:01Z".into(),
        preview: None,
        approval: None,
        submission,
        dispatch_evidence: None,
        effects,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    store
        .create_request(
            id,
            &record.request_id,
            &record.fingerprint,
            &record.operation,
            None,
            match status {
                Status::Failed => "failed",
                Status::Completed => "completed",
                Status::Expired => "expired",
                Status::Cancelled => "cancelled",
                Status::Rejected => "rejected",
                _ => "unknown",
            },
            &to_value(&record).unwrap(),
            &record.created_at,
        )
        .unwrap();
}

struct FeasibilityTestAdapter {
    calls: AtomicUsize,
    prepares: AtomicUsize,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl fin_domain::Adapter for FeasibilityTestAdapter {
    fn definitions(&self) -> Vec<fin_domain::Definition> {
        vec![fin_domain::Definition {
            name: "synthetic.feasible".into(),
            description: "Synthetic feasibility check".into(),
            kind: OperationKind::Write,
            execution_actor: fin_domain::ExecutionActor::AgentInvocable,
            input_schema: json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"account":{"type":"string"}},
                "required":["account"],
                "additionalProperties":false
            }),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic".into(),
            completion: "synthetic".into(),
            semantics: json!({"contract":"feasibility-test-v1"}),
        }]
    }

    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        Err(internal(
            "future-balance preparation must not run in plan review",
        ))
    }

    async fn validate_plan_feasibility(&self, _: &str, _: &Value) -> Result<Vec<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(FinError::validation("synthetic infeasible route"));
        }
        Ok(vec!["synthetic prerequisite is deferred".into()])
    }

    fn validate_boundary_candidate(&self, _: &BoundarySpec, _: &PreparedOperation) -> Result<()> {
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
        Err(internal("not used by plan-feasibility regression"))
    }

    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Err(internal("not used by plan-feasibility regression"))
    }
}

struct CountingPlanAuthorizer(AtomicUsize);

#[async_trait::async_trait]
impl fin_domain::Authorizer for CountingPlanAuthorizer {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<fin_domain::ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(fin_domain::ApprovalAudit {
            mechanism: "synthetic-counting-authorizer".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: fin_domain::now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn exact_feasibility_request(request_id: &str) -> FinancialPlanAuthorizationRequest {
    FinancialPlanAuthorizationRequest {
        request_id: request_id.into(),
        plan: financial_plans::FinancialPlanSpec {
            version: financial_plans::FINANCIAL_PLAN_VERSION,
            name: "Synthetic feasibility plan".into(),
            summary: None,
            steps: vec![financial_plans::FinancialPlanStepSpec {
                step_id: "step-1".into(),
                operation: "synthetic.feasible".into(),
                input: json!({"account":"synthetic-account"}),
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
                action_deadline_seconds: None,
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
async fn plan_feasibility_error_precedes_approval_without_future_balance_preparation() {
    let adapter = Arc::new(FeasibilityTestAdapter {
        calls: AtomicUsize::new(0),
        prepares: AtomicUsize::new(0),
        fail: AtomicBool::new(true),
    });
    let authorizer = Arc::new(CountingPlanAuthorizer(AtomicUsize::new(0)));
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap();

    let error = engine
        .authorize_financial_plan(exact_feasibility_request("feasibility-fail"))
        .await
        .unwrap_err();
    assert_eq!(error.message, "synthetic infeasible route");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn plan_feasibility_warnings_are_returned_and_completed_steps_are_not_rechecked() {
    let adapter = Arc::new(FeasibilityTestAdapter {
        calls: AtomicUsize::new(0),
        prepares: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    });
    let authorizer = Arc::new(CountingPlanAuthorizer(AtomicUsize::new(0)));
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        authorizer,
        EngineOptions::default(),
    )
    .unwrap();
    let request = exact_feasibility_request("feasibility-warning");
    let review = engine
        .preview_financial_plan(request.clone())
        .await
        .unwrap();
    assert_eq!(
        review.feasibility_warnings,
        vec!["step-1: synthetic prerequisite is deferred"]
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.prepares.load(Ordering::SeqCst), 0);

    let prior = FinancialPlanAuthorizationRecord {
        revision: FinancialPlanRevision {
            plan_id: "completed-plan".into(),
            revision_id: "completed-revision".into(),
            spec: request.plan.clone(),
            permissions: review.permissions,
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
        usage: FinancialPlanUsage::default(),
        attempts: vec![],
        active_attempt_id: None,
    };
    let completed_request = request.plan;
    let calls_before = adapter.calls.load(Ordering::SeqCst);
    engine
        .validate_plan_feasibility(&completed_request, Some(&prior), &["step-1".into()])
        .await
        .unwrap();
    assert_eq!(adapter.calls.load(Ordering::SeqCst), calls_before);
}
