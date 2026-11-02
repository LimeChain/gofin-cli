//! Run against b6fcb06, before schema 25 was retired.
use gofin_domain::{
    Amount, AssetId, BridgeAllocationOwner, BridgeTransferRecord, BridgeTransferState, Quantity,
    TransitResource,
};
use gofin_store::{Store, SubmissionRow};
use serde_json::json;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let mut store = Store::open(path).unwrap();
    let transfer = BridgeTransferRecord {
        version: 1,
        id: "synthetic-partial-bridge".into(),
        grant_id: Some("synthetic-grant".into()),
        plan_scope: None,
        metadata: json!({"provider":"synthetic","destination_chain":8453}),
        settlement_expectation: None,
        approved_bounds: None,
        prospective_debit: None,
        state: BridgeTransferState::InTransit,
        quote_revisions: vec![],
        steps: vec![],
        allocation_owner: BridgeAllocationOwner::Operation { operation_id: None },
        transit: Some(TransitResource {
            transfer_id: "synthetic-partial-bridge".into(),
            amount: Amount {
                asset: AssetId {
                    network: "ethereum:1".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
                atoms: Quantity::new("1100").unwrap(),
            },
            evidence_id: "synthetic-source-event".into(),
            destination_locator: None,
            proof_digest: "synthetic-source-proof".into(),
            observed_at: "2026-11-01T00:00:00Z".into(),
            spendable: false,
        }),
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    store.save_bridge_transfer(&transfer).unwrap();
    let record = json!({"id":"synthetic-bridge-op","request_id":"synthetic-bridge-request","fingerprint":"synthetic-bridge-fingerprint","status":"awaiting_approval","submission":null,"input":{"transfer_id":"synthetic-partial-bridge"}});
    store
        .create_request(
            "synthetic-bridge-op",
            "synthetic-bridge-request",
            "synthetic-bridge-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &record,
            "2026-11-01T00:00:00Z",
        )
        .unwrap();
    store
        .persist_submission(
            "synthetic-bridge-op",
            &SubmissionRow {
                kind: "synthetic".into(),
                reference: "synthetic-bridge-source".into(),
                data: json!({"transfer_id":"synthetic-partial-bridge"}),
            },
            "2026-11-01T00:00:01Z",
        )
        .unwrap();
}
