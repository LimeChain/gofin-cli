use gofin_store::{Store, SubmissionRow};
use serde_json::json;
use sha2::Digest;
use std::sync::{Arc, Barrier};
const FUTURE: &str = "2099-01-01T00:00:00Z";
fn request(store: &mut Store, id: &str) {
    let record = json!({"id":id,"request_id":format!("request-{id}"),"fingerprint":format!("hash-{id}"),"status":"awaiting_approval","submission":null});
    store
        .create_request(
            id,
            &format!("request-{id}"),
            &format!("hash-{id}"),
            "synthetic.write",
            None,
            "awaiting_approval",
            &record,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
}
#[test]
fn continuation_claim_is_atomic_and_submitted_failure_consumes_stage_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    request(&mut store, "attempt-a");
    request(&mut store, "attempt-b");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "approval-1",
            &json!({"native":"terms"}),
            FUTURE,
        )
        .unwrap();
    let issued = store
        .issue_stage_continuation("action", "bridge-deposit", "approval-1")
        .unwrap();
    assert_eq!(
        store
            .issue_stage_continuation("action", "bridge-deposit", "approval-1")
            .unwrap()
            .token,
        issued.token
    );
    drop(store);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["attempt-a", "attempt-b"]
        .into_iter()
        .map(|id| {
            let path = path.clone();
            let barrier = barrier.clone();
            let token = issued.token.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                barrier.wait();
                store
                    .claim_stage_attempt(&token, id, &json!({"candidate":id}))
                    .unwrap()
            })
        })
        .collect();
    let claims: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(claims.iter().filter(|c| c.created).count(), 1);
    assert_eq!(claims[0].attempt, claims[1].attempt);
    let mut store = Store::open(&path).unwrap();
    let row = SubmissionRow {
        kind: "synthetic".into(),
        reference: "immutable-receipt".into(),
        data: json!({"nonce":7}),
    };
    store
        .admit_stage_submission(
            &issued.token,
            &row,
            "2026-01-01T00:00:01Z",
            &json!({"native":"terms"}),
        )
        .unwrap();
    assert!(
        store
            .admit_stage_submission(&issued.token, &row, "2026-01-01T00:00:01Z", &json!({}))
            .is_err()
    );
    store
        .finish_stage_attempt(&issued.token, "failed", &json!({"proof":"reverted"}))
        .unwrap();
    assert!(
        store
            .issue_stage_continuation("action", "bridge-deposit", "approval-1")
            .is_err()
    );
    store
        .register_action_authority(
            "action",
            FUTURE,
            "approval-2",
            &json!({"native":"terms","nonce":"new"}),
            FUTURE,
        )
        .unwrap();
    let retry = store
        .issue_stage_continuation("action", "bridge-deposit", "approval-2")
        .unwrap();
    assert_ne!(retry.token, issued.token);
    assert_eq!(retry.generation, 2);
    assert_eq!(
        store
            .stage_attempt(&issued.token)
            .unwrap()
            .unwrap()
            .result
            .unwrap()["proof"],
        "reverted"
    );
}
#[test]
fn rejected_or_unsubmitted_attempt_reuses_authority_but_completed_stage_is_immutable() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    request(&mut store, "attempt");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "approval",
            &json!({"fee_cap":"1"}),
            FUTURE,
        )
        .unwrap();
    let first = store
        .issue_stage_continuation("action", "stage", "approval")
        .unwrap();
    store
        .finish_stage_attempt(
            &first.token,
            "not_dispatched",
            &json!({"proof":"never_sent"}),
        )
        .unwrap();
    let second = store
        .issue_stage_continuation("action", "stage", "approval")
        .unwrap();
    store
        .claim_stage_attempt(&second.token, "attempt", &json!({"payload":"one"}))
        .unwrap();
    store
        .admit_stage_submission(
            &second.token,
            &SubmissionRow {
                kind: "test".into(),
                reference: "receipt".into(),
                data: json!({}),
            },
            "2026-01-01T00:00:00Z",
            &json!({"fee_cap":"1"}),
        )
        .unwrap();
    store
        .finish_stage_attempt(&second.token, "completed", &json!({"receipt":"done"}))
        .unwrap();
    assert!(
        store
            .issue_stage_continuation("action", "stage", "approval")
            .is_err()
    );
    assert!(
        store
            .finish_stage_attempt(&second.token, "failed", &json!({}))
            .is_err()
    );
    let replay = store
        .claim_stage_attempt(&second.token, "attempt", &json!({"payload":"different"}))
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replay.attempt.candidate.unwrap()["payload"], "one");
}

#[test]
fn yolo_admission_allows_only_fresh_check_time_for_same_activation() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    let audit = json!({"source":"yolo","binding_hash":"h","challenge_nonce":"n","expires_at":FUTURE,"yolo_provenance":{"revision":4,"generation":"generation-4","checked_at":"2026-11-01T00:00:00Z"}});
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":audit,"intent":{"amount":"5"}}),
            FUTURE,
        )
        .unwrap();
    let stage = store
        .issue_stage_continuation("action", "stage", "authority")
        .unwrap();
    store
        .claim_stage_attempt(&stage.token, "action", &json!({"amount":"5"}))
        .unwrap();
    let submission = SubmissionRow {
        kind: "test".into(),
        reference: "id".into(),
        data: json!({}),
    };
    let mut changed = audit.clone();
    changed["yolo_provenance"]["generation"] = json!("generation-5");
    assert!(
        store
            .admit_stage_submission(&stage.token, &submission, "2026-11-01T00:01:00Z", &changed)
            .is_err()
    );
    let mut refreshed = audit;
    refreshed["yolo_provenance"]["checked_at"] = json!("2026-11-01T00:01:00Z");
    store
        .admit_stage_submission(
            &stage.token,
            &submission,
            "2026-11-01T00:01:00Z",
            &refreshed,
        )
        .unwrap();
}

#[test]
fn action_continuation_binds_one_stage_and_replays_original_claim() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let action = store.issue_action_continuation("action").unwrap();
    assert_eq!(action, store.issue_action_continuation("action").unwrap());
    let a = store
        .issue_stage_continuation("action", "stage-a", "authority")
        .unwrap();
    let b = store
        .issue_stage_continuation("action", "stage-b", "authority")
        .unwrap();
    let first = store
        .bind_action_continuation(&action.token, &a.token)
        .unwrap();
    let duplicate = store
        .bind_action_continuation(&action.token, &b.token)
        .unwrap();
    assert!(first.created);
    assert!(!duplicate.created);
    assert_eq!(duplicate.continuation.stage_token, Some(a.token));
    assert_eq!(
        store.action_continuation(&action.token).unwrap().unwrap(),
        first.continuation
    );
    assert_ne!(
        store.issue_action_continuation("action").unwrap().token,
        action.token
    );
}

#[test]
fn waiting_continuation_is_consumed_with_immutable_result() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let token = store.issue_action_continuation("action").unwrap();
    let completed = store
        .finish_action_continuation(&token.token, &json!({"phase":"in_progress"}))
        .unwrap();
    assert!(completed.created);
    let stage = store
        .issue_stage_continuation("action", "later", "authority")
        .unwrap();
    let replay = store
        .bind_action_continuation(&token.token, &stage.token)
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replay.continuation.result.unwrap()["phase"], "in_progress");
    assert_eq!(
        store
            .latest_action_continuation("action")
            .unwrap()
            .unwrap()
            .token,
        token.token
    );
    assert_ne!(
        store.issue_action_continuation("action").unwrap().token,
        token.token
    );
}

