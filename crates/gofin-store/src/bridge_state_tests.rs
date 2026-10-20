use super::*;
use gofin_domain::{
    Amount, AssetId, BridgeAllocationOwner, BridgeProspectiveDebit, BridgeQuoteRevision,
    BridgeSettlementExpectation, BridgeSettlementOutcome, BridgeSettlementProof,
    BridgeSettlementTarget, BridgeStepKind, BridgeStepLink, BridgeStepState, BridgeTransferRecord,
    BridgeTransferState, Quantity, ResourceId, TransitResource,
};

fn transfer() -> BridgeTransferRecord {
    BridgeTransferRecord {
        version: 1,
        id: "SYNTHETIC-transfer-1".into(),
        grant_id: Some("SYNTHETIC-grant-1".into()),
        plan_scope: None,
        metadata: json!({"intent":"SYNTHETIC exact route and bounds"}),
        settlement_expectation: Some(BridgeSettlementExpectation {
            destination: vec![BridgeSettlementTarget {
                resource: ResourceId("SYNTHETIC-destination".into()),
                asset: AssetId {
                    network: "SYNTHETIC-destination-chain".into(),
                    reference: "SYNTHETIC-token".into(),
                    decimals: 6,
                },
                minimum_atoms: Quantity::new("1").unwrap(),
            }],
            refund: vec![],
        }),
        approved_bounds: None,
        prospective_debit: None,
        state: BridgeTransferState::Prepared,
        quote_revisions: vec![],
        steps: vec![],
        allocation_owner: BridgeAllocationOwner::Operation { operation_id: None },
        transit: None,
        applied_proofs: vec![],
        settlement_proofs: vec![],
    }
}

fn bridge_operation(id: &str, transfer_id: &str, step_id: &str) -> Value {
    json!({
        "id":id,
        "request_id":format!("SYNTHETIC-request-{id}"),
        "fingerprint":format!("SYNTHETIC-fingerprint-{id}"),
        "operation":"bridge.transfer",
        "status":"awaiting_approval",
        "submission":null,
        "input":{"transferId":transfer_id,"stepId":step_id}
    })
}

fn quote(revision: u32, order_id: &str) -> BridgeQuoteRevision {
    BridgeQuoteRevision {
        revision,
        provider: "SYNTHETIC-provider".into(),
        route_id: "SYNTHETIC-route".into(),
        order_id: Some(order_id.into()),
        quote_hash: format!("SYNTHETIC-hash-{revision}"),
        payload: json!({"quote":{"order":order_id},"bounds":{"max_debit":"100"}}),
        observed_at: format!("2026-01-01T00:00:0{revision}Z"),
        valid_until: "2026-01-01T00:10:00Z".into(),
    }
}

#[test]
fn bridge_quote_revision_order_id_remains_backward_compatible_and_optional() {
    let old = serde_json::json!({
        "revision":1,
        "provider":"SYNTHETIC-provider",
        "route_id":"SYNTHETIC-route",
        "order_id":"SYNTHETIC-order-id",
        "quote_hash":"SYNTHETIC-hash",
        "payload":{},
        "observed_at":"2026-01-01T00:00:00Z",
        "valid_until":"2026-01-01T00:10:00Z"
    });
    let decoded: BridgeQuoteRevision = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(decoded.order_id.as_deref(), Some("SYNTHETIC-order-id"));
    assert_eq!(serde_json::to_value(decoded).unwrap(), old);

    let mut no_order = old;
    no_order.as_object_mut().unwrap().remove("order_id");
    let decoded: BridgeQuoteRevision = serde_json::from_value(no_order.clone()).unwrap();
    assert_eq!(decoded.order_id, None);
    assert_eq!(serde_json::to_value(decoded).unwrap(), no_order);
}

#[test]
fn legacy_transit_json_without_destination_locator_still_round_trips() {
    let old = json!({
        "transfer_id":"SYNTHETIC-legacy-transfer",
        "amount":{
            "asset":{"network":"SYNTHETIC-chain","reference":"native","decimals":18},
            "atoms":"1000"
        },
        "evidence_id":"SYNTHETIC:source-event",
        "proof_digest":"SYNTHETIC-proof",
        "observed_at":"2026-01-01T00:00:00Z",
        "spendable":false
    });
    let decoded: TransitResource = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(decoded.destination_locator, None);
    assert_eq!(serde_json::to_value(decoded).unwrap(), old);
}

#[test]
fn legacy_v19_lineage_upgrades_to_current_schema_and_preserves_pending_relay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-v19-relay.sqlite");
    let mut store = Store::open(&path).unwrap();
    let mut pending = transfer();
    pending.id = "SYNTHETIC-pending-evm-relay".into();
    pending.metadata = json!({
        "provider":"relay",
        "intent":{
            "wallet":"ethereum:1:0x1111111111111111111111111111111111111111",
            "source_address":"0x1111111111111111111111111111111111111111",
            "source":{"chain_id":1,"token":null,"decimals":18},
            "destination_chain_id":8453,
            "destination_assets":[{"chain_id":8453,"token":null,"decimals":18}],
            "recipient":"0x1111111111111111111111111111111111111111",
            "refund_to":"0x1111111111111111111111111111111111111111",
            "allowed_refund_assets":[{"chain_id":1,"token":null,"decimals":18}],
            "amount":"1100"
        }
    });
    pending.settlement_expectation = Some(BridgeSettlementExpectation {
        destination: vec![BridgeSettlementTarget {
            resource: ResourceId("ethereum:8453:0x1111111111111111111111111111111111111111".into()),
            asset: AssetId {
                network: "ethereum:8453".into(),
                reference: "native".into(),
                decimals: 18,
            },
            minimum_atoms: Quantity::new("1000").unwrap(),
        }],
        refund: vec![],
    });
    store.save_bridge_transfer(&pending).unwrap();
    pending.version = 2;
    pending.state = BridgeTransferState::InTransit;
    pending.transit = Some(TransitResource {
        transfer_id: pending.id.clone(),
        amount: Amount {
            asset: AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            atoms: Quantity::new("1100").unwrap(),
        },
        evidence_id: "ethereum:1:SYNTHETIC-source-event".into(),
        destination_locator: None,
        proof_digest: "SYNTHETIC-proof-digest".into(),
        observed_at: "2026-01-01T00:00:00Z".into(),
        spendable: false,
    });
    let original_json = serde_json::to_string(&pending).unwrap();
    store
        .conn
        .execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1",
            params![pending.id, pending.version, original_json],
        )
        .unwrap();
    let operation = json!({"id":"SYNTHETIC-pending-relay-op","request_id":"SYNTHETIC-pending-relay-request","fingerprint":"SYNTHETIC-pending-relay-fingerprint","operation":"bridge.transfer","status":"submitted","submission":{"kind":"evm_transaction","reference":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}});
    store
        .create_request(
            "SYNTHETIC-pending-relay-op",
            "SYNTHETIC-pending-relay-request",
            "SYNTHETIC-pending-relay-fingerprint",
            "bridge.transfer",
            None,
            "submitted",
            &operation,
            "t0",
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES (?1,?2,?3,?4)",
            params![
                "SYNTHETIC-pending-relay-op",
                "evm_transaction",
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "{}"
            ],
        )
        .unwrap();
    // The Relay development lineage used v19 as a marker without the table
    // added by main's independent v19 migration.
    store
        .conn
        .execute_batch("DROP TABLE financial_plan_bridge_settlement_costs; DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=19;")
        .unwrap();
    drop(store);

    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .conn
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        reopened.bridge_transfer(&pending.id).unwrap(),
        Some(pending)
    );
    assert_eq!(reopened.conn.query_row::<String, _, _>("SELECT record_json FROM bridge_transfers WHERE transfer_id='SYNTHETIC-pending-evm-relay'", [], |row| row.get(0)).unwrap(), original_json);
    assert_eq!(
        reopened.get_record("SYNTHETIC-pending-relay-op").unwrap(),
        Some(operation)
    );
    assert_eq!(
        reopened
            .conn
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM submissions WHERE operation_id='SYNTHETIC-pending-relay-op'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn solana_bridge_asset_identities_survive_restart_exactly() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-solana-bridge.sqlite");
    let mut store = Store::open(&path).unwrap();
    let mut record = transfer();
    record.id = "SYNTHETIC-evm-to-solana-relay".into();
    record.metadata = json!({"provider":"relay","intent":{"source":{"chain_id":1,"token":null,"decimals":18},"solana_destination":{"mint":null,"network":"solana:mainnet-beta"}}});
    record.settlement_expectation = Some(BridgeSettlementExpectation {
        destination: vec![BridgeSettlementTarget {
            resource: ResourceId("solana:mainnet-beta:11111111111111111111111111111111".into()),
            asset: AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "native".into(),
                decimals: 9,
            },
            minimum_atoms: Quantity::new("1").unwrap(),
        }],
        refund: vec![BridgeSettlementTarget {
            resource: ResourceId("ethereum:1:0x1111111111111111111111111111111111111111".into()),
            asset: AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            },
            minimum_atoms: Quantity::new("1").unwrap(),
        }],
    });
    store.save_bridge_transfer(&record).unwrap();
    let mut reverse = transfer();
    reverse.id = "SYNTHETIC-solana-to-evm-relay".into();
    reverse.metadata = json!({"provider":"relay","intent":{"solana_source":{"mint":"So11111111111111111111111111111111111111112","network":"solana:mainnet-beta"},"destination_chain_id":8453}});
    reverse.settlement_expectation = Some(BridgeSettlementExpectation {
        destination: vec![BridgeSettlementTarget {
            resource: ResourceId("ethereum:8453:0x1111111111111111111111111111111111111111".into()),
            asset: AssetId {
                network: "ethereum:8453".into(),
                reference: "native".into(),
                decimals: 18,
            },
            minimum_atoms: Quantity::new("1").unwrap(),
        }],
        refund: vec![BridgeSettlementTarget {
            resource: ResourceId("solana:mainnet-beta:11111111111111111111111111111111".into()),
            asset: AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "So11111111111111111111111111111111111111112".into(),
                decimals: 9,
            },
            minimum_atoms: Quantity::new("1").unwrap(),
        }],
    });
    store.save_bridge_transfer(&reverse).unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.bridge_transfer(&record.id).unwrap(), Some(record));
    assert_eq!(
        reopened.bridge_transfer(&reverse.id).unwrap(),
        Some(reverse)
    );
}

