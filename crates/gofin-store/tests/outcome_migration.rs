//! Synthetic legacy compatibility and persisted outcome observations.
use gofin_domain::outcome::{
    Attention, Dispatch, OutcomeFacts, OutcomeResult, Phase, Reporting, RetryApproval,
};
use gofin_store::Store;
use serde_json::json;

#[test]
fn approved_recovery_candidates_are_not_limited_by_journal_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.sqlite");
    let legacy = rusqlite::Connection::open(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/schema25.sqlite"
    ))
    .unwrap();
    let raw: String = legacy
        .query_row("SELECT record_json FROM operations LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let template: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let mut store = Store::open(&path).unwrap();
    for i in 0..510 {
        let id = format!("candidate-{i:04}");
        let request = format!("request-{i:04}");
        let mut record = template.clone();
        record["id"] = json!(id);
        record["request_id"] = json!(request);
        record["status"] = json!("awaiting_approval");
        record["submission"] = serde_json::Value::Null;
        record["approval"] = json!({"mechanism":"synthetic","binding_hash":"h","challenge_nonce":"n","approved_at":"2026-11-01T00:00:00Z","expires_at":"2026-11-02T00:00:00Z"});
        store
            .create_request(
                &id,
                &request,
                "h",
                "exchange.withdraw",
                None,
                "awaiting_approval",
                &record,
                "2026-11-01T00:00:00Z",
            )
            .unwrap();
    }
    let candidates = store.approved_unsubmitted_records().unwrap();
    assert_eq!(candidates.len(), 510);
    assert_eq!(candidates.last().unwrap()["id"], "candidate-0509");
}

#[test]
fn schema25_history_survives_migration_and_observations_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25.sqlite"
        ),
        &path,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert!(store.get_record("pending-withdrawal").unwrap().is_some());
    assert!(store.financial_plan("synthetic-plan").unwrap().is_some());
    let facts = OutcomeFacts {
        dispatch: Dispatch::Uncertain,
        result: OutcomeResult::Unknown,
        transfer: None,
        attention: Attention::None,
        reporting: Reporting::Unavailable,
        duplicate_execution_excluded: false,
        next_check_after: None,
        observed_at: "2026-11-01T00:00:00Z".into(),
    };
    store
        .append_outcome_observation("pending-withdrawal", &facts, "synthetic")
        .unwrap();
    store
        .append_outcome_observation("pending-withdrawal", &facts, "second-source")
        .unwrap();
    store
        .dismiss_operation("pending-withdrawal", "2026-11-01T01:00:00Z")
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .outcome_observations("pending-withdrawal")
            .unwrap()
            .len(),
        2
    );
    assert!(store.is_operation_dismissed("pending-withdrawal").unwrap());
    assert!(!facts.project(true, true).replay_safe);
}

#[test]
fn failed_submitted_attempt_requires_fresh_approval_and_reporting_is_independent() {
    let facts = OutcomeFacts {
        dispatch: Dispatch::Dispatched,
        result: OutcomeResult::Failed,
        transfer: None,
        attention: Attention::None,
        reporting: Reporting::Partial,
        duplicate_execution_excluded: true,
        next_check_after: None,
        observed_at: "2026-11-01T00:00:00Z".into(),
    };
    let outcome = facts.project(false, true);
    assert_eq!(outcome.phase, Phase::Failed);
    assert!(outcome.replay_safe);
    assert_eq!(outcome.retry_approval, Some(RetryApproval::Required));
    let executed = OutcomeFacts {
        result: OutcomeResult::Executed,
        ..facts
    };
    assert_eq!(executed.project(false, false).phase, Phase::Done);
    assert!(!executed.project(false, false).replay_safe);
}

#[test]
fn corrupt_history_is_isolated_and_submission_identity_survives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25.sqlite"
        ),
        &path,
    )
    .unwrap();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE operations SET record_json='{' WHERE id='pending-withdrawal'",
            [],
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let record = store.get_record("pending-withdrawal").unwrap().unwrap();
    assert_eq!(record["error"]["code"], "history_unreadable");
    assert_eq!(
        record["submission"]["reference"],
        "receipt-pending-withdrawal"
    );
    assert!(store.list_records().unwrap().len() >= 4);
}

#[test]
fn schema25_human_attestations_survive_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attestations.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25-attestations.sqlite"
        ),
        &path,
    )
    .unwrap();
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .operation_lock_override("override-request")
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .operation_resolution("synthetic-resolved")
            .unwrap()
            .is_some()
    );
}

#[test]
fn schema25_boundary_authorization_survives_as_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grants.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25-grants.sqlite"
        ),
        &path,
    )
    .unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .boundary_authorization("synthetic-boundary")
            .unwrap()
            .unwrap()["request_fingerprint"],
        "boundary-hash"
    );
}

