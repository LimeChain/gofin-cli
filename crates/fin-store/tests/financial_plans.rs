//! Independent storage fault checks. All identities, quantities and receipts are synthetic.
use chrono::{Duration, Utc};
use fin_store::{Settlement, Store, SubmissionRow};
use rusqlite::Connection;
use serde_json::{Value, json};

fn timestamp() -> String {
    Utc::now().to_rfc3339()
}
fn plan() -> Value {
    json!({"revision":{"plan_id":"SYNTHETIC-plan","revision_id":"SYNTHETIC-rev1",
        "status":"active","expires_at":(Utc::now()+Duration::minutes(10)).to_rfc3339()},
        "active_attempt_id":null,"usage":{"consumed":"0"}})
}
fn approve(store: &mut Store, record: &Value) {
    assert!(
        store
            .begin_financial_plan_request(
                "SYNTHETIC-approval",
                "SYNTHETIC-hash",
                "SYNTHETIC-plan",
                &timestamp()
            )
            .unwrap()
    );
    store
        .commit_financial_plan_revision(
            "SYNTHETIC-approval",
            "SYNTHETIC-hash",
            "SYNTHETIC-plan",
            "SYNTHETIC-rev1",
            None,
            &record["revision"],
            record,
            &timestamp(),
        )
        .unwrap();
}
fn claim(store: &mut Store, original: &Value, id: &str) -> fin_store::Result<Value> {
    let mut claimed = original.clone();
    claimed["active_attempt_id"] = json!(id);
    let request = format!("request-{id}");
    let operation = json!({"id":id,"request_id":request,"fingerprint":"SYNTHETIC-attempt-hash",
        "operation":"synthetic.write","input":{},"status":"awaiting_approval","submission":null});
    store.create_financial_plan_attempt(
        "SYNTHETIC-plan",
        "SYNTHETIC-rev1",
        "step1",
        original,
        &claimed,
        id,
        &request,
        "SYNTHETIC-attempt-hash",
        "synthetic.write",
        &operation,
        &timestamp(),
    )?;
    Ok(claimed)
}
fn submission() -> SubmissionRow {
    SubmissionRow {
        kind: "SYNTHETIC".into(),
        reference: "SYNTHETIC-receipt".into(),
        data: json!({"test_only":true}),
    }
}
fn dispatch(store: &mut Store, expected: &Value, time: &str) -> fin_store::Result<()> {
    store.persist_financial_plan_submission(
        "SYNTHETIC-plan",
        expected,
        expected,
        "attempt1",
        expected["revision"]["expires_at"].as_str().unwrap(),
        &submission(),
        time,
    )
}

#[test]
fn initial_approval_and_request_identity_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC.sqlite");
    let original = plan();
    let mut store = Store::open(&path).unwrap();
    approve(&mut store, &original);
    assert!(
        !store
            .begin_financial_plan_request(
                "SYNTHETIC-approval",
                "SYNTHETIC-hash",
                "SYNTHETIC-plan",
                &timestamp()
            )
            .unwrap()
    );
    assert!(
        store
            .begin_financial_plan_request(
                "SYNTHETIC-approval",
                "different",
                "SYNTHETIC-plan",
                &timestamp()
            )
            .is_err()
    );
    drop(store);
    let reopened = Store::open(path).unwrap();
    assert_eq!(
        reopened.financial_plan("SYNTHETIC-plan").unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        reopened
            .financial_plan_revision("SYNTHETIC-plan", "SYNTHETIC-rev1")
            .unwrap(),
        Some(original["revision"].clone())
    );
    assert_eq!(
        reopened
            .financial_plan_request_status("SYNTHETIC-approval")
            .unwrap()
            .unwrap()
            .1,
        "approved"
    );
}

#[test]
fn revocation_wins_against_stale_candidate_creation_and_dispatch() {
    let mut store = Store::in_memory().unwrap();
    let original = plan();
    approve(&mut store, &original);
    let claimed = claim(&mut store, &original, "attempt1").unwrap();
    assert!(claim(&mut store, &original, "attempt2").is_err());
    let mut revoked = claimed.clone();
    revoked["revision"]["status"] = json!("revoked");
    store
        .compare_update_financial_plan("SYNTHETIC-plan", &claimed, &revoked, &timestamp())
        .unwrap();
    assert!(dispatch(&mut store, &claimed, &timestamp()).is_err());
    assert!(dispatch(&mut store, &revoked, &timestamp()).is_err());
    assert!(store.get_record("attempt1").unwrap().unwrap()["submission"].is_null());
    assert!(store.get_record("attempt2").unwrap().is_none());
    assert_eq!(
        store.financial_plan("SYNTHETIC-plan").unwrap(),
        Some(revoked)
    );
}