#[test]
fn v8_to_v9_marker_migration_preserves_bridge_and_uncertain_operation_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-v8-bridge.sqlite");
    let mut store = Store::open(&path).unwrap();
    let mut pending = transfer();
    pending.grant_id = None;
    pending.steps = vec![BridgeStepLink {
        transfer_id: pending.id.clone(),
        step_id: "SYNTHETIC-plan-deposit-link".into(),
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        operation_id: None,
        grant_id: None,
        plan_authority: None,
        state: BridgeStepState::Unsubmitted,
    }];
    store.save_bridge_transfer(&pending).unwrap();
    pending.plan_scope = Some(gofin_domain::BridgePlanScope {
        plan_id: "SYNTHETIC-plan-owner".into(),
        revision_id: "SYNTHETIC-plan-revision".into(),
    });
    store
        .conn
        .execute(
            "UPDATE bridge_transfers SET record_json=?2 WHERE transfer_id=?1",
            params![pending.id, serde_json::to_string(&pending).unwrap()],
        )
        .unwrap();
    let plan_revision = json!({
        "plan_id":"SYNTHETIC-plan-owner",
        "revision_id":"SYNTHETIC-plan-revision",
        "spec":{"steps":[{"step_id":"bridge-deposit","operation":"bridge.transfer"}]},
        "permissions":[{"step_id":"bridge-deposit","permission":{"spec":{"fixed":{}},"possible_credits":[],"fee_assets":[]}}]
    });
    let plan_record = json!({
        "revision":plan_revision,
        "approval":{"mechanism":"synthetic"},
        "attempts":[],
        "usage":{"reservations":[]},
        "active_attempt_id":null
    });
    store.conn.execute(
        "INSERT INTO financial_plans(plan_id,active_revision_id,record_json,created_at,updated_at) VALUES (?1,?2,?3,?4,?4)",
        params!["SYNTHETIC-plan-owner", "SYNTHETIC-plan-revision", plan_record.to_string(), "t0"],
    ).unwrap();
    store.conn.execute(
        "INSERT INTO financial_plan_revisions(plan_id,revision_id,request_id,revision_json,created_at) VALUES (?1,?2,?3,?4,?5)",
        params!["SYNTHETIC-plan-owner", "SYNTHETIC-plan-revision", "SYNTHETIC-plan-request", plan_revision.to_string(), "t0"],
    ).unwrap();
    let operation = json!({
        "id":"SYNTHETIC-pending-op",
        "request_id":"SYNTHETIC-pending-request",
        "fingerprint":"SYNTHETIC-pending-fingerprint",
        "operation":"synthetic.write",
        "status":"submitted",
        "submission":{"kind":"SYNTHETIC","reference":"SYNTHETIC-external"}
    });
    store
        .create_request(
            "SYNTHETIC-pending-op",
            "SYNTHETIC-pending-request",
            "SYNTHETIC-pending-fingerprint",
            "synthetic.write",
            None,
            "submitted",
            &operation,
            "t0",
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES (?1,?2,?3,?4)",
            params![
                "SYNTHETIC-pending-op",
                "SYNTHETIC",
                "SYNTHETIC-external",
                "{}"
            ],
        )
        .unwrap();
    store.conn.execute_batch("DROP TABLE IF EXISTS settlement_progress; DROP TABLE IF EXISTS signer_nonces; DROP TABLE operation_proof_events; DROP TABLE IF EXISTS unresolved_cost_events; DROP TABLE IF EXISTS unresolved_cost_consumption; DROP TABLE IF EXISTS grouped_amendment_scopes; DROP TABLE IF EXISTS grouped_amendment_children; DROP TABLE IF EXISTS grouped_amendment_revisions; DROP TABLE IF EXISTS grouped_amendments; DROP TABLE IF EXISTS compound_amendment_children; DROP TABLE IF EXISTS operation_amendments; DROP TABLE IF EXISTS operation_proof_events; DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=8;").unwrap();

    drop(store);

    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .conn
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        reopened.bridge_transfer(&pending.id).unwrap(),
        Some(pending.clone())
    );
    assert_eq!(
        reopened.financial_plan("SYNTHETIC-plan-owner").unwrap(),
        Some(plan_record.clone())
    );
    assert_eq!(
        reopened
            .financial_plan_revision("SYNTHETIC-plan-owner", "SYNTHETIC-plan-revision")
            .unwrap(),
        Some(plan_revision)
    );
    assert_eq!(
        reopened
            .bridge_transfer(&pending.id)
            .unwrap()
            .unwrap()
            .steps[0]
            .state,
        BridgeStepState::Unsubmitted
    );
    assert_eq!(
        reopened.get_record("SYNTHETIC-pending-op").unwrap(),
        Some(operation)
    );
    assert_eq!(
        reopened
            .conn
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM submissions WHERE operation_id='SYNTHETIC-pending-op'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        1
    );
    drop(reopened);

    let future = Connection::open(&path).unwrap();
    future
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();

    drop(future);
    assert!(
        Store::open(&path).is_err(),
        "older store code must reject a newer authorization schema"
    );
    let readback = Connection::open(&path).unwrap();
    assert_eq!(
        readback
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        SCHEMA_VERSION + 1
    );
    assert_eq!(
        readback
            .query_row::<String, _, _>(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id='SYNTHETIC-transfer-1'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        serde_json::to_string(&pending).unwrap()
    );
    assert_eq!(
        readback
            .query_row::<String, _, _>(
                "SELECT record_json FROM financial_plans WHERE plan_id='SYNTHETIC-plan-owner'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        plan_record.to_string()
    );
}

#[test]
fn source_derived_destination_locator_persists_and_cannot_be_rebound() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-locator.sqlite");
    let source_evidence_id =
        "ethereum:1:0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:7";
    let locator = gofin_domain::BridgeDestinationLocator {
        message_hash: None,
        scheme: "SYNTHETIC-message-v1".into(),
        source_evidence_id: source_evidence_id.into(),
        destination_chain_id: Some(42161),
        destination_network: None,
        transaction_hash: format!("0x{}", "b".repeat(64)),
        proof: json!({"message_index":"42"}),
    };
    let transit = TransitResource {
        transfer_id: "SYNTHETIC-locator-transfer".into(),
        amount: Amount {
            asset: AssetId {
                network: "SYNTHETIC-source-chain".into(),
                reference: "native".into(),
                decimals: 18,
            },
            atoms: Quantity::new("1000000").unwrap(),
        },
        evidence_id: source_evidence_id.into(),
        destination_locator: Some(locator.clone()),
        proof_digest: "SYNTHETIC-source-proof".into(),
        observed_at: "2026-01-01T00:00:00Z".into(),
        spendable: false,
    };
    let mut record = transfer();
    record.id = transit.transfer_id.clone();
    record.state = BridgeTransferState::InTransit;
    record.allocation_owner = BridgeAllocationOwner::Transfer {
        transfer_id: record.id.clone(),
    };
    record.applied_proofs.push(transit.proof_digest.clone());
    record.transit = Some(transit.clone());

    let mut store = Store::open(&path).unwrap();
    store.save_bridge_transfer(&record).unwrap();
    assert_eq!(
        store.bridge_transfer(&record.id).unwrap(),
        Some(record.clone())
    );
    drop(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.bridge_transfer(&record.id).unwrap(),
        Some(record.clone())
    );
    let mut changed = record.clone();
    changed.version += 1;
    changed
        .transit
        .as_mut()
        .unwrap()
        .destination_locator
        .as_mut()
        .unwrap()
        .proof = json!({"message_index":"43"});
    assert!(matches!(
        store.save_bridge_transfer(&changed),
        Err(StoreError::BridgeConflict)
    ));

    let settlement = BridgeSettlementProof {
        transfer_id: transit.transfer_id.clone(),
        outcome: BridgeSettlementOutcome::Destination,
        evidence_id: format!("ethereum:42161:{}:transaction", locator.transaction_hash),
        effects: vec![],
        evidence: json!({
            "chain_id":42161,
            "transaction_hash":locator.transaction_hash,
            "correlation":source_evidence_id
        }),
        observed_at: "2026-01-01T00:01:00Z".into(),
        proof_digest: "SYNTHETIC-final-proof".into(),
    };
    assert!(settlement_locator_matches(Some(&transit), &settlement));
    let mut unrelated = settlement.clone();
    unrelated.evidence["transaction_hash"] = json!(format!("0x{}", "c".repeat(64)));
    assert!(!settlement_locator_matches(Some(&transit), &unrelated));

    let mut mismatched = transit.clone();
    mismatched.transfer_id = "SYNTHETIC-invalid-locator-transfer".into();
    mismatched
        .destination_locator
        .as_mut()
        .unwrap()
        .source_evidence_id =
        "ethereum:1:0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc:8".into();
    assert!(matches!(
        store.record_bridge_transit(&mismatched),
        Err(StoreError::BridgeConflict)
    ));
    let mut invalid_hash = transit.clone();
    invalid_hash.transfer_id = "SYNTHETIC-invalid-hash-transfer".into();
    invalid_hash
        .destination_locator
        .as_mut()
        .unwrap()
        .transaction_hash = "0x1234".into();
    assert!(matches!(
        store.record_bridge_transit(&invalid_hash),
        Err(StoreError::BridgeConflict)
    ));
}

