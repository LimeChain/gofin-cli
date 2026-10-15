use gofin_store::{Store, SubmissionRow};
use serde_json::json;

#[test]
fn router_and_meta_requests_for_the_same_message_cannot_create_two_store_submissions() {
    let mut store = Store::in_memory().unwrap();
    for (operation_id, request_id) in [
        ("router-operation", "router-request"),
        ("meta-operation", "meta-request"),
    ] {
        store
            .create_request(
                operation_id,
                request_id,
                &format!("fingerprint:{operation_id}"),
                "jupiter.swap",
                None,
                "awaiting_approval",
                &json!({"id":operation_id,"status":"awaiting_approval"}),
                "synthetic-time",
            )
            .unwrap();
    }
    let message_hash = "ab".repeat(32);
    let canonical_reference = format!("solana:message:{message_hash}");
    store
        .persist_submission(
            "router-operation",
            &SubmissionRow {
                kind: "solana.transaction".into(),
                reference: canonical_reference.clone(),
                data: json!({"message_sha256":message_hash,"provider_route":"router","provider_request_id":null}),
            },
            "synthetic-time",
        )
        .unwrap();
    let duplicate = store.persist_submission(
        "meta-operation",
        &SubmissionRow {
            kind: "solana.transaction".into(),
            reference: canonical_reference,
            data: json!({"message_sha256":message_hash,"provider_route":"meta_aggregator","provider_request_id":"different-provider-id"}),
        },
        "synthetic-time",
    );
    assert!(duplicate.is_err());
    assert_eq!(store.submission("meta-operation").unwrap(), None);
}
