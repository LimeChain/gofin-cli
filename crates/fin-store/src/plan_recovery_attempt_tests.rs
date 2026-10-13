use super::*;
use fin_domain::{
    ApprovalAudit, FinancialIntent, OperationRecord, PreparedOperation, Status, Submission,
    financial_plans::{
        FinancialPlanAttemptLinkage, FinancialPlanAuthorizationRecord,
        FinancialPlanAuthorizationStatus, FinancialPlanBounds, FinancialPlanRecoveryBinding,
        FinancialPlanRecoveryParent, FinancialPlanRevision, FinancialPlanSpec,
        FinancialPlanStepPermission, FinancialPlanStepSpec, FinancialPlanUsage,
        FinancialStepCompletion, PlanPermission,
    },
};
use serde_json::{Value, json};

fn is_concurrent_reservation(result: Result<Option<String>>) -> bool {
    matches!(
        result,
        Err(StoreError::ConcurrentReservation | StoreError::ConcurrentReservationAt { .. })
    )
}

#[test]
fn stored_recovery_parent_chain_requires_every_intervening_step_to_share_exact_parent() {
    let plan = json!({
        "revision": {
            "spec": {"steps": [
                {"step_id":"create"},
                {"step_id":"update"},
                {"step_id":"cancel"},
                {"step_id":"close"}
            ]},
            "permissions": [
                {"step_id":"create","permission":{}},
                {"step_id":"update","permission":{"recovery_parent":{"step_id":"create"}}},
                {"step_id":"cancel","permission":{"recovery_parent":{"step_id":"create"}}},
                {"step_id":"close","permission":{}}
            ]
        }
    });
    assert!(stored_recovery_chain_is_contiguous(
        &plan, "create", "update"
    ));
    assert!(stored_recovery_chain_is_contiguous(
        &plan, "create", "cancel"
    ));
    assert!(!stored_recovery_chain_is_contiguous(
        &plan, "create", "close"
    ));
    assert!(!stored_recovery_chain_is_contiguous(
        &plan, "update", "cancel"
    ));

    let mut skipped_step = plan.clone();
    skipped_step["revision"]["permissions"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|permission| permission["step_id"] == "update")
        .unwrap()["permission"] = json!({});
    assert!(!stored_recovery_chain_is_contiguous(
        &skipped_step,
        "create",
        "cancel"
    ));
}

