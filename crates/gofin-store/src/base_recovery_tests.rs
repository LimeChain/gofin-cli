//! Persistence-only Base recovery fixtures; no chain or approval assertions.
use super::*;
use gofin_domain::{
    BridgeStepKind as K, BridgeStepState as S, BridgeTransferRecord, BridgeTransferState as T,
};
const PORTAL: &str = "0x49048044d57e1c92a77f79988d21fa8faf74e97e";
fn parent() -> BridgeTransferRecord {
    let hash = format!("0x{}", "11".repeat(32));
    let quote = json!({"provider":"base_canonical","intent":{"source":{"chain_id":8453},"destination_chain_id":1,"source_address":"0x1111111111111111111111111111111111111111"}});
    let steps = vec![
        json!({"ordinal":0,"kind":"deposit","call":{"chain_id":8453,"to":"SYNTHETIC-source-bridge","value":"100","data":"0x1234"},"expected_sender":quote["intent"]["source_address"],"maximum_debit":[{"SYNTHETIC":"original principal"}],"dependencies":[]}),
        json!({"ordinal":1,"kind":"prove","call":{"chain_id":1,"to":PORTAL,"value":"0","data":"0x"},"expected_sender":quote["intent"]["source_address"],"maximum_debit":[],"dependencies":[0]}),
        json!({"ordinal":2,"kind":"finalize","call":{"chain_id":1,"to":PORTAL,"value":"0","data":"0x"},"expected_sender":quote["intent"]["source_address"],"maximum_debit":[],"dependencies":[1]}),
    ];
    serde_json::from_value(json!({"version":1,"id":"SYNTHETIC-transfer","grant_id":null,"metadata":{"SYNTHETIC":"original intent"},"settlement_expectation":null,"approved_bounds":{"SYNTHETIC":"original finite caps"},"prospective_debit":null,"state":"destination_pending",
        "quote_revisions":[{"revision":1,"provider":"base_canonical","route_id":"SYNTHETIC-route","quote_hash":"SYNTHETIC-original-hash","payload":{"quote":quote,"bounds":{"SYNTHETIC":"original caps"},"plan":{"quote":quote,"bounds":{"SYNTHETIC":"original caps"},"steps":steps,"fixed_scope_digest":"SYNTHETIC-original-scope"},"step_ids":{"0":"deposit","1":"prove","2":"finalize"}},"observed_at":"2026-01-01T00:00:00Z","valid_until":"2026-01-01T00:01:00Z"}],
        "steps":[{"transfer_id":"SYNTHETIC-transfer","grant_id":null,"step_id":"deposit","operation_id":"SYNTHETIC-source-op","kind":"deposit","depends_on":[],"state":"settled"},
            {"transfer_id":"SYNTHETIC-transfer","grant_id":null,"step_id":"prove","operation_id":"SYNTHETIC-prove-op","kind":"prove","depends_on":["deposit"],"state":"settled"},
            {"transfer_id":"SYNTHETIC-transfer","grant_id":null,"step_id":"finalize","operation_id":null,"kind":"finalize","depends_on":["prove"],"state":"unsubmitted"}],
        "allocation_owner":{"kind":"transfer","transfer_id":"SYNTHETIC-transfer"},"transit":{"transfer_id":"SYNTHETIC-transfer","amount":{"asset":{"network":"ethereum:8453","reference":"native","decimals":18},"atoms":"100"},"evidence_id":"SYNTHETIC-source-evidence","destination_locator":{"scheme":"base_withdrawal_message","source_evidence_id":"SYNTHETIC-source-evidence","destination_chain_id":1,"transaction_hash":"","message_hash":hash,"proof":{"SYNTHETIC":"original withdrawal"}},"proof_digest":"SYNTHETIC-source-proof","observed_at":"2026-01-01T00:00:00Z","spendable":false},"applied_proofs":["SYNTHETIC-source-proof"],"settlement_proofs":[]})).unwrap()
}
fn recovery(old: &BridgeTransferRecord) -> BridgeTransferRecord {
    let mut next = old.clone();
    next.version += 1;
    next.state = T::DestinationPending;
    if next.steps[2].state == S::Unsubmitted {
        next.steps[2].state = S::Superseded;
    }
    let mut revision = old.quote_revisions[0].clone();
    revision.revision += 1;
    for (ordinal, kind, id, dep) in [
        (3, K::Prove, "prove-recovery", "deposit"),
        (4, K::Finalize, "finalize-recovery", "prove-recovery"),
    ] {
        next.steps.push(gofin_domain::BridgeStepLink {
            transfer_id: old.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: id.into(),
            operation_id: None,
            kind: kind.clone(),
            depends_on: vec![dep.into()],
            state: S::Unsubmitted,
        });
        let call = json!({"ordinal":ordinal,"kind":kind,"call":{"chain_id":1,"to":PORTAL,"value":"0","data":"0x"},"expected_sender":revision.payload["quote"]["intent"]["source_address"],"maximum_debit":[],"dependencies":if ordinal==3{json!([0])}else{json!([3])}});
        revision.payload["plan"]["steps"]
            .as_array_mut()
            .unwrap()
            .push(call);
        revision.payload["step_ids"][ordinal.to_string()] = json!(id);
    }
    revision.payload["base_recovery"] = json!({"withdrawal_hash":old.transit.as_ref().unwrap().destination_locator.as_ref().unwrap().message_hash,"reason":"proof_invalidated","parent_step_id":"prove","parent_operation_id":"SYNTHETIC-prove-op","step_ids":["prove-recovery","finalize-recovery"]});
    revision.quote_hash = gofin_domain::binding_hash(&revision.payload).unwrap();
    next.quote_revisions.push(revision);
    next
}
fn seeded() -> (Store, BridgeTransferRecord) {
    let mut store = Store::in_memory().unwrap();
    let old = parent();
    // A historical, already-settled parent is seeded directly. Broker lifecycle
    // tests establish its production creation and proof path independently.
    store
        .conn
        .execute(
            "INSERT INTO bridge_transfers(transfer_id,version,record_json) VALUES (?1,?2,?3)",
            params![old.id, old.version, serde_json::to_string(&old).unwrap()],
        )
        .unwrap();
    for id in ["SYNTHETIC-source-op", "SYNTHETIC-prove-op"] {
        store
            .create_request(
                id,
                id,
                id,
                "bridge.transfer",
                None,
                "completed",
                &json!({"status":"completed","SYNTHETIC":"proved receipt"}),
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
    }
    for link in &old.steps {
        store.conn.execute("INSERT INTO bridge_step_links(transfer_id,step_id,operation_id,step_json) VALUES (?1,?2,?3,?4)",params![old.id,link.step_id,link.operation_id,serde_json::to_string(link).unwrap()]).unwrap();
    }
    (store, old)
}
#[test]
fn recovery_append_preserves_source_history_and_is_cas_safe_without_renewing_authority() {
    let (mut store, old) = seeded();
    let next = recovery(&old);
    let saved = store.append_base_recovery(&old, &next).unwrap();
    assert_eq!(saved, next);
    assert_eq!(saved.transit, old.transit);
    assert_eq!(saved.steps[1], old.steps[1]);
    assert_eq!(saved.steps[2].state, S::Superseded);
    assert_eq!(saved.approved_bounds, old.approved_bounds);
    assert_eq!(saved.quote_revisions[0], old.quote_revisions[0]);
    assert!(matches!(
        store.append_base_recovery(&old, &next),
        Err(StoreError::ConcurrentReservation)
    ));
    assert_eq!(store.bridge_transfer(&old.id).unwrap().unwrap(), next);
    assert_eq!(store.list_records().unwrap().len(), 2);
}
#[test]
fn recovery_append_rejects_principal_calls_new_wallet_wrong_chain_and_changed_parent() {
    for case in [
        "principal",
        "calldata",
        "network",
        "sender",
        "source",
        "authority",
        "transit",
        "dependencies",
    ] {
        let (mut store, old) = seeded();
        let mut next = recovery(&old);
        match case {
            "principal" => {
                next.quote_revisions[1].payload["plan"]["steps"][3]["call"]["value"] = json!("1")
            }
            "calldata" => {
                next.quote_revisions[1].payload["plan"]["steps"][3]["call"]["data"] =
                    json!("0x1234")
            }
            "network" => {
                next.quote_revisions[1].payload["plan"]["steps"][3]["call"]["chain_id"] =
                    json!(8453)
            }
            "sender" => {
                next.quote_revisions[1].payload["plan"]["steps"][3]["expected_sender"] =
                    json!("0x2222222222222222222222222222222222222222")
            }
            "source" => {
                next.quote_revisions[1].payload["quote"]["intent"]["source"]["chain_id"] = json!(1)
            }
            "authority" => {
                next.plan_scope = Some(gofin_domain::BridgePlanScope {
                    plan_id: "SYNTHETIC-new-owner".into(),
                    revision_id: "SYNTHETIC".into(),
                })
            }
            "transit" => {
                next.transit.as_mut().unwrap().amount.atoms = gofin_domain::Quantity::from_u64(101)
            }
            "dependencies" => next.steps[3].depends_on = vec!["prove".into()],
            _ => unreachable!(),
        }
        next.quote_revisions[1].quote_hash =
            gofin_domain::binding_hash(&next.quote_revisions[1].payload).unwrap();
        assert!(store.append_base_recovery(&old, &next).is_err(), "{case}");
        assert_eq!(store.bridge_transfer(&old.id).unwrap().unwrap(), old);
    }
}
#[test]
fn pending_proof_and_parent_plan_adoption_cannot_be_recovered() {
    let (mut store, mut old) = seeded();
    old.steps[1].state = S::Submitted;
    assert!(store.append_base_recovery(&old, &recovery(&old)).is_err());
    let (mut store, mut old) = seeded();
    old.plan_scope = Some(gofin_domain::BridgePlanScope {
        plan_id: "SYNTHETIC-existing-plan".into(),
        revision_id: "SYNTHETIC-existing-revision".into(),
    });
    store
        .conn
        .execute(
            "UPDATE bridge_transfers SET record_json=?2 WHERE transfer_id=?1",
            params![old.id, serde_json::to_string(&old).unwrap()],
        )
        .unwrap();
    let next = recovery(&old);
    assert_eq!(
        store.append_base_recovery(&old, &next).unwrap().plan_scope,
        old.plan_scope
    );
}

fn claim_final(store: &mut Store, old: &BridgeTransferRecord, id: &str) -> Value {
    let mut link = old.steps[2].clone();
    link.operation_id = Some(id.into());
    link.state = S::Submitted;
    let record = json!({"id":id,"operation":"bridge.transfer","status":"awaiting_approval","input":{"transferId":old.id,"stepId":"finalize","stepKind":"finalize"},"submission":null,"effects":null});
    store
        .create_bridge_attempt(
            id,
            id,
            id,
            "bridge.transfer",
            None,
            "awaiting_approval",
            &record,
            "2026-01-01T00:00:00Z",
            &link,
        )
        .unwrap();
    record
}

#[test]
fn reverted_finalization_keeps_transit_allocation_and_gets_a_new_final_identity() {
    let (mut store, old) = seeded();
    let id = "SYNTHETIC-final-failed";
    let mut record = claim_final(&mut store, &old, id);
    store.conn.execute("INSERT INTO bridge_allocation_owners(operation_id,transfer_id,charge_usd_micros,source_proved_at,transfer_settled_at) VALUES (?1,?2,'100','2026-01-01T00:00:00Z',NULL)",params!["SYNTHETIC-source-op",old.id]).unwrap();
    let submission = SubmissionRow {
        kind: "ethereum".into(),
        reference: format!("0x{}", "22".repeat(32)),
        data: json!({"SYNTHETIC":"original finalization"}),
    };
    store
        .persist_submission(id, &submission, "2026-01-01T00:00:01Z")
        .unwrap();
    record["submission"] =
        json!({"kind":submission.kind,"reference":submission.reference,"data":submission.data});
    record["status"] = json!("failed");
    record["effects"] = json!({"SYNTHETIC":"canonical revert cost"});
    let settlement = Settlement {
        operation_id: id.into(),
        terminal_record: record,
        proof: json!({"SYNTHETIC":"exact failed receipt"}),
        effects: json!([]),
        run_id: None,
        run_record: None,
    };
    store
        .commit_bridge_failed_stage(
            &settlement,
            &old.id,
            "finalize",
            &K::Finalize,
            "2026-01-01T00:00:02Z",
        )
        .unwrap();
    let failed = store.bridge_transfer(&old.id).unwrap().unwrap();
    assert_eq!(failed.state, T::NeedsAttention);
    assert_eq!(failed.transit, old.transit);
    assert_eq!(failed.steps[2].operation_id.as_deref(), Some(id));
    assert_eq!(failed.steps[2].state, S::NeedsAttention);
    let owners = store.bridge_allocation_owners().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].2, "100");
    assert!(owners[0].4.is_none());
    let mut next = recovery(&failed);
    next.steps.remove(3);
    let revision = next.quote_revisions.last_mut().unwrap();
    revision.payload["plan"]["steps"]
        .as_array_mut()
        .unwrap()
        .remove(3);
    revision.payload["plan"]["steps"][3]["ordinal"] = json!(3);
    revision.payload["plan"]["steps"][3]["dependencies"] = json!([0]);
    revision.payload["step_ids"]
        .as_object_mut()
        .unwrap()
        .remove("4");
    revision.payload["step_ids"]["3"] = json!("finalize-recovery");
    next.steps[3].depends_on = vec!["deposit".into()];
    revision.payload["base_recovery"] = json!({"reason":"stage_reverted","withdrawal_hash":failed.transit.as_ref().unwrap().destination_locator.as_ref().unwrap().message_hash,"parent_step_id":"finalize","parent_operation_id":id,"step_ids":["finalize-recovery"]});
    revision.quote_hash = gofin_domain::binding_hash(&revision.payload).unwrap();
    let recovered = store.append_base_recovery(&failed, &next).unwrap();
    assert_eq!(recovered.steps[2], failed.steps[2]);
    assert_eq!(recovered.transit, failed.transit);
    assert_eq!(recovered.steps[3].kind, K::Finalize);
    assert_eq!(store.list_records().unwrap().len(), 3);
}

