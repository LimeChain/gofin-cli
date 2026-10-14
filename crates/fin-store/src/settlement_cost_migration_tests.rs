use super::*;

fn seed_operation(store: &mut Store) -> Value {
    let record = json!({"id":"SYNTHETIC-operation","request_id":"SYNTHETIC-request","fingerprint":"SYNTHETIC-fingerprint","operation":"synthetic.read","status":"prepared"});
    store
        .create_request(
            "SYNTHETIC-operation",
            "SYNTHETIC-request",
            "SYNTHETIC-fingerprint",
            "synthetic.read",
            None,
            "prepared",
            &record,
            "synthetic-time",
        )
        .unwrap();
    record
}

#[test]
fn v18_upgrade_preserves_journal_and_recovers_after_interrupted_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fin.sqlite");
    let expected = {
        let mut store = Store::open(&path).unwrap();
        let expected = seed_operation(&mut store);
        store
            .conn
            .execute_batch(
                "DROP TABLE financial_plan_bridge_settlement_costs; PRAGMA user_version=18;",
            )
            .unwrap();
        // Model a process interruption during the v18->v19 DDL transaction.
        let tx = store.conn.unchecked_transaction().unwrap();
        tx.execute_batch("CREATE TABLE financial_plan_bridge_settlement_costs(plan_id TEXT NOT NULL, transfer_id TEXT NOT NULL, proof_digest TEXT NOT NULL, step_id TEXT NOT NULL, costs_json TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(plan_id,transfer_id,proof_digest));").unwrap();
        drop(tx);
        expected
    };

    let reopened = Store::open(&path).unwrap();
    let version: i64 = reopened
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 19);
    assert_eq!(
        reopened.get_record("SYNTHETIC-operation").unwrap().unwrap(),
        expected
    );
    let exists: bool = reopened.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='financial_plan_bridge_settlement_costs')",
        [],
        |row| row.get(0),
    ).unwrap();
    assert!(exists);
}

#[test]
fn bridge_settlement_cost_marker_is_idempotent_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fin.sqlite");
    let source = fin_domain::ResourceId("ethereum:1:wallet".into());
    let asset = fin_domain::AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let cost = fin_domain::CostBound {
        location: fin_domain::AccountingLocation::Cash { resource: source },
        amount: fin_domain::Amount {
            asset,
            atoms: fin_domain::Quantity::new("7").unwrap(),
        },
        treatment: fin_domain::CostTreatment::IncludedInDebit,
    };
    let plan_id = "SYNTHETIC-plan";
    let transfer_id = "SYNTHETIC-transfer";
    let proof = fin_domain::BridgeSettlementProof {
        transfer_id: transfer_id.into(),
        outcome: fin_domain::BridgeSettlementOutcome::Destination,
        evidence_id: "SYNTHETIC-proof-evidence".into(),
        effects: vec![],
        evidence: json!({}),
        observed_at: "synthetic-time".into(),
        proof_digest: "SYNTHETIC-proof-digest".into(),
    };
    let transfer = fin_domain::BridgeTransferRecord {
        version: 1,
        id: transfer_id.into(),
        grant_id: None,
        plan_scope: Some(fin_domain::BridgePlanScope {
            plan_id: plan_id.into(),
            revision_id: "SYNTHETIC-revision".into(),
        }),
        metadata: json!({}),
        settlement_expectation: None,
        approved_bounds: None,
        prospective_debit: None,
        state: fin_domain::BridgeTransferState::Completed,
        quote_revisions: vec![],
        steps: vec![fin_domain::BridgeStepLink {
            transfer_id: transfer_id.into(),
            grant_id: None,
            plan_authority: Some(fin_domain::BridgePlanAuthority {
                plan_id: plan_id.into(),
                revision_id: "SYNTHETIC-revision".into(),
                plan_step_id: "bridge".into(),
            }),
            step_id: "deposit".into(),
            operation_id: Some("SYNTHETIC-operation".into()),
            kind: fin_domain::BridgeStepKind::Deposit,
            depends_on: vec![],
            state: fin_domain::BridgeStepState::Settled,
        }],
        allocation_owner: fin_domain::BridgeAllocationOwner::Transfer {
            transfer_id: transfer_id.into(),
        },
        transit: None,
        applied_proofs: vec![proof.proof_digest.clone()],
        settlement_proofs: vec![proof.clone()],
    };
    let expected = json!({"revision":{"plan_id":plan_id,"revision_id":"SYNTHETIC-revision"},"usage":{"consumed_costs":[]}});
    let updated = json!({"revision":{"plan_id":plan_id,"revision_id":"SYNTHETIC-revision"},"usage":{"consumed_costs":[cost]}});
    {
        let mut store = Store::open(&path).unwrap();
        store.conn.execute(
            "INSERT INTO financial_plans(plan_id,active_revision_id,record_json,created_at,updated_at) VALUES (?1,?2,?3,?4,?4)",
            params![plan_id,"SYNTHETIC-revision",expected.to_string(),"synthetic-time"],
        ).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO bridge_transfers(transfer_id,version,record_json) VALUES (?1,?2,?3)",
                params![
                    transfer_id,
                    transfer.version,
                    serde_json::to_string(&transfer).unwrap()
                ],
            )
            .unwrap();
        assert!(
            store
                .commit_financial_plan_bridge_settlement_costs(
                    plan_id,
                    transfer_id,
                    &proof.proof_digest,
                    "bridge",
                    &expected,
                    &updated,
                    std::slice::from_ref(&cost),
                    "synthetic-time",
                )
                .unwrap()
        );
        assert!(
            !store
                .commit_financial_plan_bridge_settlement_costs(
                    plan_id,
                    transfer_id,
                    &proof.proof_digest,
                    "bridge",
                    &expected,
                    &updated,
                    std::slice::from_ref(&cost),
                    "synthetic-retry",
                )
                .unwrap()
        );
    }
    let reopened = Store::open(&path).unwrap();
    let plan = reopened.financial_plan(plan_id).unwrap().unwrap();
    assert_eq!(plan["usage"]["consumed_costs"].as_array().unwrap().len(), 1);
    let marker = reopened
        .financial_plan_bridge_settlement_costs(plan_id, transfer_id, &proof.proof_digest)
        .unwrap()
        .unwrap();
    assert_eq!(marker.0, "bridge");
    assert_eq!(
        serde_json::from_value::<Vec<fin_domain::CostBound>>(marker.1).unwrap(),
        vec![cost]
    );
}
