use super::*;
use gofin_domain::binding_hash;
use gofin_store::{ReservationRow, SubmissionRow};

const OWNER: &str = "0x1111111111111111111111111111111111111111";
const RESOURCE: &str = "hyperliquid:synthetic-owner";

fn seed(store: &mut Store, id: &str) -> Value {
    let request_id = format!("request-{id}");
    let fingerprint = format!("fingerprint-{id}");
    let record = gofin_domain::OperationRecord {
        id: id.into(),
        request_id: request_id.clone(),
        fingerprint: fingerprint.clone(),
        fingerprint_version: 1,
        operation: "hyperliquid.account.deposit".into(),
        input: json!({}),
        status: gofin_domain::Status::AwaitingApproval,
        created_at: now(),
        updated_at: now(),
        preview: Some(gofin_domain::PreparedOperation {
            operation: "hyperliquid.account.deposit".into(),
            input: json!({}),
            binding: json!({"owner": OWNER, "account":"synthetic-owner"}),
            summary: "Synthetic deposit".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: now(),
            expires_at: now(),
            valuation: None,
            intent: gofin_domain::FinancialIntent {
                locks: vec![ResourceId(RESOURCE.into())],
                ..Default::default()
            },
            operation_version: 1,
            action_deadline: None,
        }),
        approval: None,
        authorization_source: None,
        submission: None,
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    store
        .create_request(
            id,
            &request_id,
            &fingerprint,
            &record.operation,
            None,
            "awaiting_approval",
            &serde_json::to_value(&record).unwrap(),
            "t0",
        )
        .unwrap();
    store
        .reserve(
            id,
            &[ReservationRow {
                resource: format!("lock:{RESOURCE}"),
                units: "0".into(),
                fee_units: "0".into(),
            }],
        )
        .unwrap();
    store
        .persist_submission(
            id,
            &SubmissionRow {
                kind: "synthetic".into(),
                reference: format!("immutable-{id}"),
                data: json!({"nonce":17}),
            },
            "t1",
        )
        .unwrap();
    let mut current = store.get_record(id).unwrap().unwrap();
    current["status"] = json!("unknown");
    store.update_record(id, &current, "t2").unwrap();
    let rows: Vec<_> = store
        .reservations(id)
        .unwrap()
        .into_iter()
        .map(|row| json!({"resource":row.resource,"units":row.units,"fee_units":row.fee_units}))
        .collect();
    json!({"version":1,"operation_id":id,"original_request_id":request_id,
        "fingerprint":fingerprint,"record_hash":binding_hash(&current).unwrap(),
        "reservations_hash":binding_hash(&json!(rows)).unwrap(),
        "resources":[RESOURCE],"released_reservations":rows,"remaining_reservations":[],
        "submission":current["submission"],"plan_hash":null,
        "attestation":{"outcome":"succeeded","reason":"Synthetic human observation",
            "actual_debits":[],"actual_costs":[],"reported_receipts":[]}})
}

fn approval(review: &Value, action: &str) -> Value {
    json!({"source":"touch_id","binding_hash":binding_hash(&json!({
        "version":1,"action":action,"request_id":"human",
        "review_hash":binding_hash(review).unwrap(),"review":review})).unwrap()})
}

fn owner_config() -> BrokerConfigV2 {
    let mut stored = config();
    stored.connections.push(evm_wallet("original-owner", OWNER));
    crate::enrollment::effective_protected_config(stored).unwrap()
}

fn evm_wallet(name: &str, address: &str) -> ConnectionConfig {
    ConnectionConfig::Evm {
        name: name.into(),
        resource: ResourceId(format!("evm:{address}")),
        address: address.into(),
        credential_ref: Some("550e8400-e29b-41d4-a716-446655440000".into()),
        signing_enabled: true,
    }
}

fn unrelated_wallets() -> [ConnectionConfig; 2] {
    let solana_address = bs58::encode([2; 32]).into_string();
    [
        evm_wallet("test-evm", "0x2222222222222222222222222222222222222222"),
        ConnectionConfig::Solana {
            name: "test-solana".into(),
            resource: ResourceId(format!("solana:mainnet-beta:{solana_address}")),
            address: solana_address,
            credential_ref: Some("550e8400-e29b-41d4-a716-446655440001".into()),
            signing_enabled: true,
            network: "solana:mainnet-beta".into(),
        },
    ]
}

fn assert_unrelated_wallet_lifecycle(store: &Store, original: &BrokerConfigV2) {
    for wallet in unrelated_wallets() {
        let mut added = original.clone();
        added.connections.push(wallet);
        let added = crate::enrollment::effective_protected_config(added).unwrap();
        crate::enrollment::ensure_connection_changes_safe(store, original, &added).unwrap();
        // Removing the same new wallet also removes its derived Hyperliquid
        // alias; the original owner and its pending recovery remain untouched.
        crate::enrollment::ensure_connection_changes_safe(store, &added, original).unwrap();
    }
}

#[test]
fn enrollment_mapping_guard_allows_unrelated_wallet_lifecycle_with_unknown_owner() {
    let mut store = Store::in_memory().unwrap();
    seed(&mut store, "original-unknown");
    let original_record = store.get_record("original-unknown").unwrap();
    let original_holds = store.reservations("original-unknown").unwrap();
    assert_unrelated_wallet_lifecycle(&store, &owner_config());
    assert_eq!(
        store.get_record("original-unknown").unwrap(),
        original_record
    );
    assert_eq!(
        store.reservations("original-unknown").unwrap(),
        original_holds
    );
    assert!(store.get_record("original-unknown").unwrap().is_some());
}

#[test]
fn enrollment_changes_remain_available_with_pending_owner() {
    let mut store = Store::in_memory().unwrap();
    seed(&mut store, "original-unknown");
    let original = owner_config();
    assert_eq!(original.connections.len(), 2);
    assert!(matches!(
        original.connections[0],
        ConnectionConfig::Evm { .. }
    ));
    assert!(matches!(
        original.connections[1],
        ConnectionConfig::Hyperliquid {
            auto_derived: true,
            ..
        }
    ));
    let mut changed_scope = original.clone();
    changed_scope
        .networks
        .retain(|network| network.chain_id != 1);
    changed_scope.deployments.retain(|deployment| {
        changed_scope
            .networks
            .iter()
            .any(|network| network.id == deployment.network)
    });
    crate::enrollment::ensure_connection_changes_safe(&store, &original, &changed_scope).unwrap();
    for index in 0..original.connections.len() {
        let mut changed = original.clone();
        match &mut changed.connections[index] {
            ConnectionConfig::Evm { name, .. } | ConnectionConfig::Hyperliquid { name, .. } => {
                *name = "changed-alias".into();
            }
            _ => unreachable!(),
        }
        let mut removed = original.clone();
        removed.connections.remove(index);
        for candidate in [changed, removed] {
            crate::enrollment::ensure_connection_changes_safe(&store, &original, &candidate)
                .unwrap();
        }
    }
}

#[test]
fn enrollment_changes_preserve_attested_history_without_holding_configuration() {
    let mut store = Store::in_memory().unwrap();
    let mut review = seed(&mut store, "outcome-only");
    review["remaining_reservations"] = review["released_reservations"].clone();
    review["released_reservations"] = json!([]);
    review["resources"] = json!([]);
    store
        .persist_operation_resolution(
            "human",
            &review,
            &approval(&review, "operation_resolution"),
            None,
            "t3",
        )
        .unwrap();
    let original = owner_config();
    assert_unrelated_wallet_lifecycle(&store, &original);
    let mut removed = original.clone();
    removed.connections.clear();
    crate::enrollment::ensure_connection_changes_safe(&store, &original, &removed).unwrap();
    assert!(store.get_record("outcome-only").unwrap().is_some());
}

#[test]
fn enrollment_changes_do_not_parse_corrupt_operation_history() {
    let mut store = Store::in_memory().unwrap();
    let mut review = seed(&mut store, "corrupt-review");
    review["version"] = json!(2);
    assert!(
        store
            .persist_operation_resolution(
                "human",
                &review,
                &approval(&review, "operation_resolution"),
                None,
                "t3"
            )
            .is_err()
    );
    let original = owner_config();
    let mut removed = original.clone();
    removed.connections.clear();
    crate::enrollment::ensure_connection_changes_safe(&store, &original, &removed).unwrap();
}