#[test]
fn bridge_stage_request_link_and_claim_commit_together() {
    use gofin_domain::{
        BridgeAllocationOwner, BridgeStepKind, BridgeStepLink, BridgeStepState,
        BridgeTransferRecord, BridgeTransferState,
    };
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let planned = BridgeStepLink {
        transfer_id: "transfer".into(),
        grant_id: None,
        plan_authority: None,
        step_id: "deposit".into(),
        operation_id: None,
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    let transfer = BridgeTransferRecord {
        version: 1,
        id: "transfer".into(),
        grant_id: None,
        plan_scope: None,
        metadata: json!({}),
        settlement_expectation: None,
        approved_bounds: None,
        prospective_debit: None,
        state: BridgeTransferState::Prepared,
        quote_revisions: vec![gofin_domain::BridgeQuoteRevision {
            revision: 1,
            provider: "relay".into(),
            route_id: "route".into(),
            order_id: Some("order-1".into()),
            quote_hash: "quote".into(),
            payload: json!({}),
            observed_at: "2026-11-01T00:00:00Z".into(),
            valid_until: FUTURE.into(),
        }],
        steps: vec![planned.clone()],
        allocation_owner: BridgeAllocationOwner::Operation { operation_id: None },
        transit: None,
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    store.save_bridge_transfer(&transfer).unwrap();
    let stage = store
        .issue_stage_continuation("action", "transfer:deposit", "authority")
        .unwrap();
    let mut link = planned;
    link.operation_id = Some("child".into());
    link.state = BridgeStepState::Submitted;
    let record = json!({"id":"child","status":"awaiting_approval","submission":null});
    let candidate = json!({"amount":"10"});
    let mut wrong = link.clone();
    wrong.step_id = "unknown".into();
    assert!(
        store
            .create_bridge_stage_attempt(
                "child",
                "child-request",
                "hash",
                "bridge.transfer",
                None,
                "awaiting_approval",
                &record,
                "2026-11-01T00:00:00Z",
                &wrong,
                &stage.token,
                &candidate
            )
            .is_err()
    );
    assert!(store.get_record("child").unwrap().is_none());
    assert_eq!(
        store.stage_attempt(&stage.token).unwrap().unwrap().state,
        "issued"
    );
    let (row, claim) = store
        .create_bridge_stage_attempt(
            "child",
            "child-request",
            "hash",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &record,
            "2026-11-01T00:00:00Z",
            &link,
            &stage.token,
            &candidate,
        )
        .unwrap();
    assert!(row.created && claim.created);
    assert_eq!(
        store.bridge_transfer("transfer").unwrap().unwrap().steps[0]
            .operation_id
            .as_deref(),
        Some("child")
    );
    let (_, duplicate) = store
        .create_bridge_stage_attempt(
            "child",
            "child-request",
            "hash",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &record,
            "2026-11-01T00:00:00Z",
            &link,
            &stage.token,
            &candidate,
        )
        .unwrap();
    assert!(!duplicate.created);
    assert_eq!(duplicate.attempt.candidate, Some(candidate));
    let submission = SubmissionRow {
        kind: "synthetic".into(),
        reference: "bridge-source".into(),
        data: json!({"nonce":1}),
    };
    store
        .admit_stage_submission(
            &stage.token,
            &submission,
            "2026-11-01T00:00:01Z",
            &json!("one"),
        )
        .unwrap();
    let terminal = json!({"id":"child","operation":"bridge.transfer","status":"completed","submission":{"kind":"synthetic","reference":"bridge-source","data":{"nonce":1}},"effects":{"proof_digest":"source-proof","evidence":{"correlation":"order-1"},"effects":[]}});
    let transit = gofin_domain::TransitResource {
        transfer_id: "transfer".into(),
        amount: gofin_domain::Amount {
            asset: gofin_domain::AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            atoms: gofin_domain::Quantity::new("10").unwrap(),
        },
        evidence_id: "source-event".into(),
        destination_locator: None,
        proof_digest: "source-proof".into(),
        observed_at: "2026-11-01T00:00:02Z".into(),
        spendable: false,
    };
    let mut bad = transit.clone();
    bad.proof_digest = "different".into();
    assert!(
        store
            .record_bridge_stage_observation("child", &terminal, Some(&bad), "2026-11-01T00:00:02Z")
            .is_err()
    );
    assert_eq!(
        store.get_record("child").unwrap().unwrap()["status"],
        "submitted"
    );
    store
        .record_bridge_stage_observation("child", &terminal, Some(&transit), "2026-11-01T00:00:02Z")
        .unwrap();
    store
        .record_bridge_stage_observation("child", &terminal, Some(&transit), "2026-11-01T00:00:02Z")
        .unwrap();
    let mut refined = terminal.clone();
    refined["effects"]["effects"] = json!([{"kind":"cost","amount":{"atoms":"3"}}]);
    refined["effects"]["proof_digest"] = json!("source-proof-with-fee");
    let mut refined_transit = transit.clone();
    refined_transit.proof_digest = "source-proof-with-fee".into();
    store
        .record_bridge_stage_observation(
            "child",
            &refined,
            Some(&refined_transit),
            "2026-11-01T00:00:03Z",
        )
        .unwrap();
    assert_eq!(
        store.get_record("child").unwrap().unwrap()["effects"]["effects"],
        refined["effects"]["effects"]
    );
    let mut conflicting = refined.clone();
    conflicting["effects"]["evidence"]["correlation"] = json!("different-order");
    assert!(
        store
            .record_bridge_stage_observation(
                "child",
                &conflicting,
                Some(&refined_transit),
                "2026-11-01T00:00:04Z"
            )
            .is_err()
    );
    let settled = store.bridge_transfer("transfer").unwrap().unwrap();
    assert_eq!(settled.state, BridgeTransferState::InTransit);
    assert_eq!(settled.steps[0].state, BridgeStepState::Settled);
    assert_eq!(settled.transit, Some(transit));
    let mut proof = gofin_domain::BridgeSettlementProof {
        transfer_id: "transfer".into(),
        outcome: gofin_domain::BridgeSettlementOutcome::Destination,
        evidence_id: "destination-event".into(),
        effects: vec![],
        evidence: json!({"correlation":"order-1"}),
        observed_at: "2026-11-01T00:00:03Z".into(),
        proof_digest: String::new(),
    };
    proof.proof_digest = gofin_domain::bridge_settlement_digest(&proof).unwrap();
    let completed = store
        .record_bridge_transfer_observation(&proof, "2026-11-01T00:00:03Z")
        .unwrap();
    assert_eq!(completed.state, BridgeTransferState::Completed);
    assert_eq!(completed.settlement_proofs, vec![proof.clone()]);
    let mut refinement = proof.clone();
    refinement.evidence_id = format!("{}:native-cost-v1", proof.evidence_id);
    refinement.effects.push(serde_json::from_value(json!({"kind":"cost","location":{"kind":"cash","resource":"ethereum:1:owner"},"amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"3"},"category":"gas","treatment":"additional_debit"})).unwrap());
    refinement.proof_digest = gofin_domain::bridge_settlement_digest(&refinement).unwrap();
    let mut forged = refinement.clone();
    forged.evidence["correlation"] = json!("different");
    forged.proof_digest = gofin_domain::bridge_settlement_digest(&forged).unwrap();
    assert!(
        store
            .record_bridge_cost_refinement(&proof.proof_digest, &forged, "2026-11-01T00:00:04Z")
            .is_err()
    );
    let updated = store
        .record_bridge_cost_refinement(&proof.proof_digest, &refinement, "2026-11-01T00:00:04Z")
        .unwrap();
    assert_eq!(
        updated.settlement_proofs,
        vec![proof.clone(), refinement.clone()]
    );
    assert_eq!(
        store
            .record_bridge_cost_refinement(&proof.proof_digest, &refinement, "2026-11-01T00:00:04Z")
            .unwrap(),
        updated
    );
    assert_eq!(
        store
            .record_bridge_transfer_observation(&proof, "2026-11-01T00:00:04Z")
            .unwrap(),
        updated
    );
}

#[test]
fn evm_nonce_claims_are_cross_connection_unique_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonces.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "a");
    request(&mut store, "b");
    drop(store);
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = ["a", "a", "b"]
        .into_iter()
        .map(|id| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                barrier.wait();
                (
                    id,
                    store.claim_evm_nonce("ethereum:1:sender", id, 7).unwrap(),
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let a: Vec<_> = results
        .iter()
        .filter(|(id, _)| *id == "a")
        .map(|(_, nonce)| *nonce)
        .collect();
    let b = results.iter().find(|(id, _)| *id == "b").unwrap().1;
    assert_eq!(a[0], a[1]);
    assert_ne!(a[0], b);
    assert_eq!([a[0], b].into_iter().min(), Some(7));
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .claim_evm_nonce("ethereum:1:sender", "a", 100)
            .unwrap(),
        a[0]
    );
    assert!(store.claim_evm_nonce("ethereum:1:other", "a", 7).is_err());
    assert!(store.release_evm_nonce_if_unsubmitted("a").is_err());
}

#[test]
fn legacy_evm_nonce_seeds_matching_scope_and_admission_checks_exact_payload() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "legacy");
    request(&mut store, "action");
    let tx = json!({"chain_id":1,"sender":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","nonce":7});
    store
        .persist_submission(
            "legacy",
            &SubmissionRow {
                kind: "ethereum".into(),
                reference: "legacy-hash".into(),
                data: json!({"transaction":tx}),
            },
            "2026-11-01T00:00:00Z",
        )
        .unwrap();
    let scope = "ethereum:1:0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    assert_eq!(store.claim_evm_nonce(scope, "action", 7).unwrap(), 8);
    store
        .register_action_authority(
            "action",
            FUTURE,
            "approval",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let stage = store
        .issue_stage_continuation("action", "standalone", "approval")
        .unwrap();
    let transaction =
        json!({"chain_id":1,"sender":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","nonce":8});
    store
        .claim_stage_attempt(
            &stage.token,
            "action",
            &json!({"binding":{"transaction":transaction}}),
        )
        .unwrap();
    let mut wrong = transaction.clone();
    wrong["nonce"] = json!(9);
    let wrong = SubmissionRow {
        kind: "ethereum".into(),
        reference: "bad-hash".into(),
        data: json!({"transaction":wrong}),
    };
    assert!(
        store
            .admit_stage_submission(&stage.token, &wrong, "2026-11-01T00:00:01Z", &json!("one"))
            .is_err()
    );
    assert!(store.submission("action").unwrap().is_none());
    let valid = SubmissionRow {
        kind: "ethereum".into(),
        reference: "good-hash".into(),
        data: json!({"transaction":transaction}),
    };
    store
        .admit_stage_submission(&stage.token, &valid, "2026-11-01T00:00:01Z", &json!("one"))
        .unwrap();
    assert!(store.release_evm_nonce_if_unsubmitted("action").is_err());
    assert!(store.record_evm_definitive_rejection(&stage.token,&json!({"source":"rpc","definitive_nonexecution":true,"submission_reference":"wrong"})).is_err());
    let proof =
        json!({"source":"rpc","definitive_nonexecution":true,"submission_reference":"good-hash"});
    store
        .record_evm_definitive_rejection(&stage.token, &proof)
        .unwrap();
    store
        .record_evm_definitive_rejection(&stage.token, &proof)
        .unwrap();
    assert_eq!(
        store.submission("action").unwrap().unwrap().reference,
        "good-hash"
    );
    request(&mut store, "retry");
    assert_eq!(store.claim_evm_nonce(scope, "retry", 8).unwrap(), 8);
}

#[test]
fn bridge_draft_nonce_claim_can_be_adopted_without_changing_authority() {
    use gofin_domain::{
        BridgeAllocationOwner, BridgeStepKind, BridgeStepLink, BridgeStepState,
        BridgeTransferRecord, BridgeTransferState,
    };
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let planned = BridgeStepLink {
        transfer_id: "transfer".into(),
        grant_id: None,
        plan_authority: None,
        step_id: "deposit".into(),
        operation_id: None,
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    store
        .save_bridge_transfer(&BridgeTransferRecord {
            version: 1,
            id: "transfer".into(),
            grant_id: None,
            plan_scope: None,
            metadata: json!({}),
            settlement_expectation: None,
            approved_bounds: None,
            prospective_debit: None,
            state: BridgeTransferState::Prepared,
            quote_revisions: vec![],
            steps: vec![planned.clone()],
            allocation_owner: BridgeAllocationOwner::Operation { operation_id: None },
            transit: None,
            applied_proofs: vec![],
            settlement_proofs: vec![],
        })
        .unwrap();
    let draft = json!({"id":"child","status":"awaiting_approval","submission":null,"input":{"amount":"1"},"approval":{"binding_hash":"original"},"preview":{"nonce":0},"updated_at":"t0"});
    store
        .create_request(
            "child",
            "child-request",
            "hash",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &draft,
            "t0",
        )
        .unwrap();
    store
        .claim_evm_nonce("ethereum:1:sender", "child", 0)
        .unwrap();
    let stage = store
        .issue_stage_continuation("action", "transfer:deposit", "authority")
        .unwrap();
    let mut link = planned;
    link.operation_id = Some("child".into());
    link.state = BridgeStepState::Submitted;
    let mut changed = draft.clone();
    changed["approval"]["binding_hash"] = json!("changed");
    assert!(
        store
            .create_bridge_stage_attempt(
                "child",
                "child-request",
                "hash",
                "bridge.transfer",
                None,
                "awaiting_approval",
                &changed,
                "t1",
                &link,
                &stage.token,
                &json!({})
            )
            .is_err()
    );
    assert_eq!(store.get_record("child").unwrap().unwrap(), draft);
    let mut revised = draft;
    revised["preview"]["nonce"] = json!(1);
    revised["updated_at"] = json!("t1");
    let (row, claim) = store
        .create_bridge_stage_attempt(
            "child",
            "child-request",
            "hash",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &revised,
            "t1",
            &link,
            &stage.token,
            &json!({"nonce":1}),
        )
        .unwrap();
    assert!(row.created && claim.created);
    assert_eq!(store.get_record("child").unwrap().unwrap(), revised);
}

#[test]
fn conclusively_unsubmitted_nonce_can_fill_gap_without_reusing_live_claims() {
    let mut store = Store::in_memory().unwrap();
    for id in ["a", "b", "c"] {
        request(&mut store, id)
    }
    assert_eq!(
        store.claim_evm_nonce("ethereum:1:sender", "a", 0).unwrap(),
        0
    );
    assert_eq!(
        store.claim_evm_nonce("ethereum:1:sender", "b", 0).unwrap(),
        1
    );
    assert!(store.release_evm_nonce_if_unsubmitted("a").is_err());
    let mut rejected = store.get_record("a").unwrap().unwrap();
    rejected["status"] = json!("rejected");
    store
        .update_record("a", &rejected, "2026-11-01T00:00:00Z")
        .unwrap();
    assert!(store.release_evm_nonce_if_unsubmitted("a").unwrap());
    assert_eq!(
        store.claim_evm_nonce("ethereum:1:sender", "c", 0).unwrap(),
        0
    );
    assert_eq!(
        store.claim_evm_nonce("ethereum:1:sender", "b", 0).unwrap(),
        1
    );
}

#[test]
fn action_lease_serializes_processes_and_claimed_candidate_refresh_is_cas() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lease.sqlite");
    let mut first = Store::open(&path).unwrap();
    request(&mut first, "action");
    request(&mut first, "child");
    first
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let stage = first
        .issue_stage_continuation("action", "stage", "authority")
        .unwrap();
    let old = json!({"binding":{"nonce":1}});
    let new = json!({"binding":{"nonce":2}});
    let mut record = first.get_record("child").unwrap().unwrap();
    record["preview"] = old.clone();
    record["updated_at"] = json!("t1");
    first.update_record("child", &record, "t1").unwrap();
    first
        .claim_stage_attempt(&stage.token, "child", &old)
        .unwrap();
    let lease = first.try_acquire_action_lease("action").unwrap();
    let second = Store::open(&path).unwrap();
    assert!(second.try_acquire_action_lease("action").is_err());
    let mut revised = record.clone();
    revised["preview"] = new.clone();
    revised["updated_at"] = json!("t2");
    let mut tampered = revised.clone();
    tampered["request_id"] = json!("other");
    assert!(
        first
            .refresh_claimed_stage(&lease, &stage.token, &old, &new, &tampered, "t2")
            .is_err()
    );
    first
        .refresh_claimed_stage(&lease, &stage.token, &old, &new, &revised, "t2")
        .unwrap();
    assert!(
        first
            .refresh_claimed_stage(&lease, &stage.token, &old, &new, &revised, "t2")
            .is_err()
    );
    drop(lease);
    assert!(second.try_acquire_action_lease("action").is_ok());
}

#[test]
fn passive_successor_is_atomic_with_stage_binding_and_wait_result() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let action = store.issue_action_continuation("action").unwrap();
    let stage = store
        .issue_stage_continuation("action", "stage", "authority")
        .unwrap();
    let lease = store.try_acquire_action_lease("action").unwrap();
    let (claim, next) = store
        .bind_and_issue_action_continuation(&lease, &action.token, &stage.token)
        .unwrap();
    assert!(claim.created);
    let next = next.unwrap();
    let (replay, replayed_next) = store
        .bind_and_issue_action_continuation(&lease, &action.token, &stage.token)
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replayed_next, Some(next.clone()));
    let (wait, third) = store
        .finish_and_issue_action_continuation(&lease, &next.token, &json!({"phase":"in_progress"}))
        .unwrap();
    assert!(wait.created);
    let third = third.unwrap();
    let (replay, duplicate_third) = store
        .finish_and_issue_action_continuation(&lease, &next.token, &json!({"phase":"changed"}))
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replay.continuation.result.unwrap()["phase"], "in_progress");
    assert_eq!(duplicate_third, Some(third));
}

#[test]
fn new_continuation_can_resume_claimed_stage_only_under_exclusive_lease() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    request(&mut store, "child");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let first = store.issue_action_continuation("action").unwrap();
    let stage = store
        .issue_stage_continuation("action", "stage", "authority")
        .unwrap();
    let lease = store.try_acquire_action_lease("action").unwrap();
    let (_, next) = store
        .bind_and_issue_action_continuation(&lease, &first.token, &stage.token)
        .unwrap();
    store
        .claim_stage_attempt(&stage.token, "child", &json!({"binding":{}}))
        .unwrap();
    drop(lease);
    let lease = store.try_acquire_action_lease("action").unwrap();
    let (resumed, _) = store
        .bind_and_issue_action_continuation(&lease, &next.unwrap().token, &stage.token)
        .unwrap();
    assert!(resumed.created);
    assert_eq!(resumed.continuation.stage_token, Some(stage.token));
}

#[test]
fn reauthorization_claim_is_idempotent_and_completion_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reauthorization.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "original",
            &json!({"audit":{"old":true}}),
            FUTURE,
        )
        .unwrap();
    let token = store.issue_action_continuation("action").unwrap().token;
    let lease = store.try_acquire_action_lease("action").unwrap();
    let candidate = json!({"input":{"amount":"2"},"deadline":FUTURE});
    assert!(
        store
            .claim_reauthorization(&lease, &token, "hash", &candidate)
            .unwrap()
            .created
    );
    assert!(
        !store
            .claim_reauthorization(&lease, &token, "hash", &candidate)
            .unwrap()
            .created
    );
    assert!(
        store
            .claim_reauthorization(&lease, &token, "other", &candidate)
            .is_err()
    );
    let before = store.get_record("action").unwrap().unwrap();
    let mut after = before.clone();
    after["input"] = json!({"amount":"2"});
    after["preview"] = candidate.clone();
    after["approval"] = json!({"new":true});
    after["updated_at"] = json!("2098-01-01T00:00:00Z");
    let authority = json!({"prepared":candidate,"audit":{"new":true}});
    assert!(
        store
            .complete_reauthorization(
                &lease,
                &token,
                "hash",
                "new",
                &json!({"prepared":{},"audit":{"new":true}}),
                FUTURE,
                FUTURE,
                &before,
                &after,
                &json!({"approved":true})
            )
            .is_err()
    );
    let (completed, next) = store
        .complete_reauthorization(
            &lease,
            &token,
            "hash",
            "new",
            &authority,
            FUTURE,
            FUTURE,
            &before,
            &after,
            &json!({"approved":true}),
        )
        .unwrap();
    assert_eq!(completed.authority_id.as_deref(), Some("new"));
    assert!(next.is_some());
    assert_eq!(store.get_record("action").unwrap().unwrap(), after);
    assert_eq!(
        store.action_authority("action", "new").unwrap(),
        Some(authority)
    );
    let replay = store
        .complete_reauthorization(
            &lease,
            &token,
            "hash",
            "new",
            &json!({}),
            FUTURE,
            FUTURE,
            &before,
            &after,
            &json!({}),
        )
        .unwrap();
    assert_eq!(replay.0.result, Some(json!({"approved":true})));
    assert_eq!(replay.1, next);
}

#[test]
fn interrupted_reauthorization_stays_pending_until_explicit_abandonment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("interrupted-reauthorization.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "original",
            &json!({"audit":{"old":true}}),
            FUTURE,
        )
        .unwrap();
    let token = store.issue_action_continuation("action").unwrap().token;
    let lease = store.try_acquire_action_lease("action").unwrap();
    store
        .claim_reauthorization(&lease, &token, "hash", &json!({"candidate":1}))
        .unwrap();
    drop(lease);
    drop(store);
    let mut store = Store::open(&path).unwrap();
    let lease = store.try_acquire_action_lease("action").unwrap();
    let claim = store.reauthorization_claim(&token).unwrap().unwrap();
    assert!(claim.result.is_none());
    assert!(
        !store
            .claim_reauthorization(&lease, &token, "hash", &json!({"candidate":1}))
            .unwrap()
            .created
    );
    let (abandoned, next) = store
        .abandon_reauthorization(&lease, &token, "hash")
        .unwrap();
    assert_eq!(
        abandoned.result,
        Some(json!({"reauthorization":"abandoned"}))
    );
    let (again, repeated_next) = store
        .abandon_reauthorization(&lease, &token, "hash")
        .unwrap();
    assert_eq!(again, abandoned);
    assert_eq!(next, repeated_next);
    assert_ne!(next.unwrap().token, token);
    assert!(store.action_authority("action", "new").unwrap().is_none());
}

#[test]
fn submitted_parent_remains_immutable_when_reauthorized() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reauthorize-submitted.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "original",
            &json!({"audit":{"old":true}}),
            FUTURE,
        )
        .unwrap();
    store
        .persist_submission(
            "action",
            &SubmissionRow {
                kind: "synthetic".into(),
                reference: "original-receipt".into(),
                data: json!({"original":true}),
            },
            "2026-11-01T00:00:00Z",
        )
        .unwrap();
    let before = store.get_record("action").unwrap().unwrap();
    let token = store.issue_action_continuation("action").unwrap().token;
    let lease = store.try_acquire_action_lease("action").unwrap();
    let candidate = json!({"input":{"amount":"2"}});
    store
        .claim_reauthorization(&lease, &token, "hash", &candidate)
        .unwrap();
    let authority = json!({"prepared":candidate,"audit":{"new":true}});
    let mut modified = before.clone();
    modified["approval"] = json!({"new":true});
    assert!(
        store
            .complete_reauthorization(
                &lease,
                &token,
                "hash",
                "new",
                &authority,
                FUTURE,
                FUTURE,
                &before,
                &modified,
                &json!({"approved":true})
            )
            .is_err()
    );
    store
        .complete_reauthorization(
            &lease,
            &token,
            "hash",
            "new",
            &authority,
            FUTURE,
            FUTURE,
            &before,
            &before,
            &json!({"approved":true}),
        )
        .unwrap();
    assert_eq!(store.get_record("action").unwrap().unwrap(), before);
    assert_eq!(
        store.submission("action").unwrap().unwrap().reference,
        "original-receipt"
    );
    assert_eq!(
        store.current_action_authority("action").unwrap(),
        Some(("new".into(), authority))
    );
}