#[test]
fn denied_deferred_stage_releases_only_its_unsubmitted_claim_and_preserves_destination_state() {
    let (mut store, old) = seeded();
    let id = "SYNTHETIC-final-denied";
    let mut record = claim_final(&mut store, &old, id);
    record["status"] = json!("rejected");
    store
        .commit_settlement(
            &Settlement {
                operation_id: id.into(),
                terminal_record: record,
                proof: json!({"SYNTHETIC":"denied approval"}),
                effects: json!([]),
                run_id: None,
                run_record: None,
            },
            "2026-01-01T00:00:01Z",
        )
        .unwrap();
    let released = store.bridge_transfer(&old.id).unwrap().unwrap();
    assert_eq!(released.state, T::DestinationPending);
    assert_eq!(released.transit, old.transit);
    assert_eq!(released.steps[2].state, S::Unsubmitted);
    assert!(released.steps[2].operation_id.is_none());
}

#[test]
fn ordinary_exact_attempt_cannot_adopt_a_plan_owned_recovery_parent() {
    let (mut store, mut old) = seeded();
    old.plan_scope = Some(gofin_domain::BridgePlanScope {
        plan_id: "SYNTHETIC-plan".into(),
        revision_id: "SYNTHETIC-revision".into(),
    });
    store
        .conn
        .execute(
            "UPDATE bridge_transfers SET record_json=?2 WHERE transfer_id=?1",
            params![old.id, serde_json::to_string(&old).unwrap()],
        )
        .unwrap();
    let mut link = old.steps[2].clone();
    link.operation_id = Some("SYNTHETIC-adoption".into());
    link.state = S::Submitted;
    assert!(
        store
            .create_bridge_attempt(
                "SYNTHETIC-adoption",
                "SYNTHETIC-adoption",
                "SYNTHETIC-adoption",
                "bridge.transfer",
                None,
                "awaiting_approval",
                &json!({"input":{"transferId":old.id,"stepId":"finalize","stepKind":"finalize"}}),
                "2026-01-01T00:00:00Z",
                &link
            )
            .is_err()
    );
    assert_eq!(store.list_records().unwrap().len(), 2);
    assert_eq!(store.bridge_transfer(&old.id).unwrap().unwrap(), old);
}