fn create_operation(store: &mut Store, id: &str) {
    store
        .create_request(
            id,
            &format!("SYNTHETIC-request-{id}"),
            &format!("SYNTHETIC-fingerprint-{id}"),
            "bridge.transfer",
            None,
            "awaiting_approval",
            &json!({"id":id,"status":"awaiting_approval"}),
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
}

#[test]
fn bridge_quote_refresh_and_step_link_are_append_only_and_restart_safe() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-bridge.sqlite");
    let mut store = Store::open(&path).unwrap();
    let mut record = transfer();
    record.grant_id = None;
    store.save_bridge_transfer(&record).unwrap();
    assert_eq!(
        store.bridge_transfer(&record.id).unwrap(),
        Some(record.clone())
    );

    let first = quote(1, "SYNTHETIC-order-1");
    store
        .append_bridge_quote_revision(&record.id, &first)
        .unwrap();
    record.version = 2;
    record.quote_revisions.push(first.clone());
    assert_eq!(
        store.bridge_transfer(&record.id).unwrap(),
        Some(record.clone())
    );
    assert!(matches!(
        store.append_bridge_quote_revision(&record.id, &first),
        Err(StoreError::BridgeConflict)
    ));

    let second = quote(2, "SYNTHETIC-order-2");
    store
        .append_bridge_quote_revision(&record.id, &second)
        .unwrap();
    record.version = 3;
    record.quote_revisions.push(second.clone());
    let step = BridgeStepLink {
        transfer_id: record.id.clone(),
        grant_id: None,
        plan_authority: None,
        step_id: "SYNTHETIC-deposit-step".into(),
        operation_id: Some("SYNTHETIC-deposit-operation".into()),
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    create_operation(&mut store, step.operation_id.as_deref().unwrap());
    store.link_bridge_step(&step).unwrap();
    record.version = 4;
    record.steps.push(step.clone());
    assert_eq!(
        store.bridge_transfer(&record.id).unwrap(),
        Some(record.clone())
    );
    assert!(matches!(
        store.link_bridge_step(&BridgeStepLink {
            operation_id: Some("SYNTHETIC-replacement-operation".into()),
            ..step.clone()
        }),
        Err(StoreError::BridgeConflict)
    ));

    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.bridge_transfer(&record.id).unwrap(), Some(record));
    assert_eq!(reopened.bridge_transfers().unwrap().len(), 1);
}

#[test]
fn restart_releases_unsubmitted_bridge_claim_but_preserves_submitted_claim() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-bridge-recovery.sqlite");
    {
        let mut store = Store::open(&path).unwrap();

        let mut retry_transfer = transfer();
        retry_transfer.grant_id = None;
        retry_transfer.id = "SYNTHETIC-retry-transfer".into();
        store.save_bridge_transfer(&retry_transfer).unwrap();
        let retry_operation = bridge_operation(
            "SYNTHETIC-unsubmitted-operation",
            &retry_transfer.id,
            "SYNTHETIC-retry-step",
        );
        store
            .create_request(
                "SYNTHETIC-unsubmitted-operation",
                "SYNTHETIC-request-SYNTHETIC-unsubmitted-operation",
                "SYNTHETIC-fingerprint-SYNTHETIC-unsubmitted-operation",
                "bridge.transfer",
                None,
                "awaiting_approval",
                &retry_operation,
                "t0",
            )
            .unwrap();
        store
            .link_bridge_step(&BridgeStepLink {
                transfer_id: retry_transfer.id.clone(),
                step_id: "SYNTHETIC-retry-step".into(),
                kind: BridgeStepKind::Deposit,
                depends_on: vec![],
                operation_id: Some("SYNTHETIC-unsubmitted-operation".into()),
                grant_id: None,
                plan_authority: None,
                state: BridgeStepState::Unsubmitted,
            })
            .unwrap();
        store
            .link_bridge_step(&BridgeStepLink {
                transfer_id: retry_transfer.id.clone(),
                step_id: "SYNTHETIC-retry-step".into(),
                kind: BridgeStepKind::Deposit,
                depends_on: vec![],
                operation_id: Some("SYNTHETIC-unsubmitted-operation".into()),
                grant_id: None,
                plan_authority: None,
                state: BridgeStepState::Submitted,
            })
            .unwrap();

        let mut sent_transfer = transfer();
        sent_transfer.grant_id = None;
        sent_transfer.id = "SYNTHETIC-sent-transfer".into();
        store.save_bridge_transfer(&sent_transfer).unwrap();
        let sent_operation = bridge_operation(
            "SYNTHETIC-submitted-operation",
            &sent_transfer.id,
            "SYNTHETIC-sent-step",
        );
        store
            .create_request(
                "SYNTHETIC-submitted-operation",
                "SYNTHETIC-request-SYNTHETIC-submitted-operation",
                "SYNTHETIC-fingerprint-SYNTHETIC-submitted-operation",
                "bridge.transfer",
                None,
                "awaiting_approval",
                &sent_operation,
                "t0",
            )
            .unwrap();
        store
            .link_bridge_step(&BridgeStepLink {
                transfer_id: sent_transfer.id.clone(),
                step_id: "SYNTHETIC-sent-step".into(),
                kind: BridgeStepKind::Deposit,
                depends_on: vec![],
                operation_id: Some("SYNTHETIC-submitted-operation".into()),
                grant_id: None,
                plan_authority: None,
                state: BridgeStepState::Unsubmitted,
            })
            .unwrap();
        store
            .link_bridge_step(&BridgeStepLink {
                transfer_id: sent_transfer.id.clone(),
                step_id: "SYNTHETIC-sent-step".into(),
                kind: BridgeStepKind::Deposit,
                depends_on: vec![],
                operation_id: Some("SYNTHETIC-submitted-operation".into()),
                grant_id: None,
                plan_authority: None,
                state: BridgeStepState::Submitted,
            })
            .unwrap();
        store
            .persist_submission(
                "SYNTHETIC-submitted-operation",
                &SubmissionRow {
                    kind: "SYNTHETIC".into(),
                    reference: "SYNTHETIC-original-submission".into(),
                    data: json!({}),
                },
                "t1",
            )
            .unwrap();
    }

    let mut reopened = Store::open(&path).unwrap();
    let report = reopened.recover_interrupted().unwrap();
    assert_eq!(
        report.cancelled_before_submission,
        vec!["SYNTHETIC-unsubmitted-operation"]
    );
    assert_eq!(
        report.uncertain_submissions,
        vec!["SYNTHETIC-submitted-operation"]
    );
    assert_eq!(
        reopened
            .bridge_transfer("SYNTHETIC-retry-transfer")
            .unwrap()
            .unwrap()
            .steps[0]
            .state,
        BridgeStepState::Unsubmitted
    );
    assert_eq!(
        reopened
            .bridge_transfer("SYNTHETIC-retry-transfer")
            .unwrap()
            .unwrap()
            .steps[0]
            .operation_id,
        None
    );
    assert_eq!(
        reopened
            .bridge_transfer("SYNTHETIC-sent-transfer")
            .unwrap()
            .unwrap()
            .steps[0]
            .operation_id
            .as_deref(),
        Some("SYNTHETIC-submitted-operation")
    );

    let retry_operation = bridge_operation(
        "SYNTHETIC-retry-operation",
        "SYNTHETIC-retry-transfer",
        "SYNTHETIC-retry-step",
    );
    reopened
        .create_request(
            "SYNTHETIC-retry-operation",
            "SYNTHETIC-request-SYNTHETIC-retry-operation",
            "SYNTHETIC-fingerprint-SYNTHETIC-retry-operation",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &retry_operation,
            "t2",
        )
        .unwrap();
    reopened
        .link_bridge_step(&BridgeStepLink {
            transfer_id: "SYNTHETIC-retry-transfer".into(),
            step_id: "SYNTHETIC-retry-step".into(),
            kind: BridgeStepKind::Deposit,
            depends_on: vec![],
            operation_id: Some("SYNTHETIC-retry-operation".into()),
            grant_id: None,
            plan_authority: None,
            state: BridgeStepState::Submitted,
        })
        .unwrap();
    assert_eq!(
        reopened
            .bridge_transfer("SYNTHETIC-retry-transfer")
            .unwrap()
            .unwrap()
            .steps[0]
            .operation_id
            .as_deref(),
        Some("SYNTHETIC-retry-operation")
    );
}