#[test]
fn verified_position_anchors_are_indexed_by_identity_and_exclude_current_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("anchors.sqlite");
    let mut store = Store::open(&path).unwrap();
    let asset = json!({"network":"synthetic","reference":"SOL","decimals":9});
    let mut records = Vec::new();
    for (id, at, amount) in [
        ("older", "2026-11-01T00:00:01Z", "10"),
        ("newer", "2026-11-01T00:00:02Z", "7"),
    ] {
        request(&mut store, id);
        let submission = SubmissionRow {
            kind: "synthetic".into(),
            reference: format!("receipt-{id}"),
            data: json!({"id":id}),
        };
        store.persist_submission(id, &submission, at).unwrap();
        let anchor = json!({"resource":"synthetic:account","position":"SOL:long","position_kind":"derivative_long","principal":{"asset":asset,"atoms":amount},"scaled_receipt":null,"evidence_version":1,"evidence":{"id":id},"observed_at":at});
        let mut proof:gofin_domain::VerifiedEffects=serde_json::from_value(json!({"evidence_version":1,"submission":{"kind":submission.kind,"reference":submission.reference,"data":submission.data},"effects":[{"kind":"position_change","resource":"synthetic:account","position":"SOL:long","asset":asset,"delta":"-3","scaled_receipt":null,"anchor":anchor}],"evidence":{"source":"synthetic"},"observed_at":at,"proof_digest":""})).unwrap();
        proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
        let mut record = store.get_record(id).unwrap().unwrap();
        record["status"] = json!("completed");
        record["submission"] = serde_json::to_value(&proof.submission).unwrap();
        record["effects"] = serde_json::to_value(&proof).unwrap();
        store.update_record(id, &record, at).unwrap();
        records.push(record);
    }
    let latest = store.current_position_anchors(None).unwrap();
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].principal.atoms.as_str(), "7");
    let prior = store.current_position_anchors(Some("newer")).unwrap();
    assert_eq!(prior.len(), 1);
    assert_eq!(prior[0].principal.atoms.as_str(), "10");
    store
        .update_record("older", &records[0], "2026-11-01T00:00:03Z")
        .unwrap();
    assert_eq!(
        store.current_position_anchors(None).unwrap()[0]
            .principal
            .atoms
            .as_str(),
        "7"
    );
    drop(store);
    {
        let old = rusqlite::Connection::open(&path).unwrap();
        old.execute_batch("DROP TABLE IF EXISTS preapproval_revisions; DROP TABLE replacement_claims; DROP INDEX replacement_parent_identity; DROP TABLE preapproval_continuations; DROP TABLE verified_position_anchors; PRAGMA user_version=33;").unwrap();
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.current_position_anchors(Some("newer")).unwrap()[0]
            .principal
            .atoms
            .as_str(),
        "10"
    );
    request(&mut store, "conflict");
    let submission = SubmissionRow {
        kind: "synthetic".into(),
        reference: "receipt-conflict".into(),
        data: json!({"id":"conflict"}),
    };
    store
        .persist_submission("conflict", &submission, "2026-11-01T00:00:02Z")
        .unwrap();
    let mut record = store.get_record("conflict").unwrap().unwrap();
    let mut proof = records[1]["effects"].clone();
    proof["submission"] =
        json!({"kind":submission.kind,"reference":submission.reference,"data":submission.data});
    proof["effects"][0]["anchor"]["principal"]["atoms"] = json!("8");
    let mut proof: gofin_domain::VerifiedEffects = serde_json::from_value(proof).unwrap();
    proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
    record["status"] = json!("completed");
    record["submission"] = serde_json::to_value(&proof.submission).unwrap();
    record["effects"] = serde_json::to_value(&proof).unwrap();
    store
        .update_record("conflict", &record, "2026-11-01T00:00:02Z")
        .unwrap();
    assert!(
        store.current_position_anchors(None).unwrap().is_empty(),
        "same-time conflicting anchors must remain unavailable"
    );
    assert_eq!(
        store.current_position_anchors(Some("conflict")).unwrap()[0]
            .principal
            .atoms
            .as_str(),
        "7"
    );
}