#[test]
fn expiry_is_checked_at_transaction_time_not_the_callers_old_timestamp() {
    let mut store = Store::in_memory().unwrap();
    let original = plan();
    approve(&mut store, &original);
    let claimed = claim(&mut store, &original, "attempt1").unwrap();
    let mut expired = claimed.clone();
    expired["revision"]["expires_at"] = json!((Utc::now() - Duration::seconds(1)).to_rfc3339());
    store
        .compare_update_financial_plan("SYNTHETIC-plan", &claimed, &expired, &timestamp())
        .unwrap();
    let old_timestamp = (Utc::now() - Duration::minutes(1)).to_rfc3339();
    assert!(dispatch(&mut store, &expired, &old_timestamp).is_err());
    assert!(store.get_record("attempt1").unwrap().unwrap()["submission"].is_null());
}

#[test]
fn settlement_failure_rolls_back_operation_reservation_and_plan_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC.sqlite");
    let mut store = Store::open(&path).unwrap();
    let original = plan();
    approve(&mut store, &original);
    let claimed = claim(&mut store, &original, "attempt1").unwrap();
    dispatch(&mut store, &claimed, &timestamp()).unwrap();
    let before = store.get_record("attempt1").unwrap().unwrap();
    let sql = Connection::open(&path).unwrap();
    sql.execute_batch("INSERT INTO reservations(operation_id,resource,units,fee_units) VALUES ('attempt1','SYNTHETIC:cash','1000000000000000000000001','3');
        CREATE TRIGGER synthetic_settlement_failure BEFORE UPDATE ON financial_plans BEGIN SELECT RAISE(ABORT, 'SYNTHETIC injected settlement failure'); END;").unwrap();
    let mut terminal = before.clone();
    terminal["status"] = json!("completed");
    let settlement = Settlement {
        operation_id: "attempt1".into(),
        terminal_record: terminal.clone(),
        proof: json!({"SYNTHETIC":true}),
        effects: json!([{"SYNTHETIC":"effect"}]),
        run_id: None,
        run_record: None,
    };
    let mut settled_plan = claimed.clone();
    settled_plan["active_attempt_id"] = Value::Null;
    settled_plan["usage"]["consumed"] = json!("1000000000000000000000001");
    assert!(
        store
            .commit_financial_plan_settlement(
                &settlement,
                "SYNTHETIC-plan",
                &claimed,
                &settled_plan,
                "attempt1",
                "settled",
                &timestamp()
            )
            .is_err()
    );
    assert_eq!(store.get_record("attempt1").unwrap(), Some(before));
    assert_eq!(
        store.financial_plan("SYNTHETIC-plan").unwrap(),
        Some(claimed.clone())
    );
    assert_eq!(
        sql.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM reservations WHERE operation_id='attempt1'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        store
            .financial_plan_attempt_owner("attempt1")
            .unwrap()
            .unwrap()
            .3,
        "unresolved"
    );
    sql.execute_batch("DROP TRIGGER synthetic_settlement_failure")
        .unwrap();
    store
        .commit_financial_plan_settlement(
            &settlement,
            "SYNTHETIC-plan",
            &claimed,
            &settled_plan,
            "attempt1",
            "settled",
            &timestamp(),
        )
        .unwrap();
    assert!(
        store
            .commit_financial_plan_settlement(
                &settlement,
                "SYNTHETIC-plan",
                &settled_plan,
                &settled_plan,
                "attempt1",
                "settled",
                &timestamp()
            )
            .is_err()
    );
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.get_record("attempt1").unwrap(), Some(terminal));
    assert_eq!(
        reopened.financial_plan("SYNTHETIC-plan").unwrap(),
        Some(settled_plan)
    );
    assert_eq!(
        sql.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM reservations WHERE operation_id='attempt1'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn version_three_upgrade_keeps_legacy_uncertain_submission_and_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC.sqlite");
    let mut store = Store::open(&path).unwrap();
    let record = json!({"id":"legacy","request_id":"legacy-request","fingerprint":"legacy-hash","operation":"synthetic.write","status":"awaiting_approval"});
    store
        .create_request(
            "legacy",
            "legacy-request",
            "legacy-hash",
            "synthetic.write",
            None,
            "awaiting_approval",
            &record,
            &timestamp(),
        )
        .unwrap();
    store
        .persist_submission("legacy", &submission(), &timestamp())
        .unwrap();
    let submitted = store.get_record("legacy").unwrap().unwrap();
    drop(store);
    let sql = Connection::open(&path).unwrap();
    sql.execute_batch("INSERT INTO reservations(operation_id,resource,units,fee_units) VALUES ('legacy','SYNTHETIC:cash','1000000000000000000000001','3');
        INSERT INTO settlement_progress(operation_id,sequence,snapshot_id,proof_digest,proof_json,effects_json,created_at) VALUES ('legacy',1,'legacy-progress','legacy-digest','{\"proof\":\"preserved\"}','[]','2026-01-01T00:00:00Z');
        INSERT INTO signer_nonces(signer_scope,last_nonce) VALUES ('synthetic-signer',123);
        DROP TABLE financial_plan_attempts; DROP TABLE financial_plan_requests; DROP TABLE financial_plan_revisions; DROP TABLE financial_plans;
        DROP TABLE bridge_allocation_owners; DROP TABLE bridge_transit; DROP TABLE bridge_step_links; DROP TABLE bridge_quote_revisions; DROP TABLE bridge_settlement_proofs; DROP TABLE bridge_evidence_ids; DROP TABLE bridge_transfers;
        DROP TABLE IF EXISTS unresolved_cost_events; DROP TABLE IF EXISTS unresolved_cost_consumption;
        DROP TABLE IF EXISTS grouped_amendment_scopes; DROP TABLE IF EXISTS grouped_amendment_children; DROP TABLE IF EXISTS grouped_amendment_revisions; DROP TABLE IF EXISTS grouped_amendments;
        DROP TABLE IF EXISTS compound_amendment_children; DROP TABLE IF EXISTS operation_amendments; DROP TABLE IF EXISTS operation_proof_events;
        PRAGMA user_version=3;").unwrap();
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.get_record("legacy").unwrap(), Some(submitted));
    assert_eq!(
        sql.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        fin_store::SCHEMA_VERSION
    );
    let exact: String = sql
        .query_row(
            "SELECT units FROM reservations WHERE operation_id='legacy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exact, "1000000000000000000000001");
    let progress_rows: i64 = sql
        .query_row(
            "SELECT COUNT(*) FROM settlement_progress WHERE operation_id='legacy' AND sequence=1 AND proof_digest='legacy-digest'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(progress_rows, 1);
    let nonce: i64 = sql
        .query_row(
            "SELECT last_nonce FROM signer_nonces WHERE signer_scope='synthetic-signer'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(nonce, 123);
}

#[test]
fn interrupted_approval_cannot_be_replayed_as_an_approved_plan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC.sqlite");
    let mut store = Store::open(&path).unwrap();
    assert!(
        store
            .begin_financial_plan_request("interrupted", "hash", "never-approved", &timestamp())
            .unwrap()
    );
    drop(store);
    let mut reopened = Store::open(path).unwrap();
    reopened
        .recover_financial_plan_requests(&timestamp())
        .unwrap();
    assert_eq!(
        reopened
            .financial_plan_request_status("interrupted")
            .unwrap()
            .unwrap()
            .1,
        "interrupted"
    );
    assert!(reopened.financial_plan("never-approved").unwrap().is_none());
}

fn replace(store: &mut Store, expected: &Value) -> fin_store::Result<Value> {
    let mut updated = expected.clone();
    updated["active_attempt_id"] = json!("attempt2");
    let cancelled = json!({"id":"attempt1","status":"rejected","submission":null});
    let candidate = json!({"id":"attempt2","request_id":"request-attempt2","fingerprint":"replacement-hash","operation":"synthetic.write","status":"awaiting_approval","submission":null});
    store.replace_financial_plan_candidate(
        "SYNTHETIC-plan",
        "SYNTHETIC-rev1",
        "step1",
        expected,
        &updated,
        "attempt1",
        &cancelled,
        "attempt2",
        "request-attempt2",
        "replacement-hash",
        "synthetic.write",
        &candidate,
        &timestamp(),
    )?;
    Ok(updated)
}

#[test]
fn candidate_replacement_is_atomic_and_old_candidate_cannot_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC.sqlite");
    let mut store = Store::open(&path).unwrap();
    let original = plan();
    approve(&mut store, &original);
    let claimed = claim(&mut store, &original, "attempt1").unwrap();
    let sql = Connection::open(&path).unwrap();
    sql.execute_batch("CREATE TRIGGER synthetic_replacement_failure BEFORE UPDATE ON financial_plans BEGIN SELECT RAISE(ABORT, 'SYNTHETIC replacement failure'); END;").unwrap();
    assert!(replace(&mut store, &claimed).is_err());
    assert_eq!(
        store.get_record("attempt1").unwrap().unwrap()["status"],
        "awaiting_approval"
    );
    assert!(store.get_record("attempt2").unwrap().is_none());
    assert_eq!(
        store.financial_plan("SYNTHETIC-plan").unwrap(),
        Some(claimed.clone())
    );
    sql.execute_batch("DROP TRIGGER synthetic_replacement_failure")
        .unwrap();
    let replaced = replace(&mut store, &claimed).unwrap();
    assert!(dispatch(&mut store, &claimed, &timestamp()).is_err());
    assert!(dispatch(&mut store, &replaced, &timestamp()).is_err());
    assert_eq!(
        store
            .financial_plan_attempt_owner("attempt1")
            .unwrap()
            .unwrap()
            .3,
        "superseded"
    );
    assert_eq!(
        store
            .financial_plan_attempt_owner("attempt2")
            .unwrap()
            .unwrap()
            .3,
        "unresolved"
    );
}

#[test]
fn submitted_candidate_cannot_be_replaced() {
    let mut store = Store::in_memory().unwrap();
    let original = plan();
    approve(&mut store, &original);
    let claimed = claim(&mut store, &original, "attempt1").unwrap();
    dispatch(&mut store, &claimed, &timestamp()).unwrap();
    assert!(replace(&mut store, &claimed).is_err());
    assert!(store.get_record("attempt2").unwrap().is_none());
    assert_eq!(
        store.financial_plan("SYNTHETIC-plan").unwrap(),
        Some(claimed)
    );
}
