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
    // Recreate the prior schema; there is no v3 approval-request table in v2.
    store
        .conn
        .execute_batch(
            "DROP TABLE boundary_authorization_requests;
         DROP TABLE bridge_allocation_owners;
         DROP TABLE bridge_transit;
         DROP TABLE bridge_step_links;
         DROP TABLE bridge_quote_revisions;
         DROP TABLE bridge_settlement_proofs;
         DROP TABLE bridge_evidence_ids;
         DROP TABLE bridge_transfers;
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
        7
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