#[test]
fn later_recovery_child_requires_durable_completed_parent_bound_prefix() {
    let mut store = Store::in_memory().unwrap();
    let parent_submission = Submission {
        kind: "solana_transaction".into(),
        reference: "create-signature".into(),
        data: json!({"message":"create"}),
    };
    let recovery = json!({
        "step_id":"create",
        "operation_id_pointer":"/original_operation_id",
        "submission_pointer":"/original_submission",
        "parent_binding_fields":[]
    });
    let plan = json!({
        "revision":{
            "plan_id":"plan",
            "revision_id":"revision",
            "spec":{"steps":[
                {"step_id":"create"},
                {"step_id":"update"},
                {"step_id":"cancel"}
            ]},
            "permissions":[
                {"step_id":"create","permission":{}},
                {"step_id":"update","permission":{"recovery_parent":recovery}},
                {"step_id":"cancel","permission":{"recovery_parent":recovery}}
            ]
        },
        "attempts":[
            {"plan_id":"plan","revision_id":"revision","step_id":"create","attempt_id":"parent"},
            {"plan_id":"plan","revision_id":"revision","step_id":"update","attempt_id":"update"},
            {"plan_id":"plan","revision_id":"revision","step_id":"cancel","attempt_id":"cancel"}
        ]
    });
    let parent = op_record(
        "parent",
        Status::Pending,
        json!({}),
        Some(parent_submission.clone()),
        Some(json!({})),
    );
    let mut update = op_record(
        "update",
        Status::Completed,
        json!({}),
        Some(Submission {
            kind: "solana_transaction".into(),
            reference: "update-signature".into(),
            data: json!({"message":"update"}),
        }),
        Some(json!({
            "original_operation_id":"parent",
            "original_submission":parent_submission
        })),
    );
    update.operation = "trigger.update".into();
    let cancel = op_record(
        "cancel",
        Status::AwaitingApproval,
        json!({}),
        None,
        Some(json!({})),
    );
    store.conn.execute(
        "INSERT INTO financial_plans(plan_id,active_revision_id,record_json,created_at,updated_at) VALUES('plan','revision',?1,'t0','t0')",
        [plan.to_string()],
    ).unwrap();
    insert_attempt(&mut store, &parent, "create", "unresolved");
    insert_attempt(&mut store, &update, "update", "settled");
    insert_attempt(&mut store, &cancel, "cancel", "unresolved");
    for operation in [&parent, &update] {
        let submission = operation.submission.as_ref().unwrap();
        store.conn.execute(
            "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES(?1,?2,?3,?4)",
            params![operation.id, submission.kind, submission.reference, submission.data.to_string()],
        ).unwrap();
    }
    let tx = store.conn.unchecked_transaction().unwrap();
    assert!(
        stored_recovery_prefix_is_completed_tx(
            &tx, "plan", "revision", &plan, "create", "parent", "cancel"
        )
        .unwrap()
    );
    assert!(
        !stored_recovery_prefix_is_completed_tx(
            &tx,
            "plan",
            "stale-revision",
            &plan,
            "create",
            "parent",
            "cancel"
        )
        .unwrap()
    );
    let mut missing_linkage = plan.clone();
    missing_linkage["attempts"] = json!([
        {"plan_id":"plan","revision_id":"revision","step_id":"create","attempt_id":"parent"},
        {"plan_id":"plan","revision_id":"revision","step_id":"cancel","attempt_id":"cancel"}
    ]);
    assert!(
        !stored_recovery_prefix_is_completed_tx(
            &tx,
            "plan",
            "revision",
            &missing_linkage,
            "create",
            "parent",
            "cancel"
        )
        .unwrap()
    );

    tx.execute(
        "UPDATE financial_plan_attempts SET candidate_state='unresolved' WHERE operation_id='update'",
        [],
    ).unwrap();
    assert!(
        !stored_recovery_prefix_is_completed_tx(
            &tx, "plan", "revision", &plan, "create", "parent", "cancel"
        )
        .unwrap()
    );

    // A prior child is usable only when its immutable prepared binding carries
    // the original parent and submission; a similarly named completed write
    // cannot stand in for that recovery step.
    tx.execute(
        "UPDATE financial_plan_attempts SET candidate_state='settled' WHERE operation_id='update'",
        [],
    )
    .unwrap();
    let wrong_parent = op_record(
        "update",
        Status::Completed,
        json!({}),
        update.submission.clone(),
        Some(json!({
            "original_operation_id":"different-parent",
            "original_submission":parent_submission
        })),
    );
    tx.execute(
        "UPDATE operations SET status='completed',record_json=?1 WHERE id='update'",
        [serde_json::to_string(&wrong_parent).unwrap()],
    )
    .unwrap();
    assert!(
        !stored_recovery_prefix_is_completed_tx(
            &tx, "plan", "revision", &plan, "create", "parent", "cancel"
        )
        .unwrap()
    );
    let correct_update = op_record(
        "update",
        Status::Completed,
        json!({}),
        update.submission.clone(),
        Some(json!({
            "original_operation_id":"parent",
            "original_submission":parent_submission
        })),
    );
    tx.execute(
        "UPDATE operations SET status='completed',record_json=?1 WHERE id='update'",
        [serde_json::to_string(&correct_update).unwrap()],
    )
    .unwrap();

    // Definite no-submission failures may precede a completed retry, but no
    // submitted/unknown attempt can be skipped.
    let failed_retry = op_record(
        "update-failed",
        Status::Failed,
        json!({}),
        None,
        Some(json!({
            "original_operation_id":"parent",
            "original_submission":parent_submission
        })),
    );
    tx.execute(
        "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,'failed',NULL,?5,'t0','t1')",
        params![failed_retry.id, failed_retry.request_id, failed_retry.fingerprint, failed_retry.operation, serde_json::to_string(&failed_retry).unwrap()],
    ).unwrap();
    tx.execute(
        "INSERT INTO financial_plan_attempts(plan_id,revision_id,step_id,operation_id,request_id,candidate_state,created_at) VALUES('plan','revision','update','update-failed',?1,'failed','t1')",
        [&failed_retry.request_id],
    ).unwrap();
    let mut with_safe_retry = plan.clone();
    with_safe_retry["attempts"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "plan_id":"plan",
            "revision_id":"revision",
            "step_id":"update",
            "attempt_id":"update-failed"
        }));
    assert!(
        stored_recovery_prefix_is_completed_tx(
            &tx,
            "plan",
            "revision",
            &with_safe_retry,
            "create",
            "parent",
            "cancel"
        )
        .unwrap()
    );

    let submitted_failure = op_record(
        "update-unknown",
        Status::Failed,
        json!({}),
        Some(Submission {
            kind: "solana_transaction".into(),
            reference: "uncertain-update-signature".into(),
            data: json!({"message":"submitted"}),
        }),
        Some(json!({
            "original_operation_id":"parent",
            "original_submission":parent_submission
        })),
    );
    tx.execute(
        "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,'failed',NULL,?5,'t0','t2')",
        params![submitted_failure.id, submitted_failure.request_id, submitted_failure.fingerprint, submitted_failure.operation, serde_json::to_string(&submitted_failure).unwrap()],
    )
    .unwrap();
    let durable_submission = submitted_failure.submission.as_ref().unwrap();
    tx.execute(
        "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES(?1,?2,?3,?4)",
        params![
            submitted_failure.id,
            durable_submission.kind,
            durable_submission.reference,
            durable_submission.data.to_string()
        ],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO financial_plan_attempts(plan_id,revision_id,step_id,operation_id,request_id,candidate_state,created_at) VALUES('plan','revision','update','update-unknown',?1,'failed','t2')",
        [&submitted_failure.request_id],
    )
    .unwrap();
    let mut with_submitted_failure = with_safe_retry.clone();
    with_submitted_failure["attempts"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "plan_id":"plan",
            "revision_id":"revision",
            "step_id":"update",
            "attempt_id":"update-unknown"
        }));
    assert!(
        !stored_recovery_prefix_is_completed_tx(
            &tx,
            "plan",
            "revision",
            &with_submitted_failure,
            "create",
            "parent",
            "cancel"
        )
        .unwrap()
    );

    let pending_update = op_record(
        "update",
        Status::Pending,
        json!({}),
        update.submission.clone(),
        Some(json!({
            "original_operation_id":"parent",
            "original_submission":parent_submission
        })),
    );
    tx.execute(
        "UPDATE operations SET status='pending',record_json=?1 WHERE id='update'",
        [serde_json::to_string(&pending_update).unwrap()],
    )
    .unwrap();
    assert!(
        !stored_recovery_prefix_is_completed_tx(
            &tx, "plan", "revision", &plan, "create", "parent", "cancel"
        )
        .unwrap()
    );
}