#[test]
fn verified_bridge_failure_updates_only_original_link() {
    use gofin_domain::{
        BridgeAllocationOwner, BridgeStepKind, BridgeStepLink, BridgeStepState,
        BridgeTransferRecord, BridgeTransferState,
    };
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "action");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let planned = BridgeStepLink {
        transfer_id: "transfer".into(),
        grant_id: None,
        plan_authority: None,
        step_id: "deposit".into(),
        operation_id: None,
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    let transfer = BridgeTransferRecord {
        version: 1,
        id: "transfer".into(),
        grant_id: None,
        plan_scope: None,
        metadata: json!({}),
        settlement_expectation: None,
        approved_bounds: None,
        prospective_debit: None,
        state: BridgeTransferState::Prepared,
        quote_revisions: vec![gofin_domain::BridgeQuoteRevision {
            revision: 1,
            provider: "relay".into(),
            route_id: "route".into(),
            order_id: Some("order-1".into()),
            quote_hash: "quote".into(),
            payload: json!({}),
            observed_at: "2026-11-01T00:00:00Z".into(),
            valid_until: FUTURE.into(),
        }],
        steps: vec![planned.clone()],
        allocation_owner: BridgeAllocationOwner::Operation { operation_id: None },
        transit: None,
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    store.save_bridge_transfer(&transfer).unwrap();
    let stage = store
        .issue_stage_continuation("action", "transfer:deposit", "authority")
        .unwrap();
    let mut link = planned;
    link.operation_id = Some("child".into());
    link.state = BridgeStepState::Submitted;
    let record = json!({"id":"child","operation":"bridge.transfer","status":"awaiting_approval","submission":null});
    let candidate = json!({"amount":"10"});
    let mut wrong = link.clone();
    wrong.step_id = "unknown".into();
    assert!(
        store
            .create_bridge_stage_attempt(
                "child",
                "child-request",
                "hash",
                "bridge.transfer",
                None,
                "awaiting_approval",
                &record,
                "2026-11-01T00:00:00Z",
                &wrong,
                &stage.token,
                &candidate
            )
            .is_err()
    );
    assert!(store.get_record("child").unwrap().is_none());
    assert_eq!(
        store.stage_attempt(&stage.token).unwrap().unwrap().state,
        "issued"
    );
    let (row, claim) = store
        .create_bridge_stage_attempt(
            "child",
            "child-request",
            "hash",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &record,
            "2026-11-01T00:00:00Z",
            &link,
            &stage.token,
            &candidate,
        )
        .unwrap();
    assert!(row.created && claim.created);
    assert_eq!(
        store.bridge_transfer("transfer").unwrap().unwrap().steps[0]
            .operation_id
            .as_deref(),
        Some("child")
    );
    let (_, duplicate) = store
        .create_bridge_stage_attempt(
            "child",
            "child-request",
            "hash",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &record,
            "2026-11-01T00:00:00Z",
            &link,
            &stage.token,
            &candidate,
        )
        .unwrap();
    assert!(!duplicate.created);
    assert_eq!(duplicate.attempt.candidate, Some(candidate));
    let submission = SubmissionRow {
        kind: "synthetic".into(),
        reference: "bridge-source".into(),
        data: json!({"nonce":1}),
    };
    store
        .admit_stage_submission(
            &stage.token,
            &submission,
            "2026-11-01T00:00:01Z",
            &json!("one"),
        )
        .unwrap();
    let mut proof: gofin_domain::VerifiedEffects = serde_json::from_value(json!({"evidence_version":1,"submission":{"kind":"synthetic","reference":"bridge-source","data":{"nonce":1}},"effects":[],"evidence":{"outcome":"reverted"},"observed_at":"2026-11-01T00:00:02Z","proof_digest":""})).unwrap();
    proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
    let failed = json!({"id":"child","operation":"bridge.transfer","status":"failed","approval":"one","submission":{"kind":"synthetic","reference":"bridge-source","data":{"nonce":1}},"effects":proof});
    let mut forged = failed.clone();
    forged["effects"]["proof_digest"] = json!("wrong");
    assert!(
        store
            .record_bridge_stage_failure("child", &forged, "2026-11-01T00:00:02Z")
            .is_err()
    );
    assert_eq!(
        store.bridge_transfer("transfer").unwrap().unwrap().steps[0].state,
        BridgeStepState::Submitted
    );
    store
        .record_bridge_stage_failure("child", &failed, "2026-11-01T00:00:02Z")
        .unwrap();
    store
        .record_bridge_stage_failure("child", &failed, "2026-11-01T00:00:02Z")
        .unwrap();
    let saved = store.bridge_transfer("transfer").unwrap().unwrap();
    assert_eq!(saved.state, BridgeTransferState::Failed);
    assert_eq!(saved.steps[0].state, BridgeStepState::NeedsAttention);
    assert_eq!(store.get_record("child").unwrap().unwrap(), failed);
}

#[test]
fn transaction_attributed_morpho_anchor_survives_reopen_and_rejects_unrelated_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("morpho.sqlite");
    let mut store = Store::open(&path).unwrap();
    let asset = json!({"network":"ethereum:1","reference":"0xasset","decimals":6});
    for (id, hash, scope) in [
        ("valid", "0xreceipt", "0xreceipt"),
        ("other", "0xother", "0xwrong"),
    ] {
        request(&mut store, id);
        let submission = SubmissionRow {
            kind: "ethereum".into(),
            reference: hash.into(),
            data: json!({}),
        };
        store
            .persist_submission(id, &submission, "2026-11-01T00:00:01Z")
            .unwrap();
        let anchor = json!({"resource":"ethereum:1:wallet","position":"morpho:vault","position_kind":"investment","principal":{"asset":asset,"atoms":"10"},"scaled_receipt":null,"evidence_version":2,"evidence":{"share_scope":"transaction_attributed","transaction_hash":scope,"share_balance":"10"},"observed_at":"2026-11-01T00:00:01Z"});
        let mut proof:gofin_domain::VerifiedEffects=serde_json::from_value(json!({"evidence_version":1,"submission":{"kind":submission.kind,"reference":submission.reference,"data":submission.data},"effects":[{"kind":"position_change","resource":"ethereum:1:wallet","position":"morpho:vault","asset":asset,"delta":"10","scaled_receipt":null,"anchor":anchor}],"evidence":{},"observed_at":"2026-11-01T00:00:01Z","proof_digest":""})).unwrap();
        proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
        let mut record = store.get_record(id).unwrap().unwrap();
        record["status"] = json!("completed");
        record["effects"] = serde_json::to_value(proof).unwrap();
        store
            .update_record(id, &record, "2026-11-01T00:00:01Z")
            .unwrap();
    }
    drop(store);
    let store = Store::open(&path).unwrap();
    let anchors = store.current_position_anchors(None).unwrap();
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].evidence_version, 2);
    assert_eq!(anchors[0].evidence["transaction_hash"], "0xreceipt");
    assert!(
        store
            .current_position_anchors(Some("valid"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reauthorization_clears_only_obsolete_unsubmitted_bridge_fee_error() {
    for (operation, code, allowed) in [
        ("bridge.execute", "FEE_LIMIT", true),
        ("bridge.execute", "BRIDGE_CUMULATIVE_FEE_LIMIT", true),
        ("bridge.execute", "OTHER", false),
        ("other", "FEE_LIMIT", false),
    ] {
        let mut store = Store::in_memory().unwrap();
        request(&mut store, "action");
        let mut before = store.get_record("action").unwrap().unwrap();
        before["operation"] = json!(operation);
        before["error"] = json!({"code":code});
        store
            .update_record("action", &before, "2026-11-01T00:00:00Z")
            .unwrap();
        store
            .register_action_authority(
                "action",
                FUTURE,
                "original",
                &json!({"audit":{"old":true}}),
                FUTURE,
            )
            .unwrap();
        let token = store.issue_action_continuation("action").unwrap().token;
        let lease = store.try_acquire_action_lease("action").unwrap();
        let candidate = json!({"input":{"amount":"2"}});
        store
            .claim_reauthorization(&lease, &token, "hash", &candidate)
            .unwrap();
        let mut after = before.clone();
        after["preview"] = candidate.clone();
        after["approval"] = json!({"new":true});
        after["error"] = json!(null);
        after["updated_at"] = json!("2098-01-01T00:00:00Z");
        let result = store.complete_reauthorization(
            &lease,
            &token,
            "hash",
            "new",
            &json!({"prepared":candidate,"audit":{"new":true}}),
            FUTURE,
            FUTURE,
            &before,
            &after,
            &json!({"approved":true}),
        );
        assert_eq!(result.is_ok(), allowed, "{operation} {code}");
        assert_eq!(
            store.get_record("action").unwrap().unwrap()["error"].is_null(),
            allowed
        );
    }
}

#[test]
fn preapproval_retry_is_claimed_once_and_never_replays_an_uncertain_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preapproval.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    let mut initial = store.get_record("action").unwrap().unwrap();
    initial["input"] = json!({"actionDeadline":4102444800u64,"native":{"amount":"7"}});
    store
        .update_record("action", &initial, "2026-01-01T00:00:00Z")
        .unwrap();
    let before = store.get_record("action").unwrap().unwrap();
    let mut failed = before.clone();
    failed["status"] = json!("failed");
    failed["error"] = json!({"code":"RPC_UNAVAILABLE"});
    failed["last_reconciliation_evidence"] = json!({"engine_predispatch_failure":true});
    let issued = store
        .record_predispatch_failure_and_issue("action", &before, &failed, "2026-01-01T00:00:01Z")
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.latest_preapproval_continuation("action").unwrap(),
        Some(issued.clone())
    );
    let lease = store.try_acquire_action_lease("action").unwrap();
    assert!(
        store
            .claim_preapproval_continuation(&lease, &issued.token, "different")
            .is_err()
    );
    let (claimed, created) = store
        .claim_preapproval_continuation(&lease, &issued.token, "hash-action")
        .unwrap();
    assert!(created);
    assert!(
        !store
            .claim_preapproval_continuation(&lease, &issued.token, "hash-action")
            .unwrap()
            .1
    );
    assert_eq!(
        store.get_record("action").unwrap().unwrap()["input"],
        before["input"]
    );
    assert!(
        store
            .latest_preapproval_continuation("action")
            .unwrap()
            .is_none()
    );
    let successor = format!(
        "preapproval-v1-{}",
        hex::encode(sha2::Sha256::digest(
            serde_json::to_vec(&("gofin-preapproval-continuation-v1", "action", 2i64)).unwrap()
        ))
    );
    assert!(
        store
            .claim_preapproval_continuation(&lease, &successor, "hash-action")
            .is_err()
    );
    drop(lease);
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.get_record("action").unwrap().unwrap()["input"],
        before["input"]
    );
    let lease = store.try_acquire_action_lease("action").unwrap();
    assert!(
        !store
            .claim_preapproval_continuation(&lease, &claimed.token, "hash-action")
            .unwrap()
            .1
    );
    assert!(
        store
            .latest_preapproval_continuation("action")
            .unwrap()
            .is_none()
    );
    let expected = store.get_record("action").unwrap().unwrap();
    let mut failed_again = expected.clone();
    failed_again["status"] = json!("failed");
    let result = json!({"state":"not_dispatched","continuation_token":successor});
    let mut wrong = expected.clone();
    wrong["fingerprint"] = json!("different");
    assert!(
        store
            .interrupt_preapproval_continuation(
                &lease,
                &claimed.token,
                &wrong,
                &failed_again,
                &result,
                "2026-01-01T00:00:02Z"
            )
            .is_err()
    );
    assert_eq!(store.get_record("action").unwrap().unwrap(), expected);
    let (done, next) = store
        .interrupt_preapproval_continuation(
            &lease,
            &claimed.token,
            &expected,
            &failed_again,
            &result,
            "2026-01-01T00:00:02Z",
        )
        .unwrap();
    assert_eq!(done.result, Some(result.clone()));
    assert_eq!(next.token, successor);
    assert_eq!(
        store
            .interrupt_preapproval_continuation(
                &lease,
                &claimed.token,
                &expected,
                &failed_again,
                &result,
                "2026-01-01T00:00:02Z"
            )
            .unwrap()
            .0,
        done
    );
    assert_eq!(
        store
            .latest_preapproval_continuation("action")
            .unwrap()
            .unwrap()
            .token,
        successor
    );
    assert_eq!(
        store.get_record("action").unwrap().unwrap()["input"],
        before["input"]
    );
    assert!(
        store
            .claim_preapproval_continuation(&lease, &successor, "hash-action")
            .unwrap()
            .1
    );
    assert!(
        store
            .finish_preapproval_continuation(
                &lease,
                &claimed.token,
                &json!({"different":true}),
                true
            )
            .is_err()
    );
}