#[test]
fn source_proof_handoff_counts_one_owner_past_24_hours_and_never_double_charges() {
    let mut store = Store::in_memory().unwrap();
    let record = transfer();
    store.save_bridge_transfer(&record).unwrap();
    create_operation(&mut store, "SYNTHETIC-source-op");
    store
        .handoff_bridge_allocation(
            "SYNTHETIC-source-op",
            &record.id,
            "10000000",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    store
        .handoff_bridge_allocation(
            "SYNTHETIC-source-op",
            &record.id,
            "10000000",
            "2026-01-02T00:00:00Z",
        )
        .unwrap();
    assert_eq!(store.bridge_allocation_owners().unwrap().len(), 1);
    assert!(matches!(
        store.handoff_bridge_allocation(
            "SYNTHETIC-source-op",
            &record.id,
            "10000001",
            "2026-01-02T00:00:00Z"
        ),
        Err(StoreError::BridgeConflict)
    ));
    store
        .settle_bridge_allocation(&record.id, "2026-01-03T00:00:00Z")
        .unwrap();
    // A restart preserves a settled clock; owner ages out only 24h after final proof.
    let owner = store.bridge_allocation_owners().unwrap().remove(0);
    assert_eq!(owner.2, "10000000");
    assert_eq!(owner.3, "2026-01-01T00:00:00Z");
    assert_eq!(owner.4.as_deref(), Some("2026-01-03T00:00:00Z"));
}

#[test]
fn transit_is_nonspendable_and_proof_digest_cannot_be_reused() {
    let mut store = Store::in_memory().unwrap();
    let record = transfer();
    store.save_bridge_transfer(&record).unwrap();
    let transit = TransitResource {
        transfer_id: record.id.clone(),
        amount: Amount {
            asset: AssetId {
                network: "SYNTHETIC-chain".into(),
                reference: "SYNTHETIC-token".into(),
                decimals: 6,
            },
            atoms: Quantity::new("1000000").unwrap(),
        },
        evidence_id: "SYNTHETIC:source-evidence-1".into(),
        destination_locator: None,
        proof_digest: "SYNTHETIC-source-proof".into(),
        observed_at: "2026-01-01T00:00:00Z".into(),
        spendable: false,
    };
    store.record_bridge_transit(&transit).unwrap();
    store.record_bridge_transit(&transit).unwrap();
    assert_eq!(
        store.bridge_transit(&record.id).unwrap(),
        Some(transit.clone())
    );
    let mut second_transfer = transfer();
    second_transfer.id = "SYNTHETIC-transfer-2".into();
    store.save_bridge_transfer(&second_transfer).unwrap();
    assert!(matches!(
        store.record_bridge_transit(&TransitResource {
            transfer_id: second_transfer.id,
            ..transit.clone()
        }),
        Err(StoreError::BridgeConflict)
    ));
    assert!(matches!(
        store.record_bridge_transit(&TransitResource {
            proof_digest: "SYNTHETIC-other-proof".into(),
            ..transit.clone()
        }),
        Err(StoreError::BridgeConflict)
    ));
    assert!(matches!(
        store.record_bridge_transit(&TransitResource {
            spendable: true,
            ..transit
        }),
        Err(StoreError::BridgeConflict)
    ));
}

#[test]
fn v3_upgrade_adds_bridge_tables_without_touching_pending_operation_or_submission() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("SYNTHETIC-v3.sqlite");
    let mut store = Store::open(&path).unwrap();
    let operation = json!({"id":"SYNTHETIC-pending","status":"unknown","submission":{"kind":"SYNTHETIC","reference":"SYNTHETIC-tx"}});
    store
        .create_request(
            "SYNTHETIC-pending",
            "SYNTHETIC-pending-request",
            "SYNTHETIC-pending-fingerprint",
            "bridge.transfer",
            None,
            "unknown",
            &operation,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    store
        .conn
        .execute("INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES ('SYNTHETIC-pending','SYNTHETIC','SYNTHETIC-tx','{}')", [])
        .unwrap();
    store.conn.execute_batch(
        "DROP TABLE IF EXISTS settlement_progress; DROP TABLE IF EXISTS signer_nonces; DROP TABLE bridge_evidence_ids; DROP TABLE bridge_allocation_owners; DROP TABLE bridge_transit; DROP TABLE bridge_step_links; DROP TABLE bridge_quote_revisions; DROP TABLE bridge_settlement_proofs; DROP TABLE bridge_transfers; DROP TABLE IF EXISTS unresolved_cost_events; DROP TABLE IF EXISTS unresolved_cost_consumption; DROP TABLE IF EXISTS grouped_amendment_scopes; DROP TABLE IF EXISTS grouped_amendment_children; DROP TABLE IF EXISTS grouped_amendment_revisions; DROP TABLE IF EXISTS grouped_amendments; DROP TABLE IF EXISTS compound_amendment_children; DROP TABLE IF EXISTS operation_amendments; DROP TABLE IF EXISTS operation_proof_events; DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=3;",

    ).unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened
            .conn
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        reopened.get_record("SYNTHETIC-pending").unwrap(),
        Some(operation)
    );
    assert_eq!(reopened.reservation_count("SYNTHETIC-pending").unwrap(), 0);
    let reference: String = reopened
        .conn
        .query_row(
            "SELECT reference FROM submissions WHERE operation_id='SYNTHETIC-pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reference, "SYNTHETIC-tx");
}

#[test]
fn boundary_source_proof_atomically_releases_lock_hands_off_budget_and_grant_usage() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    transfer.state = BridgeTransferState::SourcePending;
    store.save_bridge_transfer(&transfer).unwrap();
    let grant_before =
        json!({"id":"SYNTHETIC-grant-1","spent":"0","active_attempt":"SYNTHETIC-deposit-op"});
    let grant_after = json!({"id":"SYNTHETIC-grant-1","spent":"10000000","active_attempt":null});
    store
        .create_boundary_authorization(
            "SYNTHETIC-grant-1",
            "SYNTHETIC-grant-request",
            "SYNTHETIC-transfer-1",
            "SYNTHETIC-grant-fingerprint",
            &grant_before,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

    let operation = json!({
        "id":"SYNTHETIC-deposit-op", "request_id":"SYNTHETIC-deposit-request",
        "fingerprint":"SYNTHETIC-deposit-fingerprint", "operation":"bridge.transfer",
        "status":"submitted", "submission":{"kind":"SYNTHETIC","reference":"SYNTHETIC-deposit-tx","data":{}},
        "dispatch_evidence":null, "effects":null
    });
    store
        .create_request(
            "SYNTHETIC-deposit-op",
            "SYNTHETIC-deposit-request",
            "SYNTHETIC-deposit-fingerprint",
            "bridge.transfer",
            None,
            "submitted",
            &operation,
            "2026-01-01T00:00:01Z",
        )
        .unwrap();
    store
        .reserve(
            "SYNTHETIC-deposit-op",
            &[ReservationRow {
                resource: "lock:SYNTHETIC-wallet".into(),
                units: "0".into(),
                fee_units: "0".into(),
            }],
        )
        .unwrap();
    store
        .persist_submission(
            "SYNTHETIC-deposit-op",
            &SubmissionRow {
                kind: "SYNTHETIC".into(),
                reference: "SYNTHETIC-deposit-tx".into(),
                data: json!({}),
            },
            "2026-01-01T00:00:02Z",
        )
        .unwrap();
    let operation = store.get_record("SYNTHETIC-deposit-op").unwrap().unwrap();
    store
        .attach_boundary_attempt(
            "SYNTHETIC-grant-1",
            "SYNTHETIC-deposit-op",
            "2026-01-01T00:00:02Z",
        )
        .unwrap();

    let deposit_step = BridgeStepLink {
        transfer_id: transfer.id.clone(),
        grant_id: None,
        plan_authority: None,
        step_id: "SYNTHETIC-deposit-step".into(),
        operation_id: None,
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    store.link_bridge_step(&deposit_step).unwrap();
    transfer = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    transfer.version += 1;
    transfer.grant_id = Some("SYNTHETIC-grant-1".into());
    transfer.steps[0].grant_id = transfer.grant_id.clone();
    store.save_bridge_transfer(&transfer).unwrap();
    let mut transit = TransitResource {
        transfer_id: transfer.id.clone(),
        amount: Amount {
            asset: AssetId {
                network: "SYNTHETIC-chain".into(),
                reference: "SYNTHETIC-token".into(),
                decimals: 6,
            },
            atoms: Quantity::new("1000000").unwrap(),
        },
        evidence_id: "SYNTHETIC:source-evidence-1".into(),
        destination_locator: None,
        proof_digest: "SYNTHETIC-source-proof".into(),
        observed_at: "2026-01-01T00:01:00Z".into(),
        spendable: false,
    };
    let mut updated = transfer.clone();
    updated.version += 1;
    updated.state = BridgeTransferState::InTransit;
    updated.allocation_owner = BridgeAllocationOwner::Transfer {
        transfer_id: transfer.id.clone(),
    };
    updated.applied_proofs.push(transit.proof_digest.clone());
    updated.transit = Some(transit.clone());
    updated.steps[0].operation_id = Some("SYNTHETIC-deposit-op".into());
    updated.steps[0].state = BridgeStepState::Settled;
    let mut terminal = operation.clone();
    terminal["status"] = json!("completed");
    terminal["updated_at"] = json!("2026-01-01T00:01:00Z");
    terminal["effects"] = json!([{"kind":"cash_debit"},{"kind":"transit_principal"}]);
    let settlement = Settlement {
        operation_id: "SYNTHETIC-deposit-op".into(),
        terminal_record: terminal,
        proof: json!({"proof":"SYNTHETIC-source-proof"}),
        effects: json!([{"kind":"cash_debit"},{"kind":"transit_principal"}]),
        run_id: None,
        run_record: None,
    };
    assert!(matches!(
        store.commit_boundary_bridge_source_handoff(
            &settlement,
            "SYNTHETIC-grant-1",
            &json!({"stale":true}),
            &grant_after,
            &updated,
            &transit,
            "10000000",
            "2026-01-01T00:01:00Z"
        ),
        Err(StoreError::ConcurrentReservation)
    ));
    assert_eq!(
        store.get_record("SYNTHETIC-deposit-op").unwrap(),
        Some(operation.clone())
    );
    assert_eq!(store.reservation_count("SYNTHETIC-deposit-op").unwrap(), 1);
    assert!(store.bridge_allocation_owners().unwrap().is_empty());

    let mut bad_locator_transit = transit.clone();
    bad_locator_transit.destination_locator = Some(gofin_domain::BridgeDestinationLocator {
        message_hash: None,
        scheme: "SYNTHETIC-message-v1".into(),
        source_evidence_id: "SYNTHETIC-unrelated-source-proof".into(),
        destination_chain_id: Some(42161),
        destination_network: None,
        transaction_hash: format!("0x{}", "a".repeat(64)),
        proof: json!({"message_index":"9"}),
    });
    let mut bad_locator_transfer = updated.clone();
    bad_locator_transfer.transit = Some(bad_locator_transit.clone());
    assert!(matches!(
        store.commit_boundary_bridge_source_handoff(
            &settlement,
            "SYNTHETIC-grant-1",
            &grant_before,
            &grant_after,
            &bad_locator_transfer,
            &bad_locator_transit,
            "10000000",
            "2026-01-01T00:01:00Z"
        ),
        Err(StoreError::BridgeConflict)
    ));
    let mut invalid_hash_transit = transit.clone();
    invalid_hash_transit.destination_locator = Some(gofin_domain::BridgeDestinationLocator {
        message_hash: None,
        scheme: "SYNTHETIC-message-v1".into(),
        source_evidence_id: invalid_hash_transit.evidence_id.clone(),
        destination_chain_id: Some(42161),
        destination_network: None,
        transaction_hash: "0x1234".into(),
        proof: json!({"message_index":"9"}),
    });
    let mut invalid_hash_transfer = updated.clone();
    invalid_hash_transfer.transit = Some(invalid_hash_transit.clone());
    assert!(matches!(
        store.commit_boundary_bridge_source_handoff(
            &settlement,
            "SYNTHETIC-grant-1",
            &grant_before,
            &grant_after,
            &invalid_hash_transfer,
            &invalid_hash_transit,
            "10000000",
            "2026-01-01T00:01:00Z"
        ),
        Err(StoreError::BridgeConflict)
    ));

    transit.destination_locator = Some(gofin_domain::BridgeDestinationLocator {
        message_hash: None,
        scheme: "SYNTHETIC-message-v1".into(),
        source_evidence_id: transit.evidence_id.clone(),
        destination_chain_id: Some(42161),
        destination_network: None,
        transaction_hash: format!("0x{}", "b".repeat(64)),
        proof: json!({"message_index":"10"}),
    });
    updated.transit = Some(transit.clone());

    store
        .commit_boundary_bridge_source_handoff(
            &settlement,
            "SYNTHETIC-grant-1",
            &grant_before,
            &grant_after,
            &updated,
            &transit,
            "10000000",
            "2026-01-01T00:01:00Z",
        )
        .unwrap();
    assert_eq!(store.reservation_count("SYNTHETIC-deposit-op").unwrap(), 0);
    assert_eq!(store.bridge_allocation_owners().unwrap().len(), 1);
    assert_eq!(store.bridge_transit(&transfer.id).unwrap(), Some(transit));
    assert_eq!(
        store.boundary_authorization("SYNTHETIC-grant-1").unwrap(),
        Some(grant_after)
    );
    assert_eq!(store.bridge_transfer(&transfer.id).unwrap(), Some(updated));

    let mut completed = store
        .bridge_transfer("SYNTHETIC-transfer-1")
        .unwrap()
        .unwrap();
    completed.version += 1;
    completed.state = BridgeTransferState::Completed;
    completed.transit = None;
    completed
        .applied_proofs
        .push("SYNTHETIC-destination-proof".into());
    let mut destination_proof = BridgeSettlementProof {
        transfer_id: completed.id.clone(),
        outcome: BridgeSettlementOutcome::Destination,
        evidence_id: format!("ethereum:42161:0x{}:transaction", "b".repeat(64)),
        effects: vec![],
        evidence: json!({
            "chain_id":42161,
            "transaction_hash":format!("0x{}", "b".repeat(64)),
            "correlation":"SYNTHETIC-destination-proof"
        }),
        observed_at: "2026-01-02T00:00:00Z".into(),
        proof_digest: String::new(),
    };
    destination_proof.proof_digest =
        gofin_domain::bridge_settlement_digest(&destination_proof).unwrap();
    completed.applied_proofs.pop();
    completed
        .applied_proofs
        .push(destination_proof.proof_digest.clone());
    completed.settlement_proofs.push(destination_proof.clone());
    store
        .commit_bridge_transfer_settlement(&completed, &destination_proof, "2026-01-02T00:00:00Z")
        .unwrap();
    store
        .commit_bridge_transfer_settlement(&completed, &destination_proof, "2026-01-02T00:00:00Z")
        .unwrap();
    assert_eq!(store.bridge_transit("SYNTHETIC-transfer-1").unwrap(), None);
    let owners = store.bridge_allocation_owners().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].2, "10000000");
    assert_eq!(owners[0].4.as_deref(), Some("2026-01-02T00:00:00Z"));
    assert!(matches!(
        store.commit_bridge_transfer_settlement(
            &completed,
            &BridgeSettlementProof {
                evidence: json!({"different":true}),
                ..destination_proof
            },
            "2026-01-02T00:00:00Z"
        ),
        Err(StoreError::BridgeConflict)
    ));
}