#[test]
fn replay_requires_nonexecution_proof_even_for_failure_or_cancellation() {
    let mut facts = OutcomeFacts {
        dispatch: Dispatch::Dispatched,
        result: OutcomeResult::Failed,
        transfer: None,
        attention: Attention::None,
        reporting: Reporting::Partial,
        duplicate_execution_excluded: false,
        next_check_after: None,
        observed_at: "2026-11-01T00:00:00Z".into(),
    };
    assert!(!facts.project(false, false).replay_safe);
    facts.result = OutcomeResult::Cancelled;
    assert!(!facts.project(false, false).replay_safe);
    facts.duplicate_execution_excluded = true;
    assert!(facts.project(false, false).replay_safe);
    facts.result = OutcomeResult::Partial;
    assert!(!facts.project(false, false).replay_safe);
}

#[test]
fn schema25_in_transit_bridge_and_submission_survive_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25-partial-bridge.sqlite"
        ),
        &path,
    )
    .unwrap();
    let store = Store::open(&path).unwrap();
    let transfer = store
        .bridge_transfer("synthetic-partial-bridge")
        .unwrap()
        .unwrap();
    assert_eq!(transfer.state, gofin_domain::BridgeTransferState::InTransit);
    assert_eq!(
        store
            .submission("synthetic-bridge-op")
            .unwrap()
            .unwrap()
            .reference,
        "synthetic-bridge-source"
    );
}

#[test]
fn interrupted_schema28_transaction_restarts_without_losing_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("interrupted.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25.sqlite"
        ),
        &path,
    )
    .unwrap();
    drop(Store::open(&path).unwrap());
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE preapproval_continuations; DROP TABLE verified_position_anchors; DROP TABLE reauthorization_claims; DROP TABLE submission_admissions; DROP TABLE evm_definitive_rejections; DROP TABLE evm_submission_nonces; DROP TABLE evm_nonce_claims; DROP TABLE action_continuations; PRAGMA user_version=27; BEGIN IMMEDIATE; CREATE TABLE action_continuations(token TEXT); ROLLBACK;").unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert!(store.get_record("pending-withdrawal").unwrap().is_some());
    assert_eq!(
        store
            .submission("pending-withdrawal")
            .unwrap()
            .unwrap()
            .reference,
        "receipt-pending-withdrawal"
    );
}

#[test]
fn similar_open_operation_advisories_are_bounded_and_skip_bad_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("advisories.sqlite");
    let mut store = Store::open(&path).unwrap();
    for (id, resource) in [
        ("matching", "wallet:a"),
        ("different", "wallet:b"),
        ("bad", "wallet:a"),
    ] {
        let record = json!({"id":id,"status":"unknown","preview":{"intent":{"debits":[{"resource":resource,"amount":{"asset":{"network":"synthetic","reference":"token","decimals":6},"atoms":"15"}}]}}});
        store
            .create_request(
                id,
                &format!("request-{id}"),
                &format!("hash-{id}"),
                "wallet.transfer",
                None,
                "unknown",
                &record,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("UPDATE operations SET record_json='{' WHERE id='bad'", [])
            .unwrap();
    }
    let found = store
        .similar_open_operation_records("wallet.transfer", &["wallet:a".into()], 3)
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], "matching");
    assert_eq!(found[0]["amounts"][0]["amount"]["atoms"], "15");
}