fn recovery_plan() -> FinancialPlanAuthorizationRecord {
    let recovery = FinancialPlanRecoveryParent {
        step_id: "create".into(),
        operation_id_pointer: "/original_operation_id".into(),
        submission_pointer: "/original_submission".into(),
        parent_binding_fields: vec![FinancialPlanRecoveryBinding {
            parent_binding_pointer: "/order_key".into(),
            child_input_pointer: "/order_key".into(),
        }],
    };
    let permission = |step_id: &str, recovery_parent| FinancialPlanStepPermission {
        step_id: step_id.into(),
        generated_input_fields: vec![],
        exact: None,
        permission: Some(PlanPermission {
            spec: fin_domain::boundaries::BoundarySpec {
                fixed: json!({}),
                parameters: json!({}),
                maximum_debits: vec![],
                maximum_costs: vec![],
                summary: "synthetic test".into(),
            },
            recovery_parent,
            possible_credits: vec![],
            fee_assets: vec![],
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        }),
    };
    FinancialPlanAuthorizationRecord {
        revision: FinancialPlanRevision {
            plan_id: "plan".into(),
            revision_id: "revision".into(),
            spec: FinancialPlanSpec {
                version: 1,
                name: "recovery test".into(),
                steps: vec!["create", "cancel"]
                    .into_iter()
                    .map(|step_id| FinancialPlanStepSpec {
                        step_id: step_id.into(),
                        operation: if step_id == "create" {
                            "trigger.create"
                        } else {
                            "trigger.cancel"
                        }
                        .into(),
                        input: if step_id == "create" {
                            json!({})
                        } else {
                            json!({
                                "original_operation_id": null,
                                "original_submission": null,
                                "order_key": null
                            })
                        },
                        permitted_variations: json!({}),
                        completion: FinancialStepCompletion {
                            principal_spend: None,
                            minimum_net_receipt: None,
                            operation_succeeded: true,
                        },
                        funding_dependencies: vec![],
                    })
                    .collect(),
                aggregate_bounds: FinancialPlanBounds {
                    maximum_debits: vec![],
                    maximum_costs: vec![],
                },
            },
            permissions: vec![
                permission("create", None),
                permission("cancel", Some(recovery)),
            ],
            approval_binding_hash: "approval-hash".into(),
            approved_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            valid_for_seconds: 60,
            status: FinancialPlanAuthorizationStatus::Active,
            replaces_revision_id: None,
        },
        approval: ApprovalAudit {
            mechanism: "synthetic".into(),
            binding_hash: "approval-hash".into(),
            challenge_nonce: "nonce".into(),
            approved_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
        },
        request_fingerprint: "fingerprint".into(),
        usage: FinancialPlanUsage::default(),
        attempts: vec![linkage("create", "parent"), linkage("cancel", "child")],
        active_attempt_id: Some("parent".into()),
    }
}