#[test]
fn preapproval_retry_has_one_cross_connection_claimant() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    let before = store.get_record("action").unwrap().unwrap();
    let mut failed = before.clone();
    failed["status"] = json!("failed");
    failed["last_reconciliation_evidence"] = json!({"engine_predispatch_failure":true});
    let token = store
        .record_predispatch_failure_and_issue("action", &before, &failed, "2026-01-01T00:00:01Z")
        .unwrap()
        .token;
    drop(store);
    let start = Arc::new(Barrier::new(2));
    let finish = Arc::new(Barrier::new(2));
    let threads = (0..2)
        .map(|_| {
            let path = path.clone();
            let token = token.clone();
            let start = start.clone();
            let finish = finish.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(&path).unwrap();
                start.wait();
                let lease = store.try_acquire_action_lease("action");
                let won = lease.as_ref().ok().is_some_and(|lease| {
                    store
                        .claim_preapproval_continuation(lease, &token, "hash-action")
                        .unwrap()
                        .1
                });
                finish.wait();
                won
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|won| *won)
            .count(),
        1
    );
}

#[test]
fn migration_backfills_only_engine_attested_unsubmitted_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("migration.sqlite");
    let mut store = Store::open(&path).unwrap();
    for id in [
        "attested",
        "legacy",
        "submitted",
        "retired-run",
        "prior-yolo",
    ] {
        request(&mut store, id);
        if id == "submitted" {
            store
                .persist_submission(
                    id,
                    &SubmissionRow {
                        kind: "synthetic".into(),
                        reference: "receipt".into(),
                        data: json!({}),
                    },
                    "2026-01-01T00:00:00Z",
                )
                .unwrap();
        }
        let mut record = store.get_record(id).unwrap().unwrap();
        record["status"] = json!("failed");
        record["input"] = json!({"actionDeadline":4102444800u64});
        if id == "retired-run" {
            record["run_id"] = json!("old-run");
        }
        if id == "prior-yolo" {
            record["authorization_source"] = json!({"source":"yolo"});
        }
        if id != "legacy" {
            record["last_reconciliation_evidence"] = json!({"engine_predispatch_failure":true});
        }
        store
            .update_record(id, &record, "2026-01-01T00:00:01Z")
            .unwrap();
    }
    drop(store);
    let old = rusqlite::Connection::open(&path).unwrap();
    old.execute_batch("DROP TABLE IF EXISTS preapproval_revisions; DROP TABLE replacement_claims; DROP INDEX replacement_parent_identity; DELETE FROM preapproval_continuations; UPDATE operations SET run_id='old-run' WHERE id='retired-run'; DROP TABLE IF EXISTS preapproval_revisions; DROP TABLE IF EXISTS replacement_claims; DROP INDEX IF EXISTS replacement_parent_identity; PRAGMA user_version=38;").unwrap();
    drop(old);
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .latest_preapproval_continuation("attested")
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .latest_preapproval_continuation("legacy")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .latest_preapproval_continuation("submitted")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .latest_preapproval_continuation("retired-run")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .latest_preapproval_continuation("prior-yolo")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.get_record("attested").unwrap().unwrap()["input"]["actionDeadline"],
        4102444800u64
    );
}

