//! Captured with the schema-25 code at base b6fcb06. To reproduce the fixture,
//! run this example against that revision, before the schema-26 migration.
use gofin_store::{ReservationRow, Store, SubmissionRow};
use serde_json::json;
fn main() {
    let path = std::env::args().nth(1).expect("fixture path");
    let mut store = Store::open(path).unwrap();
    let time = "2026-11-01T00:00:00Z";
    for (id, operation, status) in [
        ("pending-withdrawal", "exchange.withdraw", "pending"),
        ("unknown-order", "exchange.order", "unknown"),
        ("accounting-blocked", "exchange.trade", "pending"),
        ("partial-bridge", "bridge.transfer", "pending"),
    ] {
        let record = json!({"id":id,"request_id":format!("request-{id}"),"fingerprint":format!("fingerprint-{id}"),"fingerprint_version":1,"operation":operation,"input":{},"status":status,"created_at":time,"updated_at":time,"preview":null,"approval":null,"submission":null,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null});
        store
            .create_request(
                id,
                &format!("request-{id}"),
                &format!("fingerprint-{id}"),
                operation,
                None,
                status,
                &record,
                time,
            )
            .unwrap();
        store
            .reserve(
                id,
                &[ReservationRow {
                    resource: format!("synthetic:{id}"),
                    units: "10".into(),
                    fee_units: "1".into(),
                }],
            )
            .unwrap();
        store
            .persist_submission(
                id,
                &SubmissionRow {
                    kind: "synthetic".into(),
                    reference: format!("receipt-{id}"),
                    data: json!({"test_only":true}),
                },
                time,
            )
            .unwrap();
    }
    let plan = json!({"revision":{"plan_id":"synthetic-plan","revision_id":"synthetic-revision","status":"active","expires_at":"2030-01-01T00:00:00Z"},"active_attempt_id":null,"usage":{"consumed":"0"}});
    store
        .begin_financial_plan_request("plan-request", "plan-hash", "synthetic-plan", time)
        .unwrap();
    store
        .commit_financial_plan_revision(
            "plan-request",
            "plan-hash",
            "synthetic-plan",
            "synthetic-revision",
            None,
            &plan["revision"],
            &plan,
            time,
        )
        .unwrap();
}
