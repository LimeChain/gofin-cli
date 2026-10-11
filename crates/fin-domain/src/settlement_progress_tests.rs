use super::SettlementProgress;
use serde_json::json;

#[test]
fn cumulative_progress_rejects_one_receipt_digest_under_multiple_event_ids() {
    let evidence = json!({
        "settlement_progress": {
            "version": 1,
            "sequence": 2,
            "snapshot_id": "snapshot-2",
            "event_digests": {
                "oid:tid:hash": "canonical-receipt-digest",
                "adapter-alias": "canonical-receipt-digest"
            }
        }
    });
    assert!(SettlementProgress::from_evidence(&evidence).is_err());
}

#[test]
fn cumulative_progress_accepts_distinct_receipt_digests() {
    let evidence = json!({
        "settlement_progress": {
            "version": 1,
            "sequence": 2,
            "snapshot_id": "snapshot-2",
            "event_digests": {
                "oid:1:hash-a": "canonical-receipt-digest-a",
                "oid:2:hash-b": "canonical-receipt-digest-b"
            }
        }
    });
    assert_eq!(
        SettlementProgress::from_evidence(&evidence)
            .unwrap()
            .unwrap()
            .sequence,
        2
    );
}