#[test]
fn bridge_attempt_atomically_claims_only_declared_ready_step_once() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    let deposit = BridgeStepLink {
        transfer_id: transfer.id.clone(),
        grant_id: None,
        plan_authority: None,
        step_id: "SYNTHETIC-deposit-step".into(),
        operation_id: None,
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    store.save_bridge_transfer(&transfer).unwrap();
    store.link_bridge_step(&deposit).unwrap();
    let mut current = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    current.version += 1;
    current.grant_id = Some("SYNTHETIC-grant-1".into());
    current.steps[0].grant_id = current.grant_id.clone();
    store.save_bridge_transfer(&current).unwrap();
    let deposit = current.steps[0].clone();
    current = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    let before = json!({"id":"SYNTHETIC-grant-1","active_attempt":null,"spent":"0"});
    store
        .create_boundary_authorization(
            "SYNTHETIC-grant-1",
            "SYNTHETIC-grant-request",
            &transfer.id,
            "SYNTHETIC-grant-fingerprint",
            &before,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    let operation_id = "SYNTHETIC-claimed-deposit-operation";
    let updated = json!({"id":"SYNTHETIC-grant-1","active_attempt":operation_id,"spent":"0"});
    let record = json!({"id":operation_id,"status":"awaiting_approval"});
    store
        .create_boundary_bridge_attempt(
            "SYNTHETIC-grant-1",
            &before,
            &updated,
            operation_id,
            "SYNTHETIC-deposit-request",
            "SYNTHETIC-deposit-fingerprint",
            "bridge.transfer",
            &record,
            "2026-01-01T00:00:01Z",
            &BridgeStepLink {
                operation_id: Some(operation_id.into()),
                state: BridgeStepState::Submitted,
                ..deposit.clone()
            },
        )
        .unwrap();
    let claimed = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    let claimed_step = claimed
        .steps
        .iter()
        .find(|step| step.step_id == deposit.step_id)
        .unwrap();
    assert_eq!(claimed_step.operation_id.as_deref(), Some(operation_id));
    assert_eq!(claimed_step.state, BridgeStepState::Submitted);
    assert_eq!(claimed.version, current.version + 1);
    let selected_bounds = json!({"max_source_debit":"1000000","native_fee_cap":"20000"});
    let expectation = claimed.settlement_expectation.clone().unwrap();
    let scoped = store
        .bind_bridge_approval_scope(
            &transfer.id,
            Some("SYNTHETIC-grant-1"),
            &deposit.step_id,
            operation_id,
            &selected_bounds,
            &expectation,
        )
        .unwrap();
    assert_eq!(scoped.approved_bounds, Some(selected_bounds.clone()));
    assert_eq!(scoped.version, claimed.version + 1);
    assert!(matches!(
        store.bind_bridge_approval_scope(
            &transfer.id,
            Some("SYNTHETIC-grant-1"),
            &deposit.step_id,
            operation_id,
            &json!({"max_source_debit":"1000001","native_fee_cap":"20000"}),
            &expectation,
        ),
        Err(StoreError::BridgeConflict)
    ));

    let unrelated = BridgeStepLink {
        operation_id: Some("SYNTHETIC-second-operation".into()),
        ..deposit
    };
    assert!(matches!(
        store.create_boundary_bridge_attempt(
            "SYNTHETIC-grant-1",
            &updated,
            &json!({"id":"SYNTHETIC-grant-1","active_attempt":"SYNTHETIC-second-operation","spent":"0"}),
            "SYNTHETIC-second-operation",
            "SYNTHETIC-second-request",
            "SYNTHETIC-second-fingerprint",
            "bridge.transfer",
            &json!({"id":"SYNTHETIC-second-operation","status":"awaiting_approval"}),
            "2026-01-01T00:00:02Z",
            &BridgeStepLink {
                state: BridgeStepState::Submitted,
                ..unrelated
            },
        ),
        Err(StoreError::BridgeConflict) | Err(StoreError::ConcurrentReservation)
    ));
    assert_eq!(
        store.bridge_transfer(&transfer.id).unwrap().unwrap(),
        scoped
    );
}

#[test]
fn boundary_grant_scope_and_transfer_binding_commit_together() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    transfer.approved_bounds = None;
    transfer.steps = vec![
        BridgeStepLink {
            transfer_id: transfer.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: "SYNTHETIC-approval".into(),
            operation_id: None,
            kind: BridgeStepKind::Approval,
            depends_on: vec![],
            state: BridgeStepState::Unsubmitted,
        },
        BridgeStepLink {
            transfer_id: transfer.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: "SYNTHETIC-deposit".into(),
            operation_id: None,
            kind: BridgeStepKind::Deposit,
            depends_on: vec!["SYNTHETIC-approval".into()],
            state: BridgeStepState::Unsubmitted,
        },
    ];
    transfer.prospective_debit = Some(BridgeProspectiveDebit {
        deposit_step_id: "SYNTHETIC-deposit".into(),
        resource: ResourceId("SYNTHETIC-source-wallet".into()),
        maximum: Amount {
            asset: AssetId {
                network: "SYNTHETIC-source-chain".into(),
                reference: "SYNTHETIC-USDC".into(),
                decimals: 6,
            },
            atoms: Quantity::new("1000000").unwrap(),
        },
    });
    store.save_bridge_transfer(&transfer).unwrap();
    let bounds = json!({"maximum_source_debit":"1000000","native_fee_cap":"25000"});
    let scope = transfer.settlement_expectation.clone().unwrap();
    store.create_boundary_bridge_authorization(
        "SYNTHETIC-finite-grant",
        "SYNTHETIC-finite-grant-request",
        "SYNTHETIC-bridge-intent",
        "SYNTHETIC-grant-fingerprint",
        &json!({"authorization":{"id":"SYNTHETIC-finite-grant"},"request_fingerprint":"SYNTHETIC-grant-fingerprint"}),
        "2026-01-01T00:00:00Z",
        &transfer.id,
        "SYNTHETIC-approval",
        &BridgeStepKind::Approval,
        &bounds,
        &scope,
        transfer.prospective_debit.as_ref(),
    ).unwrap();
    let saved = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    assert_eq!(saved.grant_id.as_deref(), Some("SYNTHETIC-finite-grant"));
    assert_eq!(saved.approved_bounds, Some(bounds));
    assert_eq!(saved.settlement_expectation, Some(scope));
    assert_eq!(saved.prospective_debit, transfer.prospective_debit);
    assert!(
        saved
            .steps
            .iter()
            .all(|step| step.grant_id.as_deref() == Some("SYNTHETIC-finite-grant"))
    );
    assert!(
        store
            .boundary_authorization("SYNTHETIC-finite-grant")
            .unwrap()
            .is_some()
    );
    let mut weakened_scope = saved.clone();
    weakened_scope.version += 1;
    weakened_scope
        .prospective_debit
        .as_mut()
        .unwrap()
        .maximum
        .atoms = Quantity::new("1000001").unwrap();
    assert!(matches!(
        store.save_bridge_transfer(&weakened_scope),
        Err(StoreError::BridgeConflict)
    ));
}

