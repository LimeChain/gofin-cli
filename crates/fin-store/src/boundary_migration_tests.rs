//! Synthetic migration preserves opaque financial state without any dispatch.
use super::*;

#[test]
fn v2_to_v3_keeps_grants_unresolved_submissions_and_exact_reservations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-v2.sqlite");
    let mut store = Store::open(&path).unwrap();
    let operation = json!({"id":"SYNTHETIC-op","request_id":"SYNTHETIC-request","fingerprint":"SYNTHETIC-fingerprint",
        "operation":"synthetic.transfer","status":"submitted","submission":{"kind":"SYNTHETIC","reference":"SYNTHETIC-external"}});
    store
        .create_request(
            "SYNTHETIC-op",
            "SYNTHETIC-request",
            "SYNTHETIC-fingerprint",
            "synthetic.transfer",
            None,
            "submitted",
            &operation,
            "t0",
        )
        .unwrap();
    let grant = json!({"request_fingerprint":"SYNTHETIC-grant-fingerprint","authorization":{"id":"SYNTHETIC-grant","intent_id":"SYNTHETIC-intent","status":"active",
        "spent_debits":[{"atoms":"1000000000000000001"}],"spent_costs":[{"atoms":"3"}]},"active_attempt":"SYNTHETIC-op","attempts":["SYNTHETIC-op"]});
    store
        .create_boundary_authorization(
            "SYNTHETIC-grant",
            "SYNTHETIC-grant-request",
            "SYNTHETIC-intent",
            "SYNTHETIC-grant-fingerprint",
            &grant,
            "t0",
        )
        .unwrap();
    store
        .attach_boundary_attempt("SYNTHETIC-grant", "SYNTHETIC-op", "t0")
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES (?1,?2,?3,?4)",
            params![
                "SYNTHETIC-op",
                "SYNTHETIC",
                "SYNTHETIC-external",
                "{\"test_only\":true}"
            ],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO reservations(operation_id,resource,units,fee_units) VALUES (?1,?2,?3,?4)",
            params![
                "SYNTHETIC-op",
                "SYNTHETIC:resource",
                "999999999999999999999999999999999999",
                "3"
            ],
        )
        .unwrap();
    // Recreate v2 after removing the v3 and v4 tables from this fixture.
    store
        .conn
        .execute_batch(
            "DROP TABLE boundary_authorization_requests;
         DROP TABLE financial_plan_attempts;
         DROP TABLE financial_plan_requests;
         DROP TABLE financial_plan_revisions;
         DROP TABLE financial_plans;
         DROP TABLE bridge_allocation_owners;
         DROP TABLE bridge_transit;
         DROP TABLE bridge_step_links;
         DROP TABLE bridge_quote_revisions;
         DROP TABLE bridge_settlement_proofs;
         DROP TABLE bridge_evidence_ids;
         DROP TABLE bridge_transfers;
         DROP TABLE settlement_progress;
         DROP TABLE signer_nonces;
         PRAGMA user_version=2;",
        )
        .unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .conn
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        10
    );
    assert_eq!(
        reopened.get_record("SYNTHETIC-op").unwrap().unwrap(),
        operation
    );
    assert_eq!(
        reopened
            .boundary_authorization("SYNTHETIC-grant")
            .unwrap()
            .unwrap(),
        grant
    );
    assert_eq!(
        reopened.boundary_attempts("SYNTHETIC-grant").unwrap(),
        vec!["SYNTHETIC-op"]
    );
    let identity: (String, String) = reopened
        .conn
        .query_row(
            "SELECT kind,reference FROM submissions WHERE operation_id='SYNTHETIC-op'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(identity, ("SYNTHETIC".into(), "SYNTHETIC-external".into()));
    let reserved: (String, String) = reopened
        .conn
        .query_row(
            "SELECT units,fee_units FROM reservations WHERE operation_id='SYNTHETIC-op'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        reserved,
        ("999999999999999999999999999999999999".into(), "3".into())
    );
    assert!(
        reopened
            .boundary_authorization_request_status("SYNTHETIC-grant-request")
            .unwrap()
            .is_none()
    );
}