#[test]
fn immutable_original_permission_read_preserves_old_inventory_and_requires_exact_authority() {
    let (store, _) = seeded();
    let permission = gofin_domain::financial_plans::PlanPermission {
        recovery_parent: None,
        spec: gofin_domain::boundaries::BoundarySpec {
            fixed: json!({"steps":["SYNTHETIC-original-source","SYNTHETIC-original-proof","SYNTHETIC-original-final"],"scope":"SYNTHETIC-fixed"}),
            parameters: json!({"SYNTHETIC":"original caps"}),
            maximum_debits: vec![],
            maximum_costs: vec![],
            summary: "SYNTHETIC original permission".into(),
        },
        possible_credits: vec![],
        fee_assets: vec![],
        deferred_receipt: None,
        authorization_seconds: None,
        cumulative_scope: None,
    };
    let revision = json!({"plan_id":"SYNTHETIC-plan","revision_id":"SYNTHETIC-original-revision","spec":{"version":1,"name":"SYNTHETIC","steps":[],"aggregate_bounds":{"maximum_debits":[],"maximum_costs":[]}},"permissions":[{"step_id":"original","permission":permission}],"approval_binding_hash":"SYNTHETIC","approved_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:01:00Z","valid_for_seconds":60,"status":"superseded","replaces_revision_id":null});
    store.conn.execute("INSERT INTO financial_plans(plan_id,active_revision_id,record_json,created_at,updated_at) VALUES ('SYNTHETIC-plan','SYNTHETIC-new-revision','{}','t','t')",[]).unwrap();
    store.conn.execute("INSERT INTO financial_plan_revisions(plan_id,revision_id,request_id,revision_json,created_at) VALUES ('SYNTHETIC-plan','SYNTHETIC-original-revision','SYNTHETIC-request',?1,'t')",[revision.to_string()]).unwrap();
    let shared = Arc::new(Mutex::new(store));
    let state = BridgeStateRepository::new(shared.clone());
    let mut authority = gofin_domain::BridgePlanAuthority {
        plan_id: "SYNTHETIC-plan".into(),
        revision_id: "SYNTHETIC-original-revision".into(),
        plan_step_id: "original".into(),
    };
    assert_eq!(state.plan_permission(&authority).unwrap(), Some(permission));
    authority.plan_step_id = "undeclared".into();
    assert!(state.plan_permission(&authority).unwrap().is_none());
    authority.plan_step_id = "original".into();
    authority.revision_id = "wrong-revision".into();
    assert!(state.plan_permission(&authority).unwrap().is_none());
    authority.revision_id = "SYNTHETIC-original-revision".into();
    let mut corrupt = revision;
    corrupt["plan_id"] = json!("SYNTHETIC-other-plan");
    shared.lock().unwrap().conn.execute("UPDATE financial_plan_revisions SET revision_json=?1 WHERE request_id='SYNTHETIC-request'",[corrupt.to_string()]).unwrap();
    assert!(state.plan_permission(&authority).is_err());
}
