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

fn assert_pending(store: &Store, pending: bool) {
    assert_eq!(
        resource_has_pending_operations(store, &ResourceId(RESOURCE.into())).unwrap(),
        pending
    );
    assert_eq!(
        hyperliquid_owner_has_pending_operations(store, OWNER).unwrap(),
        pending
    );
}

#[test]
fn enrollment_guards_honor_persisted_human_success_and_failure_after_restart() {
    for outcome in ["succeeded", "failed"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.sqlite");
        let mut store = Store::open(&path).unwrap();
        let mut review = seed(&mut store, "resolved");
        let original = store.get_record("resolved").unwrap();
        let reservations = store.reservations("resolved").unwrap();
        assert_pending(&store, true);
        review["attestation"]["outcome"] = json!(outcome);
        store
            .persist_operation_resolution(
                "human",
                &review,
                &approval(&review, "operation_resolution"),
                None,
                "t3",
            )
            .unwrap();
        assert_pending(&store, false);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_pending(&store, false);
        let original_config = owner_config();
        let mut removed = original_config.clone();
        removed.connections.clear();
        crate::enrollment::ensure_connection_changes_safe(&store, &original_config, &removed)
            .unwrap();
        assert_eq!(store.get_record("resolved").unwrap(), original);
        assert_eq!(store.reservations("resolved").unwrap(), reservations);
        seed(&mut store, "still-unknown");
        assert_pending(&store, true);
    }
}

#[test]
fn enrollment_guards_preserve_unresolved_work_after_reservation_release() {
    let mut store = Store::in_memory().unwrap();
    let review = seed(&mut store, "unresolved");
    store
        .persist_operation_lock_override(
            "human",
            &review,
            &approval(&review, "reservation_override"),
            "t3",
        )
        .unwrap();
    assert!(store.all_effective_reservations().unwrap().is_empty());
    assert_pending(&store, true);
}

#[test]
fn enrollment_guards_preserve_retained_lifecycle_holds_after_human_resolution() {
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
    assert_pending(&store, true);
}

#[test]
fn enrollment_guards_reject_invalid_and_failed_human_resolutions() {
    let mut store = Store::in_memory().unwrap();
    let review = seed(&mut store, "invalid");
    let mut invalid_approval = approval(&review, "operation_resolution");
    invalid_approval["binding_hash"] = json!("wrong");
    assert!(
        store
            .persist_operation_resolution("human", &review, &invalid_approval, None, "t3")
            .is_err()
    );
    assert_pending(&store, true);
    // The low-level store writer can seed an invalid review version, but its
    // authoritative reader rejects it. Enrollment must propagate that failure.
    let mut malformed = review.clone();
    malformed["version"] = json!(2);
    assert!(
        store
            .persist_operation_resolution(
                "human",
                &malformed,
                &approval(&malformed, "operation_resolution"),
                None,
                "t3",
            )
            .is_err()
    );
    assert!(store.operation_resolution("invalid").is_err());
    assert_eq!(
        resource_has_pending_operations(&store, &ResourceId(RESOURCE.into()))
            .unwrap_err()
            .code,
        "STORE_INVALID_RECORD"
    );
    assert_eq!(
        hyperliquid_owner_has_pending_operations(&store, OWNER)
            .unwrap_err()
            .code,
        "STORE_INVALID_RECORD"
    );
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
    assert_pending(&store, true);
}

#[test]
fn enrollment_mapping_guard_blocks_pending_owner_and_derived_mapping_changes() {
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
    assert_eq!(
        crate::enrollment::ensure_connection_changes_safe(&store, &original, &changed_scope)
            .unwrap_err()
            .code,
        "CONNECTION_HAS_PENDING_OPERATION"
    );
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
            let error =
                crate::enrollment::ensure_connection_changes_safe(&store, &original, &candidate)
                    .unwrap_err();
            assert_eq!(error.code, "CONNECTION_HAS_PENDING_OPERATION");
        }
    }
}

#[test]
fn enrollment_mapping_guard_retains_resolution_holds_only_on_affected_mappings() {
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
    assert_eq!(
        crate::enrollment::ensure_connection_changes_safe(&store, &original, &removed)
            .unwrap_err()
            .code,
        "CONNECTION_HAS_PENDING_OPERATION"
    );
    assert_pending(&store, true);
}

#[test]
fn enrollment_mapping_guard_fails_closed_on_corrupt_touched_operation() {
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
    assert_eq!(
        crate::enrollment::ensure_connection_changes_safe(&store, &original, &removed)
            .unwrap_err()
            .code,
        "STORE_INVALID_RECORD"
    );
}