#[test]
fn prospective_debit_must_name_exactly_one_declared_deposit() {
    let mut store = Store::in_memory().unwrap();
    let mut record = transfer();
    record.grant_id = None;
    record.prospective_debit = Some(BridgeProspectiveDebit {
        deposit_step_id: "SYNTHETIC-missing-deposit".into(),
        resource: ResourceId("SYNTHETIC-source-wallet".into()),
        maximum: Amount {
            asset: AssetId {
                network: "SYNTHETIC-source-chain".into(),
                reference: "SYNTHETIC-USDC".into(),
                decimals: 6,
            },
            atoms: Quantity::new("1000000").unwrap(),
        },
    });
    assert!(matches!(
        store.save_bridge_transfer(&record),
        Err(StoreError::BridgeConflict)
    ));
}

#[test]
fn selected_prospective_cap_can_replace_quote_hint_only_before_a_step_claim() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    transfer.steps = vec![
        BridgeStepLink {
            transfer_id: transfer.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: "SYNTHETIC-approval-before-cap".into(),
            operation_id: None,
            kind: BridgeStepKind::Approval,
            depends_on: vec![],
            state: BridgeStepState::Unsubmitted,
        },
        BridgeStepLink {
            transfer_id: transfer.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: "SYNTHETIC-deposit-before-cap".into(),
            operation_id: None,
            kind: BridgeStepKind::Deposit,
            depends_on: vec!["SYNTHETIC-approval-before-cap".into()],
            state: BridgeStepState::Unsubmitted,
        },
    ];
    transfer.prospective_debit = Some(BridgeProspectiveDebit {
        deposit_step_id: "SYNTHETIC-deposit-before-cap".into(),
        resource: ResourceId("SYNTHETIC-source-wallet".into()),
        maximum: Amount {
            asset: AssetId {
                network: "SYNTHETIC-source-chain".into(),
                reference: "SYNTHETIC-USDC".into(),
                decimals: 6,
            },
            atoms: Quantity::new("10000000").unwrap(),
        },
    });
    store.save_bridge_transfer(&transfer).unwrap();

    // The quote's prospective maximum is only a planning hint. A human may
    // select a larger finite source cap before an authorization/step is bound.
    let mut selected = transfer.clone();
    selected.version += 1;
    selected.prospective_debit.as_mut().unwrap().maximum.atoms = Quantity::new("12000000").unwrap();
    store.save_bridge_transfer(&selected).unwrap();

    // Once an operation has claimed a plan step, the prospective debit is
    // frozen even if the operation has not yet reached venue submission.
    let operation_id = "SYNTHETIC-claimed-approval-operation";
    let claimed = BridgeStepLink {
        operation_id: Some(operation_id.into()),
        state: BridgeStepState::Submitted,
        ..selected.steps[0].clone()
    };
    store
        .create_bridge_attempt(
            operation_id,
            "SYNTHETIC-claimed-approval-request",
            "SYNTHETIC-claimed-approval-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &json!({"id":operation_id,"status":"awaiting_approval"}),
            "2026-01-01T00:00:00Z",
            &claimed,
        )
        .unwrap();
    let mut changed_after_claim = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    changed_after_claim.version += 1;
    changed_after_claim
        .prospective_debit
        .as_mut()
        .unwrap()
        .maximum
        .atoms = Quantity::new("13000000").unwrap();
    assert!(matches!(
        store.save_bridge_transfer(&changed_after_claim),
        Err(StoreError::BridgeConflict)
    ));
}