#[test]
fn meteora_v3_anchor_is_correlated_and_backfilled_on_schema39_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meteora.sqlite");
    let mut store = Store::open(&path).unwrap();
    let asset = json!({"network":"solana:mainnet-beta","reference":"liquidity","decimals":0});
    let position = "meteora:dlmm:position:liquidity_units";
    for (id, signature) in [("valid", "sig"), ("forged", "wrong")] {
        let mut record = json!({"id":id,"request_id":format!("request-{id}"),"fingerprint":format!("hash-{id}"),"operation":"meteora.open","status":"awaiting_approval","submission":null});
        store
            .create_request(
                id,
                &format!("request-{id}"),
                &format!("hash-{id}"),
                "meteora.open",
                None,
                "awaiting_approval",
                &record,
                "2026-11-01T00:00:00Z",
            )
            .unwrap();
        let submission = SubmissionRow {
            kind: "solana.transaction".into(),
            reference: format!("{id}-message-hash"),
            data: json!({"chain_signature":format!("{id}-signature")}),
        };
        store
            .persist_submission(id, &submission, "2026-11-01T00:00:01Z")
            .unwrap();
        let evidence_signature = if id == "valid" {
            submission.data["chain_signature"].as_str().unwrap()
        } else {
            signature
        };
        let event = json!({"signature":evidence_signature,"submission":{"kind":submission.kind,"reference":submission.reference,"data":submission.data},"slot":123,"action":"open","liquidity_delta_atoms":"10","token_deltas":[],"fees":[],"rewards":[]});
        let lp = json!({"version":1,"family":"dlmm","pool":"pool","position":"position","anchor_position_id":position,"observed_slot":123,"liquidity_units_atoms":"10","liquidity_units_known":true,"historical_principal_known":true,"token_a":{"asset":asset,"atoms":"1","token_program":"token"},"token_b":{"asset":asset,"atoms":"2","token_program":"token"},"history":[event]});
        let anchor = json!({"resource":"solana:mainnet-beta:wallet","position":position,"position_kind":"liquidity","principal":{"asset":asset,"atoms":"10"},"scaled_receipt":null,"evidence_version":3,"evidence":{"version":3,"source":"finalized_meteora_liquidity_unit_delta","basis":"stable_protocol_liquidity_units","signature":evidence_signature,"liquidity_position":lp},"observed_at":"2026-11-01T00:00:01Z"});
        let mut proof:gofin_domain::VerifiedEffects=serde_json::from_value(json!({"evidence_version":1,"submission":{"kind":submission.kind,"reference":submission.reference,"data":submission.data},"effects":[{"kind":"position_change","resource":"solana:mainnet-beta:wallet","position":position,"asset":asset,"delta":"10","scaled_receipt":null,"anchor":anchor}],"evidence":{},"observed_at":"2026-11-01T00:00:01Z","proof_digest":""})).unwrap();
        proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
        record = store.get_record(id).unwrap().unwrap();
        record["status"] = json!("completed");
        record["effects"] = serde_json::to_value(proof).unwrap();
        store
            .update_record(id, &record, "2026-11-01T00:00:01Z")
            .unwrap();
    }
    assert_eq!(store.current_position_anchors(None).unwrap().len(), 1);
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE IF EXISTS preapproval_revisions; DROP TABLE replacement_claims; DROP INDEX replacement_parent_identity; DELETE FROM verified_position_anchors; PRAGMA user_version=39;")
        .unwrap();
    drop(conn);
    let store = Store::open(&path).unwrap();
    let anchors = store.current_position_anchors(None).unwrap();
    assert_eq!(anchors.len(), 1);
    assert_eq!(
        anchors[0].evidence["liquidity_position"]["history"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .current_position_anchors(Some("valid"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn concurrent_replacements_claim_one_parent_and_preserve_submission() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replacement.sqlite");
    let mut store = Store::open(&path).unwrap();
    for id in ["parent", "child-a", "child-b"] {
        request(&mut store, id);
    }
    let row = SubmissionRow {
        kind: "order".into(),
        reference: "original".into(),
        data: json!({"venue":"synthetic"}),
    };
    store
        .persist_submission("parent", &row, "2026-01-01T00:00:01Z")
        .unwrap();
    drop(store);
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = ["child-a", "child-b"]
        .into_iter()
        .map(|id| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                let submission = gofin_domain::Submission {
                    kind: "order".into(),
                    reference: "original".into(),
                    data: json!({"venue":"synthetic"}),
                };
                barrier.wait();
                store.claim_replacement("parent", id, &submission).is_ok()
            })
        })
        .collect();
    let successes = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(successes, 1);
    let store = Store::open(path).unwrap();
    assert_eq!(store.submission("parent").unwrap(), Some(row));
}