#[test]
fn nonce_projection_backfills_legacy_submissions_without_reading_unrelated_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-nonces.sqlite");
    let mut store = Store::open(&path).unwrap();
    for id in ["legacy", "new"] {
        let record = json!({"id":id,"request_id":format!("request-{id}"),"fingerprint":format!("hash-{id}"),"status":"awaiting_approval","submission":null});
        store
            .create_request(
                id,
                &format!("request-{id}"),
                &format!("hash-{id}"),
                "wallet.transfer",
                None,
                "awaiting_approval",
                &record,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
    }
    store.persist_submission("legacy",&gofin_store::SubmissionRow{kind:"ethereum".into(),reference:"legacy".into(),data:json!({"transaction":{"chain_id":1,"sender":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","nonce":7}})},"2026-01-01T00:00:01Z").unwrap();
    drop(store);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE preapproval_continuations; DROP TABLE verified_position_anchors; DROP TABLE reauthorization_claims; DROP TABLE submission_admissions; DROP TABLE evm_definitive_rejections; DROP TABLE evm_submission_nonces; PRAGMA user_version=29;").unwrap();
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .claim_evm_nonce(
                "ethereum:1:0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "new",
                7
            )
            .unwrap(),
        8
    );
}

#[test]
fn typed_but_incomplete_history_is_not_rewritten_during_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shape.sqlite");
    let mut store = Store::open(&path).unwrap();
    let raw = json!({"id":"bad","status":"awaiting_approval","input":{}});
    store
        .create_request(
            "bad",
            "bad-request",
            "bad-fingerprint",
            "synthetic.transfer",
            None,
            "awaiting_approval",
            &raw,
            "t0",
        )
        .unwrap();
    assert!(
        store
            .recover_interrupted()
            .unwrap()
            .cancelled_before_submission
            .is_empty()
    );
    assert_eq!(store.get_record("bad").unwrap().unwrap(), raw);
    let typed = store.get_typed_record("bad").unwrap().unwrap();
    assert_eq!(typed["error"]["code"], "history_unreadable");
    assert_eq!(store.list_typed_records().unwrap()[0]["id"], "bad");
}

#[test]
fn submission_admission_time_is_immutable_and_legacy_falls_back_to_creation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("admission.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25.sqlite"
        ),
        &path,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .submission_admitted_at("pending-withdrawal")
            .unwrap()
            .as_deref(),
        Some("2026-11-01T00:00:00Z")
    );
    let record = json!({"id":"new","request_id":"new-request","fingerprint":"hash","status":"awaiting_approval","submission":null});
    store
        .create_request(
            "new",
            "new-request",
            "hash",
            "synthetic",
            None,
            "awaiting_approval",
            &record,
            "t0",
        )
        .unwrap();
    store
        .persist_submission(
            "new",
            &gofin_store::SubmissionRow {
                kind: "synthetic".into(),
                reference: "receipt".into(),
                data: json!({}),
            },
            "t1",
        )
        .unwrap();
    assert_eq!(
        store.submission_admitted_at("new").unwrap().as_deref(),
        Some("t1")
    );
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.submission_admitted_at("new").unwrap().as_deref(),
        Some("t1")
    );
}

#[test]
fn legacy_bridge_stage_links_and_submission_survive_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bridge.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25-bridge-stages.sqlite"
        ),
        &path,
    )
    .unwrap();
    let store = Store::open(&path).unwrap();
    let transfer = store
        .bridge_transfer("synthetic-partial-bridge")
        .unwrap()
        .unwrap();
    assert_eq!(transfer.steps.len(), 2);
    assert_eq!(
        transfer.steps[0].operation_id.as_deref(),
        Some("synthetic-bridge-op")
    );
    assert_eq!(
        transfer.steps[0].state,
        gofin_domain::BridgeStepState::Submitted
    );
    assert_eq!(
        transfer.steps[1].state,
        gofin_domain::BridgeStepState::Unsubmitted
    );
    assert_eq!(
        store
            .submission("synthetic-bridge-op")
            .unwrap()
            .unwrap()
            .reference,
        "synthetic-bridge-source"
    );
}

#[test]
fn legacy_hyperliquid_favorable_sol_sell_keeps_approval_and_original_fill() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hyperliquid.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25-hyperliquid-sell.sqlite"
        ),
        &path,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    let records = store.list_typed_records().unwrap();
    let raw = records
        .iter()
        .find(|r| r["request_id"] == "directional-close")
        .unwrap()
        .clone();
    let record: gofin_domain::OperationRecord = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(record.status, gofin_domain::Status::Completed);
    let prepared = record.preview.as_ref().unwrap();
    assert_eq!(
        gofin_domain::binding_hash(&gofin_domain::authorization_binding(prepared)).unwrap(),
        record.approval.as_ref().unwrap().binding_hash
    );
    let order = &raw["preview"]["binding"]["orders"][0];
    assert_eq!(order["input"]["symbol"], "SOL/USDC:USDC");
    assert_eq!(order["input"]["size"], "0.95");
    assert_eq!(order["min_price"], "90.95");
    assert_eq!(order["price_semantics"], "directional_limit_v1");
    assert!(raw["effects"].to_string().contains("95.75"));
    assert!(
        store
            .current_position_anchors(None)
            .unwrap()
            .iter()
            .any(|anchor| anchor.position == "SOL/USDC:USDC:long"
                && anchor.principal.atoms.as_str() == "0")
    );
    let submission = store.submission(&record.id).unwrap().unwrap();
    store.recover_interrupted().unwrap();
    assert_eq!(store.submission(&record.id).unwrap().unwrap(), submission);
    assert_eq!(store.list_typed_records().unwrap().len(), 2);
}

#[test]
fn legacy_over_limit_run_remains_observable_without_accounting_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("over-limit.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schema25-over-limit.sqlite"
        ),
        &path,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    let before = store.get_run("synthetic-over-limit-run").unwrap().unwrap();
    assert_eq!(before["initial_allocation_usd_micros"], "100000000");
    assert_eq!(before["spent_cost_usd_micros"], "90000000");
    store.recover_interrupted().unwrap();
    assert_eq!(
        store.get_run("synthetic-over-limit-run").unwrap().unwrap(),
        before
    );
}