#[test]
fn selected_cap_after_reset_requires_the_exact_awaiting_approval_selection() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    transfer.steps = vec![
        BridgeStepLink {
            transfer_id: transfer.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: "SYNTHETIC-reset-approved-cap".into(),
            operation_id: None,
            kind: BridgeStepKind::ResetAllowance,
            depends_on: vec![],
            state: BridgeStepState::Unsubmitted,
        },
        BridgeStepLink {
            transfer_id: transfer.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: "SYNTHETIC-approval-approved-cap".into(),
            operation_id: None,
            kind: BridgeStepKind::Approval,
            depends_on: vec!["SYNTHETIC-reset-approved-cap".into()],
            state: BridgeStepState::Unsubmitted,
        },
        BridgeStepLink {
            transfer_id: transfer.id.clone(),
            grant_id: None,
            plan_authority: None,
            step_id: "SYNTHETIC-deposit-approved-cap".into(),
            operation_id: None,
            kind: BridgeStepKind::Deposit,
            depends_on: vec!["SYNTHETIC-approval-approved-cap".into()],
            state: BridgeStepState::Unsubmitted,
        },
    ];
    transfer.prospective_debit = Some(BridgeProspectiveDebit {
        deposit_step_id: "SYNTHETIC-deposit-approved-cap".into(),
        resource: ResourceId("SYNTHETIC-source-wallet".into()),
        maximum: Amount {
            asset: AssetId {
                network: "SYNTHETIC-source-chain".into(),
                reference: "SYNTHETIC-USDC".into(),
                decimals: 6,
            },
            atoms: Quantity::new("10000000").unwrap(),
        },
    });
    store.save_bridge_transfer(&transfer).unwrap();

    let reset_id = "SYNTHETIC-reset-approved-cap-op";
    let reset_initial = json!({
        "id":reset_id,"request_id":"SYNTHETIC-reset-approved-cap-request",
        "fingerprint":"SYNTHETIC-reset-approved-cap-fingerprint",
        "operation":"bridge.transfer","status":"awaiting_approval","submission":null
    });
    store
        .create_bridge_attempt(
            reset_id,
            "SYNTHETIC-reset-approved-cap-request",
            "SYNTHETIC-reset-approved-cap-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &reset_initial,
            "2026-01-01T00:00:00Z",
            &BridgeStepLink {
                operation_id: Some(reset_id.into()),
                state: BridgeStepState::Submitted,
                ..transfer.steps[0].clone()
            },
        )
        .unwrap();
    let reset_submission = SubmissionRow {
        kind: "SYNTHETIC".into(),
        reference: "SYNTHETIC-reset-approved-cap-tx".into(),
        data: json!({}),
    };
    store
        .persist_submission(reset_id, &reset_submission, "2026-01-01T00:00:01Z")
        .unwrap();
    let mut reset_terminal = reset_initial;
    reset_terminal["status"] = json!("completed");
    reset_terminal["submission"] = json!({
        "kind":reset_submission.kind,
        "reference":reset_submission.reference,
        "data":reset_submission.data
    });
    store
        .commit_bridge_step_settlement(
            &Settlement {
                operation_id: reset_id.into(),
                terminal_record: reset_terminal,
                proof: json!({"evidence":"SYNTHETIC-reset-approved-cap-proof"}),
                effects: json!([]),
                run_id: None,
                run_record: None,
            },
            &transfer.id,
            &transfer.steps[0].step_id,
            None,
            &BridgeStepKind::ResetAllowance,
            "2026-01-01T00:00:02Z",
        )
        .unwrap();

    let selected_atoms = Quantity::new("12000000").unwrap();
    let approval_id = "SYNTHETIC-approval-approved-cap-op";
    let approval_input = json!({
        "transferId":transfer.id,
        "stepId":transfer.steps[1].step_id,
        "stepKind":"approval",
        "bounds":{"maximum_source_debit":selected_atoms}
    });
    let approval_initial = json!({
        "id":approval_id,"request_id":"SYNTHETIC-approval-approved-cap-request",
        "fingerprint":"SYNTHETIC-approval-approved-cap-fingerprint",
        "operation":"bridge.transfer","input":approval_input,
        "status":"awaiting_approval","submission":null
    });
    store
        .create_bridge_attempt(
            approval_id,
            "SYNTHETIC-approval-approved-cap-request",
            "SYNTHETIC-approval-approved-cap-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &approval_initial,
            "2026-01-01T00:00:03Z",
            &BridgeStepLink {
                operation_id: Some(approval_id.into()),
                state: BridgeStepState::Submitted,
                ..transfer.steps[1].clone()
            },
        )
        .unwrap();

    let mut selected = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    selected.version += 1;
    selected.prospective_debit.as_mut().unwrap().maximum.atoms = selected_atoms.clone();
    store.save_bridge_transfer(&selected).unwrap();

    // A different candidate cap cannot be smuggled through the same claimed
    // Approval operation, even while that operation remains unsubmitted.
    let mut changed = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    changed.version += 1;
    changed.prospective_debit.as_mut().unwrap().maximum.atoms = Quantity::new("13000000").unwrap();
    let mut approved_operation = store.get_record(approval_id).unwrap().unwrap();
    approved_operation["input"]["bounds"]["maximum_source_debit"] = json!("13000000");
    approved_operation["approval"] = json!({"mechanism":"synthetic test audit"});
    store
        .update_record(approval_id, &approved_operation, "2026-01-01T00:00:04Z")
        .unwrap();
    assert!(matches!(
        store.save_bridge_transfer(&changed),
        Err(StoreError::BridgeConflict)
    ));
}