fn linkage(step_id: &str, operation_id: &str) -> FinancialPlanAttemptLinkage {
    FinancialPlanAttemptLinkage {
        plan_id: "plan".into(),
        revision_id: "revision".into(),
        step_id: step_id.into(),
        attempt_id: operation_id.into(),
        request_fingerprint: format!("{operation_id}-fingerprint"),
        execution_binding_hash: format!("{operation_id}-binding"),
        submission_id: None,
        result: None,
    }
}

fn op_record(
    id: &str,
    status: Status,
    input: Value,
    submission: Option<Submission>,
    binding: Option<Value>,
) -> OperationRecord {
    let preview = binding.map(|binding| PreparedOperation {
        operation: "trigger.create".into(),
        input: json!({}),
        binding,
        summary: "test".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: "2026-01-01T00:00:00Z".into(),
        expires_at: "2099-01-01T00:00:00Z".into(),
        valuation: None,
        intent: FinancialIntent::default(),
        operation_version: 1,
    });
    OperationRecord {
        id: id.into(),
        request_id: format!("request-{id}"),
        fingerprint: format!("fingerprint-{id}"),
        fingerprint_version: 1,
        operation: "trigger.test".into(),
        input,
        status,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        preview,
        approval: None,
        submission,
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}

fn insert_attempt(
    store: &mut Store,
    operation: &OperationRecord,
    step_id: &str,
    candidate_state: &str,
) {
    let json = serde_json::to_string(operation).unwrap();
    store.conn.execute(
        "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,NULL,?6,?7,?7)",
        params![operation.id, operation.request_id, operation.fingerprint, operation.operation, format!("{:?}", operation.status).to_lowercase(), json, operation.created_at],
    ).unwrap();
    store.conn.execute(
        "INSERT INTO financial_plan_attempts(plan_id,revision_id,step_id,operation_id,request_id,candidate_state,created_at) VALUES('plan','revision',?1,?2,?3,?4,'2026-01-01T00:00:00Z')",
        params![step_id, operation.id, operation.request_id, candidate_state],
    ).unwrap();
}

fn setup(parent_status: Status, parent_state: &str) -> (Store, Value) {
    let mut store = Store::in_memory().unwrap();
    let submission = Submission {
        kind: "trigger".into(),
        reference: "parent-submission".into(),
        data: json!({"immutable":true}),
    };
    let parent = op_record(
        "parent",
        parent_status,
        json!({}),
        Some(submission.clone()),
        Some(json!({"order_key":"verified-order-key"})),
    );
    let child = op_record(
        "child",
        Status::Pending,
        json!({"order_key":null}),
        Some(Submission {
            kind: "trigger".into(),
            reference: "cancel-submission".into(),
            data: json!({}),
        }),
        Some(json!({
            "original_operation_id":"parent",
            "original_submission":submission,
            "order_key":"verified-order-key"
        })),
    );
    let record = recovery_plan();
    store.conn.execute(
        "INSERT INTO financial_plans(plan_id,active_revision_id,record_json,created_at,updated_at) VALUES('plan','revision',?1,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        [serde_json::to_string(&record).unwrap()],
    ).unwrap();
    insert_attempt(&mut store, &parent, "create", parent_state);
    insert_attempt(&mut store, &child, "cancel", "unresolved");
    (store, serde_json::to_value(record).unwrap())
}

fn insert_amendment_candidate(
    store: &mut Store,
    id: &str,
    status: Status,
    parent_submission: Value,
    submitted: bool,
    operation: &str,
) {
    let submission = submitted.then(|| Submission {
        kind: "solana_transaction".into(),
        reference: format!("signature-{id}"),
        data: json!({"synthetic":true}),
    });
    let mut child = op_record(
        id,
        status,
        json!({}),
        submission.clone(),
        Some(json!({
            "__fin_amendment": {
                "version": 1,
                "parent_operation_id": "amend-parent",
                "parent_submission": parent_submission,
                "request_id": "request-pda"
            }
        })),
    );
    child.operation = operation.into();
    store.conn.execute(
        "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,NULL,?6,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        params![child.id, child.request_id, child.fingerprint, child.operation, format!("{:?}", child.status).to_lowercase(), serde_json::to_string(&child).unwrap()],
    ).unwrap();
    if let Some(submission) = submission {
        store.conn.execute(
            "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES(?1,?2,?3,?4)",
            params![child.id, submission.kind, submission.reference, submission.data.to_string()],
        ).unwrap();
    }
}

#[test]
fn amendment_query_returns_unordered_durable_candidates_and_excludes_forged_links() {
    let mut store = Store::in_memory().unwrap();
    let parent_submission = Submission {
        kind: "solana_transaction".into(),
        reference: "parent-signature".into(),
        data: json!({"message_sha256":"pinned"}),
    };
    let parent = op_record(
        "amend-parent",
        Status::Pending,
        json!({}),
        Some(parent_submission.clone()),
        Some(json!({"request":"request-pda"})),
    );
    store.conn.execute(
        "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,NULL,?6,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        params![parent.id, parent.request_id, parent.fingerprint, parent.operation, "pending", serde_json::to_string(&parent).unwrap()],
    ).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES(?1,?2,?3,?4)",
            params![
                parent.id,
                parent_submission.kind,
                parent_submission.reference,
                parent_submission.data.to_string()
            ],
        )
        .unwrap();

    let exact = serde_json::to_value(&parent_submission).unwrap();
    insert_amendment_candidate(
        &mut store,
        "z-pending",
        Status::Pending,
        exact.clone(),
        true,
        "jupiter.perps.limit.update",
    );
    insert_amendment_candidate(
        &mut store,
        "a-completed",
        Status::Completed,
        exact.clone(),
        true,
        "jupiter.perps.limit.update",
    );
    insert_amendment_candidate(
        &mut store,
        "wrong-parent-signature",
        Status::Completed,
        json!({"kind":"solana_transaction","reference":"other","data":{}}),
        true,
        "jupiter.perps.limit.update",
    );
    insert_amendment_candidate(
        &mut store,
        "unsubmitted",
        Status::AwaitingApproval,
        exact,
        false,
        "jupiter.perps.limit.update",
    );
    insert_amendment_candidate(
        &mut store,
        "other-operation",
        Status::Completed,
        serde_json::to_value(parent_submission).unwrap(),
        true,
        "jupiter.perps.tpsl.update",
    );

    let candidates = store
        .amendment_children("amend-parent", "jupiter.perps.limit.update")
        .unwrap();
    let ids = candidates
        .iter()
        .map(|record| record["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["a-completed", "z-pending"]);
    assert_eq!(candidates[0]["status"], "completed");
    assert_eq!(candidates[1]["status"], "pending");
}

#[test]
fn cancellation_child_query_matches_exact_durable_parent_submission() {
    let mut store = Store::in_memory().unwrap();
    let parent_submission = Submission {
        kind: "solana_transaction".into(),
        reference: "parent-signature".into(),
        data: json!({"message_sha256":"parent-message"}),
    };
    let parent = op_record(
        "cancel-parent",
        Status::Pending,
        json!({}),
        Some(parent_submission.clone()),
        Some(json!({})),
    );
    store.conn.execute(
        "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,'pending',NULL,?5,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        params![parent.id, parent.request_id, parent.fingerprint, parent.operation, serde_json::to_string(&parent).unwrap()],
    ).unwrap();

    let insert_child = |store: &mut Store, id: &str, submission: Value| {
        let mut child = op_record(id, Status::Completed, json!({}), None, Some(json!({})));
        child.operation = "jupiter.perps.limit.cancel".into();
        let prepared = child.preview.as_mut().unwrap();
        prepared.intent.expected = vec![fin_domain::EffectConstraint::SubmissionCancellation {
            operation_id: "cancel-parent".into(),
            submission: serde_json::from_value(submission).unwrap(),
        }];
        store.conn.execute(
            "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,'completed',NULL,?5,'2026-01-01T00:00:01Z','2026-01-01T00:00:01Z')",
            params![child.id, child.request_id, child.fingerprint, child.operation, serde_json::to_string(&child).unwrap()],
        ).unwrap();
    };
    insert_child(
        &mut store,
        "wrong-submission",
        json!({"kind":"solana_transaction","reference":"other-signature","data":{"message_sha256":"other"}}),
    );
    insert_child(
        &mut store,
        "exact-child",
        serde_json::to_value(&parent_submission).unwrap(),
    );

    let children = store
        .cancellation_children("cancel-parent", &parent_submission)
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], "exact-child");
    let wrong_parent_submission = Submission {
        kind: "solana_transaction".into(),
        reference: "substituted-signature".into(),
        data: json!({"message_sha256":"substituted"}),
    };
    assert!(
        store
            .cancellation_children("cancel-parent", &wrong_parent_submission)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn submitted_recovery_child_reconciles_when_parent_settles_first() {
    let (store, plan) = setup(Status::Completed, "settled");
    let tx = store.conn.unchecked_transaction().unwrap();
    assert_eq!(
        recovery_parent_for_existing_attempt_tx(&tx, "plan", "child", &plan)
            .unwrap()
            .as_deref(),
        Some("parent")
    );
}

#[test]
fn child_first_and_unresolved_parent_recovery_remain_linked() {
    let (store, plan) = setup(Status::Pending, "unresolved");
    let tx = store.conn.unchecked_transaction().unwrap();
    assert_eq!(
        recovery_parent_for_existing_attempt_tx(&tx, "plan", "child", &plan)
            .unwrap()
            .as_deref(),
        Some("parent")
    );
}

#[test]
fn recovery_child_must_freeze_parent_identity_in_its_prepared_binding() {
    let (store, plan) = setup(Status::Pending, "unresolved");
    let mut child: OperationRecord = {
        let raw: String = store
            .conn
            .query_row(
                "SELECT record_json FROM operations WHERE id='child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    };
    // The engine-injected input alone is not the immutable adapter binding.
    // A recovery adapter must copy the resolved parent identity and exact
    // submission into the prepared binding at the configured pointer paths.
    child.preview.as_mut().unwrap().binding = json!({"order_key":"verified-order-key"});
    store
        .conn
        .execute(
            "UPDATE operations SET record_json=?1 WHERE id='child'",
            [serde_json::to_string(&child).unwrap()],
        )
        .unwrap();
    let tx = store.conn.unchecked_transaction().unwrap();
    assert!(matches!(
        recovery_parent_for_existing_attempt_tx(&tx, "plan", "child", &plan),
        Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_operation_binding_missing"
        })
    ));
}

#[test]
fn terminal_parent_cannot_authorize_a_child_without_a_persisted_submission() {
    let (store, plan) = setup(Status::Completed, "settled");
    let mut child = op_record(
        "child",
        Status::AwaitingApproval,
        json!({"order_key":null}),
        None,
        Some(json!({
            "original_operation_id":"parent",
            "original_submission":{
                "kind":"trigger",
                "reference":"parent-submission",
                "data":{"immutable":true}
            },
            "order_key":"verified-order-key"
        })),
    );
    child.request_id = "request-child".into();
    store
        .conn
        .execute(
            "UPDATE operations SET record_json=?1,status='awaiting_approval' WHERE id='child'",
            [serde_json::to_string(&child).unwrap()],
        )
        .unwrap();
    let tx = store.conn.unchecked_transaction().unwrap();
    assert!(is_concurrent_reservation(
        recovery_parent_for_existing_attempt_tx(&tx, "plan", "child", &plan)
    ));
}

#[test]
fn a_second_unresolved_recovery_child_is_rejected_even_with_another_request_id() {
    let (mut store, plan) = setup(Status::Pending, "unresolved");
    let extra = op_record(
        "other-child",
        Status::Pending,
        json!({}),
        Some(Submission {
            kind: "trigger".into(),
            reference: "another-request-id".into(),
            data: json!({}),
        }),
        None,
    );
    insert_attempt(&mut store, &extra, "cancel", "unresolved");
    let tx = store.conn.unchecked_transaction().unwrap();
    assert!(is_concurrent_reservation(
        recovery_parent_for_existing_attempt_tx(&tx, "plan", "child", &plan)
    ));
}