#[test]
fn named_plans_v4_upgrades_with_bridge_schema_without_replaying_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-named-plans-v4.sqlite");
    let mut store = Store::open(&path).unwrap();
    let plan = json!({
        "revision": {"plan_id":"SYNTHETIC-plan","revision_id":"SYNTHETIC-rev1","status":"active","expires_at":"2099-01-01T00:00:00Z"},
        "active_attempt_id":null,
        "usage":{"consumed":"0"}
    });
    assert!(
        store
            .begin_financial_plan_request(
                "SYNTHETIC-plan-request",
                "SYNTHETIC-plan-fingerprint",
                "SYNTHETIC-plan",
                "t0",
            )
            .unwrap()
    );
    store
        .commit_financial_plan_revision(
            "SYNTHETIC-plan-request",
            "SYNTHETIC-plan-fingerprint",
            "SYNTHETIC-plan",
            "SYNTHETIC-rev1",
            None,
            &plan["revision"],
            &plan,
            "t1",
        )
        .unwrap();
    let operation = json!({
        "id":"SYNTHETIC-unresolved",
        "request_id":"SYNTHETIC-unresolved-request",
        "fingerprint":"SYNTHETIC-operation-fingerprint",
        "operation":"synthetic.transfer",
        "status":"submitted",
        "submission":{"kind":"SYNTHETIC","reference":"SYNTHETIC-external"}
    });
    store
        .create_request(
            "SYNTHETIC-unresolved",
            "SYNTHETIC-unresolved-request",
            "SYNTHETIC-operation-fingerprint",
            "synthetic.transfer",
            None,
            "submitted",
            &operation,
            "t2",
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES (?1,?2,?3,?4)",
            params![
                "SYNTHETIC-unresolved",
                "SYNTHETIC",
                "SYNTHETIC-external",
                "{\"retained\":true}"
            ],
        )
        .unwrap();

    // Recreate the schema emitted by the earlier named-plans branch: plans
    // exist at version 4, while this bridge schema lineage is absent.
    store
        .conn
        .execute_batch(
            "DROP TABLE bridge_allocation_owners;
             DROP TABLE bridge_transit;
             DROP TABLE bridge_step_links;
             DROP TABLE bridge_quote_revisions;
             DROP TABLE bridge_settlement_proofs;
             DROP TABLE bridge_evidence_ids;
             DROP TABLE bridge_transfers;
             DROP TABLE settlement_progress;
             DROP TABLE signer_nonces;
             PRAGMA user_version=4;",
        )
        .unwrap();
    drop(store);

    let upgraded = Store::open(&path).unwrap();
    assert_eq!(
        upgraded
            .conn
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        10
    );
    assert_eq!(
        upgraded.financial_plan("SYNTHETIC-plan").unwrap(),
        Some(plan.clone())
    );
    assert_eq!(
        upgraded
            .financial_plan_revision("SYNTHETIC-plan", "SYNTHETIC-rev1")
            .unwrap(),
        Some(plan["revision"].clone())
    );
    assert_eq!(
        upgraded
            .financial_plan_request_status("SYNTHETIC-plan-request")
            .unwrap()
            .unwrap()
            .1,
        "approved"
    );
    assert_eq!(
        upgraded.get_record("SYNTHETIC-unresolved").unwrap(),
        Some(operation)
    );
    assert_eq!(
        upgraded
            .conn
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM operations WHERE id='SYNTHETIC-unresolved'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        upgraded
            .conn
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM bridge_transfers", [], |row| row
                .get(0),)
            .unwrap(),
        0
    );
}

#[test]
fn unknown_v4_database_is_rejected_without_ddl_or_row_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-unknown-v4.sqlite");
    let mut store = Store::open(&path).unwrap();
    let operation = json!({
        "id":"SYNTHETIC-preserved",
        "request_id":"SYNTHETIC-preserved-request",
        "fingerprint":"SYNTHETIC-preserved-hash",
        "operation":"synthetic.transfer",
        "status":"submitted"
    });
    store
        .create_request(
            "SYNTHETIC-preserved",
            "SYNTHETIC-preserved-request",
            "SYNTHETIC-preserved-hash",
            "synthetic.transfer",
            None,
            "submitted",
            &operation,
            "t0",
        )
        .unwrap();
    store
        .conn
        .execute_batch(
            "DROP TABLE financial_plan_attempts;
             DROP TABLE financial_plan_requests;
             DROP TABLE financial_plan_revisions;
             DROP TABLE financial_plans;
             DROP TABLE bridge_allocation_owners;
             DROP TABLE bridge_transit;
             DROP TABLE bridge_step_links;
             DROP TABLE bridge_quote_revisions;
             DROP TABLE bridge_settlement_proofs;
             DROP TABLE bridge_evidence_ids;
             DROP TABLE bridge_transfers;
             DROP TABLE settlement_progress;
             DROP TABLE signer_nonces;
             PRAGMA user_version=4;",
        )
        .unwrap();
    drop(store);

    assert!(matches!(
        Store::open(&path),
        Err(StoreError::ForeignDatabase)
    ));
    let inspect = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        inspect
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        4
    );
    assert_eq!(
        inspect
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='bridge_transfers'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        inspect
            .query_row::<String, _, _>(
                "SELECT record_json FROM operations WHERE id='SYNTHETIC-preserved'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        operation.to_string()
    );
}