#[test]
fn stale_observation_compare_and_swap_preserves_newer_record() {
    let mut store = Store::in_memory().unwrap();
    request(&mut store, "cas");
    let before = store.get_record("cas").unwrap().unwrap();
    let mut newer = before.clone();
    newer["note"] = json!("newer");
    newer["status"] = json!("completed");
    store
        .update_record("cas", &newer, "2026-01-01T00:00:01Z")
        .unwrap();
    let mut stale = before.clone();
    stale["note"] = json!("stale");
    assert!(
        !store
            .update_record_if_unchanged("cas", &before, &stale, "2026-01-01T00:00:02Z")
            .unwrap()
    );
    assert_eq!(store.get_record("cas").unwrap().unwrap(), newer);
}

#[test]
fn corrupt_and_future_observations_are_isolated_without_losing_good_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("observations.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "observe");
    let facts:gofin_domain::outcome::OutcomeFacts=serde_json::from_value(json!({"dispatch":"dispatched","result":"pending","attention":{"kind":"none"},"reporting":"partial","observed_at":"2026-01-01T00:00:00Z"})).unwrap();
    store
        .append_outcome_observation("observe", &facts, "good")
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("INSERT INTO outcome_observations(operation_id,facts_json,source) VALUES ('observe','not-json','bad')",[]).unwrap();
    db.execute("INSERT INTO outcome_observations(operation_id,facts_json,source) VALUES ('observe','{\"dispatch\":\"future\"}','future')",[]).unwrap();
    let (rows, incomplete) = store.outcome_observations_isolated("observe").unwrap();
    assert!(incomplete);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, "good");
    assert_eq!(store.outcome_observations("observe").unwrap().len(), 1);
}