#[test]
fn definite_pre_submission_failure_releases_only_the_claimed_bridge_step() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    let step = BridgeStepLink {
        transfer_id: transfer.id.clone(),
        grant_id: None,
        plan_authority: None,
        step_id: "SYNTHETIC-exact-approval-step".into(),
        operation_id: None,
        kind: BridgeStepKind::Approval,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    transfer.steps.push(step.clone());
    store.save_bridge_transfer(&transfer).unwrap();
    store.link_bridge_step(&step).unwrap();
    transfer = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    let input = json!({
        "transferId": transfer.id,
        "stepId": step.step_id,
        "stepKind": "approval"
    });
    let operation_id = "SYNTHETIC-exact-approval-operation";
    let initial = json!({
        "id": operation_id,
        "request_id": "SYNTHETIC-exact-approval-request",
        "fingerprint": "SYNTHETIC-exact-approval-fingerprint",
        "operation": "bridge.transfer",
        "input": input,
        "status": "awaiting_approval",
        "submission": null
    });
    let claimed = BridgeStepLink {
        operation_id: Some(operation_id.into()),
        state: BridgeStepState::Submitted,
        ..step.clone()
    };
    store
        .create_bridge_attempt(
            operation_id,
            "SYNTHETIC-exact-approval-request",
            "SYNTHETIC-exact-approval-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &initial,
            "2026-01-01T00:00:00Z",
            &claimed,
        )
        .unwrap();
    let mut terminal = initial;
    terminal["status"] = json!("rejected");
    terminal["updated_at"] = json!("2026-01-01T00:00:01Z");
    terminal["error"] = json!({"code":"HUMAN_REJECTED"});
    store
        .commit_settlement(
            &Settlement {
                operation_id: operation_id.into(),
                terminal_record: terminal,
                proof: Value::Null,
                effects: json!([]),
                run_id: None,
                run_record: None,
            },
            "2026-01-01T00:00:01Z",
        )
        .unwrap();
    let updated = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    assert_eq!(updated.steps[0].state, BridgeStepState::Unsubmitted);
    assert_eq!(updated.steps[0].operation_id, None);
    assert_eq!(updated.version, transfer.version + 2);
    let retry_id = "SYNTHETIC-explicit-retry-operation";
    let retry = json!({
        "id":retry_id,
        "request_id":"SYNTHETIC-explicit-retry-request",
        "fingerprint":"SYNTHETIC-explicit-retry-fingerprint",
        "operation":"bridge.transfer",
        "input":{"transfer_id":transfer.id,"step_id":step.step_id,"step_kind":"approval"},
        "status":"awaiting_approval",
        "submission":null
    });
    store
        .create_bridge_attempt(
            retry_id,
            "SYNTHETIC-explicit-retry-request",
            "SYNTHETIC-explicit-retry-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &retry,
            "2026-01-01T00:00:02Z",
            &BridgeStepLink {
                operation_id: Some(retry_id.into()),
                state: BridgeStepState::Submitted,
                ..step
            },
        )
        .unwrap();
    let retried = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    assert_eq!(retried.steps[0].operation_id.as_deref(), Some(retry_id));
    assert_eq!(retried.steps[0].state, BridgeStepState::Submitted);
}

#[test]
fn proven_reverted_deposit_keeps_identity_and_never_creates_transit() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    let step = BridgeStepLink {
        transfer_id: transfer.id.clone(),
        grant_id: None,
        plan_authority: None,
        step_id: "SYNTHETIC-reverted-deposit".into(),
        operation_id: None,
        kind: BridgeStepKind::Deposit,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    transfer.steps.push(step.clone());
    store.save_bridge_transfer(&transfer).unwrap();
    let operation_id = "SYNTHETIC-reverted-deposit-operation";
    let mut operation = json!({
        "id":operation_id,
        "request_id":"SYNTHETIC-reverted-deposit-request",
        "fingerprint":"SYNTHETIC-reverted-deposit-fingerprint",
        "operation":"bridge.transfer",
        "input":{"transferId":transfer.id,"stepId":step.step_id,"stepKind":"deposit"},
        "status":"awaiting_approval",
        "submission":null
    });
    store
        .create_bridge_attempt(
            operation_id,
            "SYNTHETIC-reverted-deposit-request",
            "SYNTHETIC-reverted-deposit-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &operation,
            "2026-01-01T00:00:00Z",
            &BridgeStepLink {
                operation_id: Some(operation_id.into()),
                state: BridgeStepState::Submitted,
                ..step.clone()
            },
        )
        .unwrap();
    let submission = SubmissionRow {
        kind: "SYNTHETIC_EVM".into(),
        reference: "0xSYNTHETIC_REVERT_TX".into(),
        data: json!({"correlation":"SYNTHETIC-order"}),
    };
    store
        .persist_submission(operation_id, &submission, "2026-01-01T00:00:01Z")
        .unwrap();
    operation["status"] = json!("failed");
    operation["updated_at"] = json!("2026-01-01T00:00:02Z");
    operation["submission"] =
        json!({"kind":submission.kind,"reference":submission.reference,"data":submission.data});
    let settlement = Settlement {
        operation_id: operation_id.into(),
        terminal_record: operation,
        proof: json!({"evidence":"SYNTHETIC-reverted-receipt"}),
        effects: json!([{"kind":"cost","category":"gas","amount":"SYNTHETIC-fee"}]),
        run_id: None,
        run_record: None,
    };
    store
        .commit_bridge_failed_deposit(
            &settlement,
            &transfer.id,
            &step.step_id,
            None,
            "2026-01-01T00:00:02Z",
            None,
        )
        .unwrap();
    let failed = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    assert_eq!(failed.state, BridgeTransferState::Failed);
    assert_eq!(failed.transit, None);
    assert_eq!(
        failed.allocation_owner,
        BridgeAllocationOwner::Operation { operation_id: None }
    );
    assert_eq!(failed.steps[0].state, BridgeStepState::NeedsAttention);
    assert_eq!(failed.steps[0].operation_id.as_deref(), Some(operation_id));
}

#[test]
fn proved_approval_settles_only_its_submitted_step_atomically() {
    let mut store = Store::in_memory().unwrap();
    let mut transfer = transfer();
    transfer.grant_id = None;
    store.save_bridge_transfer(&transfer).unwrap();
    let step = BridgeStepLink {
        transfer_id: transfer.id.clone(),
        grant_id: None,
        plan_authority: None,
        step_id: "SYNTHETIC-approval-step".into(),
        operation_id: None,
        kind: BridgeStepKind::Approval,
        depends_on: vec![],
        state: BridgeStepState::Unsubmitted,
    };
    store.link_bridge_step(&step).unwrap();
    let operation_id = "SYNTHETIC-approval-op";
    let initial = json!({
        "id":operation_id,"request_id":"SYNTHETIC-approval-request",
        "fingerprint":"SYNTHETIC-approval-fingerprint","operation":"bridge.transfer",
        "input":{"transfer_id":transfer.id,"step_id":step.step_id,"step_kind":"approval"},
        "status":"awaiting_approval","submission":null
    });
    store
        .create_bridge_attempt(
            operation_id,
            "SYNTHETIC-approval-request",
            "SYNTHETIC-approval-fingerprint",
            "bridge.transfer",
            None,
            "awaiting_approval",
            &initial,
            "2026-01-01T00:00:00Z",
            &BridgeStepLink {
                operation_id: Some(operation_id.into()),
                state: BridgeStepState::Submitted,
                ..step.clone()
            },
        )
        .unwrap();
    store
        .persist_submission(
            operation_id,
            &SubmissionRow {
                kind: "SYNTHETIC".into(),
                reference: "SYNTHETIC-approval-tx".into(),
                data: json!({}),
            },
            "2026-01-01T00:00:01Z",
        )
        .unwrap();
    let mut terminal = initial;
    terminal["status"] = json!("completed");
    terminal["updated_at"] = json!("2026-01-01T00:00:02Z");
    terminal["submission"] =
        json!({"kind":"SYNTHETIC","reference":"SYNTHETIC-approval-tx","data":{}});
    let settlement = Settlement {
        operation_id: operation_id.into(),
        terminal_record: terminal,
        proof: json!({"evidence":"SYNTHETIC-approval-receipt"}),
        effects: json!([]),
        run_id: None,
        run_record: None,
    };
    store
        .commit_bridge_step_settlement(
            &settlement,
            &transfer.id,
            &step.step_id,
            None,
            &BridgeStepKind::Approval,
            "2026-01-01T00:00:02Z",
        )
        .unwrap();
    let updated = store.bridge_transfer(&transfer.id).unwrap().unwrap();
    assert_eq!(updated.steps[0].state, BridgeStepState::Settled);
    assert_eq!(updated.steps[0].operation_id.as_deref(), Some(operation_id));
    assert_eq!(updated.state, BridgeTransferState::ApprovalPending);
    assert_eq!(
        store.get_record(operation_id).unwrap().unwrap()["status"],
        "completed"
    );
}
