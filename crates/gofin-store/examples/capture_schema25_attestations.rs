use gofin_store::{ReservationRow, Store, SubmissionRow};
use serde_json::{Value, json};
fn hash(v: &Value) -> String {
    gofin_domain::binding_hash(v).unwrap()
}
fn seed(store: &mut Store, id: &str) -> Value {
    let request = format!("request-{id}");
    let fingerprint = format!("fingerprint-{id}");
    let record = json!({"id":id,"request_id":request,"fingerprint":fingerprint,"status":"awaiting_approval","submission":null});
    store
        .create_request(
            id,
            &request,
            &fingerprint,
            "synthetic",
            None,
            "awaiting_approval",
            &record,
            "t0",
        )
        .unwrap();
    store
        .reserve(
            id,
            &[
                ReservationRow {
                    resource: "lock:account".into(),
                    units: "0".into(),
                    fee_units: "0".into(),
                },
                ReservationRow {
                    resource: "cash:account:COIN".into(),
                    units: "10".into(),
                    fee_units: "2".into(),
                },
            ],
        )
        .unwrap();
    store
        .persist_submission(
            id,
            &SubmissionRow {
                kind: "synthetic".into(),
                reference: format!("receipt-{id}"),
                data: json!({"nonce":17}),
            },
            "t1",
        )
        .unwrap();
    let mut current = store.get_record(id).unwrap().unwrap();
    current["status"] = json!("unknown");
    store.update_record(id, &current, "t2").unwrap();
    let rows = json!([{"resource":"cash:account:COIN","units":"10","fee_units":"2"},{"resource":"lock:account","units":"0","fee_units":"0"}]);
    json!({"version":1,"operation_id":id,"original_request_id":request,"fingerprint":fingerprint,"record_hash":hash(&current),"reservations_hash":hash(&rows),"resources":["account"],"released_reservations":rows,"submission":current["submission"]})
}
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let mut store = Store::open(path).unwrap();
    let review = seed(&mut store, "synthetic-overridden");
    let approval = json!({"source":"touch_id","binding_hash":hash(&json!({"action":"reservation_override","version":1,"request_id":"override-request","review_hash":hash(&review),"review":review}))});
    store
        .persist_operation_lock_override("override-request", &review, &approval, "t3")
        .unwrap();
    let mut review = seed(&mut store, "synthetic-resolved");
    review["plan_hash"] = Value::Null;
    review["attestation"] = json!({"outcome":"succeeded","reason":"Synthetic human attestation","actual_debits":[],"actual_costs":[],"reported_receipts":[]});
    let approval = json!({"source":"touch_id","binding_hash":hash(&json!({"version":1,"action":"operation_resolution","request_id":"resolution-request","review_hash":hash(&review),"review":review}))});
    store
        .persist_operation_resolution("resolution-request", &review, &approval, None, "t3")
        .unwrap();
}