#[test]
fn recovered_nonce_claim_requires_action_lease_and_no_submission() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    request(&mut store, "child");
    request(&mut store, "other");
    store
        .register_action_authority(
            "action",
            FUTURE,
            "authority",
            &json!({"audit":"one"}),
            FUTURE,
        )
        .unwrap();
    let stage = store
        .issue_stage_continuation("action", "stage", "authority")
        .unwrap();
    store
        .claim_stage_attempt(&stage.token, "child", &json!({"binding":{}}))
        .unwrap();
    assert_eq!(
        store
            .claim_evm_nonce("ethereum:1:sender", "child", 7)
            .unwrap(),
        7
    );
    assert_eq!(
        store
            .claim_evm_nonce("ethereum:1:sender", "child", 7)
            .unwrap(),
        7
    );
    let lease = store.try_acquire_action_lease("action").unwrap();
    let second = Store::open(&path).unwrap();
    assert!(second.try_acquire_action_lease("action").is_err());
    assert!(store.release_orphaned_evm_nonce(&lease, "other").is_err());
    assert!(store.release_orphaned_evm_nonce(&lease, "child").unwrap());
    assert_eq!(
        store
            .claim_evm_nonce("ethereum:1:sender", "other", 7)
            .unwrap(),
        7
    );
    assert_eq!(
        store
            .claim_evm_nonce("ethereum:1:sender", "child", 7)
            .unwrap(),
        8
    );
    store
        .persist_submission(
            "child",
            &SubmissionRow {
                kind: "synthetic".into(),
                reference: "admitted".into(),
                data: json!({}),
            },
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    assert!(store.release_orphaned_evm_nonce(&lease, "child").is_err());
    drop(lease);
}

#[test]
fn submitted_replacement_requires_correlated_terminal_no_remainder_proof() {
    let mut store = Store::in_memory().unwrap();
    for id in ["parent", "replacement", "new"] {
        request(&mut store, id);
    }
    let original = SubmissionRow {
        kind: "order".into(),
        reference: "original".into(),
        data: json!({}),
    };
    store
        .persist_submission("parent", &original, "2026-01-01T00:00:00Z")
        .unwrap();
    let parent = gofin_domain::Submission {
        kind: original.kind.clone(),
        reference: original.reference.clone(),
        data: original.data.clone(),
    };
    assert!(
        store
            .claim_replacement("parent", "replacement", &parent)
            .unwrap()
    );
    let submission = SubmissionRow {
        kind: "order".into(),
        reference: "child".into(),
        data: json!({}),
    };
    store
        .persist_submission("replacement", &submission, "2026-01-01T00:00:01Z")
        .unwrap();
    let mut proof:gofin_domain::VerifiedEffects=serde_json::from_value(json!({"evidence_version":1,"submission":{"kind":"order","reference":"child","data":{}},"effects":[],"evidence":{},"observed_at":"2026-01-01T00:00:02Z","proof_digest":""})).unwrap();
    proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
    let mut record = store.get_record("replacement").unwrap().unwrap();
    record["status"] = json!("failed");
    record["effects"] = serde_json::to_value(&proof).unwrap();
    store
        .update_record("replacement", &record, "2026-01-01T00:00:02Z")
        .unwrap();
    assert!(
        store
            .release_unsubmitted_replacement("replacement")
            .is_err()
    );
    assert!(
        store
            .release_replacement_with_verified_terminal("replacement")
            .is_err()
    );
    assert!(store.claim_replacement("parent", "new", &parent).is_err());
    proof.evidence["replacement_terminal_no_live_remainder"] = json!(true);
    proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
    record["effects"] = serde_json::to_value(proof).unwrap();
    store
        .update_record("replacement", &record, "2026-01-01T00:00:03Z")
        .unwrap();
    assert!(
        store
            .release_replacement_with_verified_terminal("replacement")
            .unwrap()
    );
    assert!(store.claim_replacement("parent", "new", &parent).unwrap());
}

#[test]
fn revised_preapproval_is_durable_and_bound_to_one_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("revision.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    let mut before = store.get_record("action").unwrap().unwrap();
    before["input"] = json!({"deadline": 1});
    store
        .update_record("action", &before, "2026-01-01T00:00:00Z")
        .unwrap();
    let mut failed = before.clone();
    failed["status"] = json!("expired");
    failed["last_reconciliation_evidence"] = json!({"engine_predispatch_failure":true});
    let issued = store
        .record_predispatch_failure_and_issue("action", &before, &failed, "2026-01-01T00:00:01Z")
        .unwrap();
    let lease = store.try_acquire_action_lease("action").unwrap();
    let revised = json!({"deadline": 2});
    let (claimed, created) = store
        .claim_revised_preapproval_continuation(&lease, &issued.token, &revised, "revised-hash")
        .unwrap();
    assert!(created);
    let record = store.get_record("action").unwrap().unwrap();
    assert_eq!(record["input"], revised);
    assert_eq!(record["fingerprint"], before["fingerprint"]);
    assert_eq!(record["request_id"], before["request_id"]);
    assert!(
        store
            .claim_revised_preapproval_continuation(
                &lease,
                &issued.token,
                &json!({"deadline":3}),
                "revised-hash"
            )
            .is_err()
    );
    assert!(
        store
            .claim_revised_preapproval_continuation(&lease, &issued.token, &revised, "other")
            .is_err()
    );
    drop(lease);
    drop(store);
    let mut store = Store::open(&path).unwrap();
    let lease = store.try_acquire_action_lease("action").unwrap();
    assert_eq!(
        store
            .claim_revised_preapproval_continuation(&lease, &issued.token, &revised, "revised-hash")
            .unwrap(),
        (claimed.clone(), false)
    );
    let successor = store.preapproval_successor(&issued.token).unwrap().unwrap();
    assert!(
        store
            .claim_revised_preapproval_continuation(
                &lease,
                &successor.token,
                &revised,
                "revised-hash"
            )
            .is_err()
    );
    let expected = store.get_record("action").unwrap().unwrap();
    let mut failed_again = expected.clone();
    failed_again["status"] = json!("failed");
    let result = json!({"state":"not_dispatched"});
    store
        .interrupt_preapproval_continuation(
            &lease,
            &issued.token,
            &expected,
            &failed_again,
            &result,
            "2026-01-01T00:00:02Z",
        )
        .unwrap();
    assert_eq!(
        store
            .claim_revised_preapproval_continuation(&lease, &issued.token, &revised, "revised-hash")
            .unwrap()
            .0
            .result,
        Some(result)
    );
    assert!(
        store
            .claim_revised_preapproval_continuation(
                &lease,
                &successor.token,
                &json!({"deadline":4}),
                "next-hash"
            )
            .unwrap()
            .1
    );
}

#[test]
fn revised_preapproval_rejects_prior_authority_or_submission() {
    for field in ["approval", "authorization_source", "submission", "run_id"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guard.sqlite");
        let mut store = Store::open(&path).unwrap();
        request(&mut store, "action");
        let before = store.get_record("action").unwrap().unwrap();
        let mut failed = before.clone();
        failed["status"] = json!("expired");
        failed["last_reconciliation_evidence"] = json!({"engine_predispatch_failure":true});
        let issued = store
            .record_predispatch_failure_and_issue(
                "action",
                &before,
                &failed,
                "2026-01-01T00:00:01Z",
            )
            .unwrap();
        failed[field] = json!({"evidence":"prior"});
        drop(store);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE operations SET record_json=?1 WHERE id='action'",
            [failed.to_string()],
        )
        .unwrap();
        drop(conn);
        let mut store = Store::open(&path).unwrap();
        let lease = store.try_acquire_action_lease("action").unwrap();
        assert!(
            store
                .claim_revised_preapproval_continuation(
                    &lease,
                    &issued.token,
                    &json!({"deadline":2}),
                    "hash"
                )
                .is_err(),
            "{field}"
        );
    }
}

#[test]
fn revision_schema_upgrade_recovers_interrupted_transaction_and_preserves_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("migration.sqlite");
    let mut store = Store::open(&path).unwrap();
    request(&mut store, "action");
    let before = store.get_record("action").unwrap().unwrap();
    let mut failed = before.clone();
    failed["status"] = json!("expired");
    failed["last_reconciliation_evidence"] = json!({"engine_predispatch_failure":true});
    let issued = store
        .record_predispatch_failure_and_issue("action", &before, &failed, "2026-01-01T00:00:01Z")
        .unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE preapproval_revisions; PRAGMA user_version=41; BEGIN; CREATE TABLE preapproval_revisions(token TEXT PRIMARY KEY, input_json TEXT NOT NULL, input_hash TEXT NOT NULL, prior_record_json TEXT NOT NULL); PRAGMA user_version=42; ROLLBACK;").unwrap();
    drop(conn);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.get_record("action").unwrap(), Some(failed));
    assert_eq!(
        store.latest_preapproval_continuation("action").unwrap(),
        Some(issued)
    );
    drop(store);
    Store::open(&path).unwrap();
}
