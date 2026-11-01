//! Run against b6fcb06, before schema 25 was retired.
use gofin_domain::{
    AssetId, BridgeAllocationOwner, BridgeStepKind, BridgeStepLink, BridgeStepState,
    BridgeTransferRecord, BridgeTransferState,
};
use gofin_store::{Store, SubmissionRow};
use serde_json::json;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let mut store = Store::open(path).unwrap();
    let _asset = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let transfer = BridgeTransferRecord {
        version: 1,
        id: "synthetic-partial-bridge".into(),
        grant_id: None,
        plan_scope: None,
        metadata: json!({"provider":"synthetic","destination_chain":8453}),
        settlement_expectation: None,
        approved_bounds: None,
        prospective_debit: None,
        state: BridgeTransferState::Prepared,
        quote_revisions: vec![],
        steps: vec![
            BridgeStepLink {
                transfer_id: "synthetic-partial-bridge".into(),
                grant_id: None,
                plan_authority: None,
                step_id: "source-deposit".into(),
                operation_id: None,
                kind: BridgeStepKind::Deposit,
                depends_on: vec![],
                state: BridgeStepState::Unsubmitted,
            },
            BridgeStepLink {
                transfer_id: "synthetic-partial-bridge".into(),
                grant_id: None,
                plan_authority: None,
                step_id: "destination-finalize".into(),
                operation_id: None,
                kind: BridgeStepKind::Finalize,
                depends_on: vec!["source-deposit".into()],
                state: BridgeStepState::Unsubmitted,
            },
        ],
        allocation_owner: BridgeAllocationOwner::Operation { operation_id: None },
        transit: None,
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    let record = json!({"id":"synthetic-bridge-op","request_id":"synthetic-bridge-request","fingerprint":"synthetic-bridge-fingerprint","status":"awaiting_approval","submission":null,"input":{"transfer_id":"synthetic-partial-bridge"}});
    store.save_bridge_transfer(&transfer).unwrap();
    let mut submitted = transfer.steps[0].clone();
    submitted.operation_id = Some("synthetic-bridge-op".into());
    submitted.state = BridgeStepState::Submitted;
    store
        .create_bridge_attempt(
            "synthetic-bridge-op",
            "synthetic-bridge-request",
            "synthetic-bridge-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &record,
            "2026-11-01T00:00:00Z",
            &submitted,
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
