//! Persistence invariants for explicit recovery of a deferred mint.
use super::*;
use gofin_domain::{
    BridgeStepKind as K, BridgeStepState as S, BridgeTransferRecord, BridgeTransferState as T,
};

pub(super) fn validate_recovery_append(
    old: &BridgeTransferRecord,
    next: &BridgeTransferRecord,
) -> Result<()> {
    let invalid = || StoreError::BridgeConflict;
    if old.grant_id.is_some()
        || old.approved_bounds.is_none()
        || !matches!(
            old.state,
            T::InTransit | T::DestinationPending | T::NeedsAttention
        )
        || next.state != T::DestinationPending
        || next.version != old.version.checked_add(1).ok_or_else(invalid)?
        || next.steps.len() != old.steps.len() + 1
        || next.quote_revisions.len() != old.quote_revisions.len() + 1
        || old.steps.iter().any(|step| {
            step.state == S::Submitted || (step.kind == K::Mint && step.state == S::Unsubmitted)
        })
    {
        return Err(invalid());
    }
    let transit = old.transit.as_ref().ok_or_else(invalid)?;
    let locator = transit.destination_locator.as_ref().ok_or_else(invalid)?;
    if locator.scheme != "cctp_v2_message" || !valid_transit_locator(transit) {
        return Err(invalid());
    }
    let prior = old.quote_revisions.last().ok_or_else(invalid)?;
    let revision = next.quote_revisions.last().ok_or_else(invalid)?;
    if prior.provider != "cctp"
        || prior.payload["quote"]["intent"]["destination_chain_id"]
            != json!(locator.destination_chain_id)
        || prior.payload["quote"]["intent"]["solana_destination"]["network"]
            != json!(locator.destination_network)
        || next.quote_revisions[..old.quote_revisions.len()] != old.quote_revisions
        || revision.provider != prior.provider
        || revision.route_id != prior.route_id
        || revision.order_id != prior.order_id
        || revision.valid_until != prior.valid_until
        || revision.revision != prior.revision.checked_add(1).ok_or_else(invalid)?
        || gofin_domain::binding_hash(&revision.payload).map_err(|_| invalid())?
            != revision.quote_hash
    {
        return Err(invalid());
    }
    let mut normalized = next.clone();
    normalized.version = old.version;
    normalized.state = old.state.clone();
    normalized.steps = old.steps.clone();
    normalized.quote_revisions = old.quote_revisions.clone();
    if normalized != *old || next.steps[..old.steps.len()] != old.steps {
        return Err(invalid());
    }
    let marker = &revision.payload["cctp_recovery"];
    if marker["reason"] != "stage_reverted"
        || marker["message_hash"].as_str() != locator.message_hash.as_deref()
    {
        return Err(invalid());
    }
    let parent = old
        .steps
        .iter()
        .find(|step| Some(step.step_id.as_str()) == marker["parent_step_id"].as_str())
        .ok_or_else(invalid)?;
    if parent.kind != K::Mint
        || parent.state != S::NeedsAttention
        || parent.operation_id.is_none()
        || parent.operation_id.as_deref() != marker["parent_operation_id"].as_str()
    {
        return Err(invalid());
    }
    let deposit = old
        .steps
        .iter()
        .find(|step| step.kind == K::Deposit && step.state == S::Settled)
        .ok_or_else(invalid)?;
    let addition = next.steps.last().ok_or_else(invalid)?;
    if addition.transfer_id != old.id
        || addition.kind != K::Mint
        || addition.grant_id.is_some()
        || addition.plan_authority.is_some()
        || addition.operation_id.is_some()
        || addition.state != S::Unsubmitted
        || addition.step_id.is_empty()
        || old
            .steps
            .iter()
            .any(|step| step.step_id == addition.step_id)
        || addition.depends_on != vec![deposit.step_id.clone()]
        || marker["step_ids"] != json!([addition.step_id])
    {
        return Err(invalid());
    }
    let prior_calls = prior.payload["plan"]["steps"]
        .as_array()
        .ok_or_else(invalid)?;
    let calls = revision.payload["plan"]["steps"]
        .as_array()
        .ok_or_else(invalid)?;
    let prior_ids = prior.payload["step_ids"].as_object().ok_or_else(invalid)?;
    let ids = revision.payload["step_ids"]
        .as_object()
        .ok_or_else(invalid)?;
    let ordinal = u8::try_from(prior_calls.len()).map_err(|_| invalid())?;
    if prior_calls.len() != old.steps.len()
        || calls.len() != prior_calls.len() + 1
        || calls[..prior_calls.len()] != *prior_calls
        || ids.len() != prior_ids.len() + 1
        || prior_ids
            .iter()
            .any(|(key, value)| ids.get(key) != Some(value))
        || ids.get(&ordinal.to_string()).and_then(Value::as_str) != Some(addition.step_id.as_str())
    {
        return Err(invalid());
    }
    let find_ordinal = |id: &str| {
        prior_ids
            .iter()
            .find_map(|(key, value)| {
                (value.as_str() == Some(id))
                    .then(|| key.parse::<u8>().ok())
                    .flatten()
            })
            .ok_or_else(invalid)
    };
    let deposit_ordinal = find_ordinal(&deposit.step_id)?;
    let parent_ordinal = find_ordinal(&parent.step_id)?;
    let mut expected_call = prior_calls
        .get(usize::from(parent_ordinal))
        .ok_or_else(invalid)?
        .clone();
    let evm_scope = locator.destination_chain_id.is_some()
        && expected_call["solana_mint"].is_null()
        && expected_call["call"]["chain_id"] == json!(locator.destination_chain_id)
        && expected_call["call"]["value"] == "0"
        && expected_call["call"]["data"] == "0x"
        && expected_call["expected_sender"] == prior.payload["quote"]["intent"]["source_address"];
    let solana_scope = locator.destination_network.as_deref() == Some("solana:mainnet-beta")
        && expected_call["call"].is_null()
        && expected_call["solana_mint"]["destination"]
            == prior.payload["quote"]["intent"]["solana_destination"]
        && expected_call["solana_mint"]["destination"]["network"] == "solana:mainnet-beta"
        && expected_call["expected_sender"]
            == prior.payload["quote"]["intent"]["solana_destination"]["wallet_address"]
        && expected_call["solana_mint"]["message_transmitter"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        && expected_call["solana_mint"]["token_messenger_minter"]
            .as_str()
            .is_some_and(|s| !s.is_empty());
    if expected_call["kind"] != "mint"
        || expected_call["maximum_debit"] != json!([])
        || !(evm_scope || solana_scope)
    {
        return Err(invalid());
    }
    expected_call["ordinal"] = json!(ordinal);
    expected_call["dependencies"] = json!([deposit_ordinal]);
    if calls.last() != Some(&expected_call) {
        return Err(invalid());
    }
    let mut payload = revision.payload.clone();
    payload["plan"]["steps"] = json!(prior_calls);
    payload["step_ids"] = prior.payload["step_ids"].clone();
    match prior.payload.get("cctp_recovery") {
        Some(value) => payload["cctp_recovery"] = value.clone(),
        None => {
            payload
                .as_object_mut()
                .ok_or_else(invalid)?
                .remove("cctp_recovery");
        }
    }
    if payload != prior.payload {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
    const TX: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";
    fn parent() -> BridgeTransferRecord {
        let quote = json!({"provider":"cctp","intent":{"source":{"chain_id":1},"destination_chain_id":42161,"source_address":"0x3333333333333333333333333333333333333333"}});
        let calls = json!([
            {"ordinal":0,"kind":"deposit","call":{"chain_id":1,"to":"SYNTHETIC-burn","value":"0","data":"0x1234"},"expected_sender":quote["intent"]["source_address"],"maximum_debit":[{"SYNTHETIC":"original principal"}],"dependencies":[]},
            {"ordinal":1,"kind":"mint","call":{"chain_id":42161,"to":"SYNTHETIC-transmitter","value":"0","data":"0x"},"expected_sender":quote["intent"]["source_address"],"maximum_debit":[],"dependencies":[0]}
        ]);
        serde_json::from_value(json!({"version":1,"id":"SYNTHETIC-cctp","grant_id":null,"metadata":{"SYNTHETIC":"approved route"},"settlement_expectation":null,"approved_bounds":{"SYNTHETIC":"caps"},"state":"needs_attention",
            "quote_revisions":[{"revision":1,"provider":"cctp","route_id":"SYNTHETIC-route","quote_hash":"SYNTHETIC-quote-hash","payload":{"quote":quote,"bounds":{"SYNTHETIC":"caps"},"plan":{"quote":quote,"steps":calls,"fixed_scope_digest":"SYNTHETIC-scope"},"step_ids":{"0":"burn","1":"mint"}},"observed_at":"2026-01-01T00:00:00Z","valid_until":"2026-01-01T01:00:00Z"}],
            "steps":[{"transfer_id":"SYNTHETIC-cctp","grant_id":null,"step_id":"burn","operation_id":"SYNTHETIC-burn-op","kind":"deposit","depends_on":[],"state":"settled"},{"transfer_id":"SYNTHETIC-cctp","grant_id":null,"step_id":"mint","operation_id":"SYNTHETIC-mint-op","kind":"mint","depends_on":["burn"],"state":"needs_attention"}],
            "allocation_owner":{"kind":"transfer","transfer_id":"SYNTHETIC-cctp"},"transit":{"transfer_id":"SYNTHETIC-cctp","amount":{"asset":{"network":"ethereum:1","reference":"SYNTHETIC-USDC","decimals":6},"atoms":"10000000"},"evidence_id":"SYNTHETIC-source-evidence","destination_locator":{"scheme":"cctp_v2_message","source_evidence_id":"SYNTHETIC-source-evidence","destination_chain_id":42161,"transaction_hash":"","message_hash":HASH,"proof":{"SYNTHETIC":"immutable burn proof"}},"proof_digest":"SYNTHETIC-burn-proof","observed_at":"2026-01-01T00:00:00Z","spendable":false},"applied_proofs":["SYNTHETIC-burn-proof"],"settlement_proofs":[]})).unwrap()
    }
    fn recovery(old: &BridgeTransferRecord) -> BridgeTransferRecord {
        let mut next = old.clone();
        next.version += 1;
        next.state = T::DestinationPending;
        let mut step = old.steps[1].clone();
        step.step_id = "mint-recovery".into();
        step.operation_id = None;
        step.plan_authority = None;
        step.state = S::Unsubmitted;
        next.steps.push(step);
        let mut revision = old.quote_revisions.last().unwrap().clone();
        revision.revision += 1;
        let mut call = revision.payload["plan"]["steps"][1].clone();
        call["ordinal"] = json!(2);
        revision.payload["plan"]["steps"]
            .as_array_mut()
            .unwrap()
            .push(call);
        revision.payload["step_ids"]["2"] = json!("mint-recovery");
        revision.payload["cctp_recovery"] = json!({"message_hash":HASH,"reason":"stage_reverted","parent_step_id":"mint","parent_operation_id":"SYNTHETIC-mint-op","step_ids":["mint-recovery"]});
        revision.quote_hash = gofin_domain::binding_hash(&revision.payload).unwrap();
        next.quote_revisions.push(revision);
        next
    }
    fn seed(store: &mut Store, old: &BridgeTransferRecord) {
        store
            .conn
            .execute(
                "INSERT INTO bridge_transfers(transfer_id,version,record_json) VALUES (?1,?2,?3)",
                params![old.id, old.version, serde_json::to_string(old).unwrap()],
            )
            .unwrap();
        for (id, status) in [
            ("SYNTHETIC-burn-op", "completed"),
            ("SYNTHETIC-mint-op", "failed"),
        ] {
            store
                .create_request(
                    id,
                    id,
                    id,
                    "bridge.transfer",
                    None,
                    status,
                    &json!({"status":status,"SYNTHETIC":"persisted submission and proved outcome"}),
                    "2026-01-01T00:00:00Z",
                )
                .unwrap();
        }
        for step in &old.steps {
            store.conn.execute("INSERT INTO bridge_step_links(transfer_id,step_id,operation_id,step_json) VALUES (?1,?2,?3,?4)",params![old.id,step.step_id,step.operation_id,serde_json::to_string(step).unwrap()]).unwrap();
        }
    }
    #[test]
    fn cctp_message_settlement_requires_immutable_burn_and_source_evidence() {
        let record = parent();
        let transit = record.transit.as_ref().unwrap();
        assert!(valid_transit_locator(transit));
        let proof = gofin_domain::BridgeSettlementProof {
            transfer_id: record.id.clone(),
            outcome: gofin_domain::BridgeSettlementOutcome::Destination,
            evidence_id: "SYNTHETIC-mint-event".into(),
            effects: vec![],
            evidence: json!({"chain_id":42161,"transaction_hash":TX,"message_hash":HASH,"source_evidence_id":"SYNTHETIC-source-evidence"}),
            observed_at: "2026-01-01T00:00:00Z".into(),
            proof_digest: "SYNTHETIC-mint-proof".into(),
        };
        assert!(settlement_locator_matches(Some(transit), &proof));
        for (field, value) in [
            ("chain_id", json!(8453)),
            ("transaction_hash", json!("")),
            ("message_hash", json!(TX)),
            ("source_evidence_id", json!("wrong-source")),
        ] {
            let mut bad = proof.clone();
            bad.evidence[field] = value;
            assert!(!settlement_locator_matches(Some(transit), &bad), "{field}");
        }
        let mut bad = transit.clone();
        bad.destination_locator.as_mut().unwrap().transaction_hash = TX.into();
        assert!(!valid_transit_locator(&bad));
        let mut bad = transit.clone();
        bad.destination_locator.as_mut().unwrap().message_hash = None;
        assert!(!valid_transit_locator(&bad));
    }
    #[test]
    fn cctp_recovery_preserves_burn_and_authority_and_is_compare_and_swap_safe() {
        let mut store = Store::in_memory().unwrap();
        let old = parent();
        seed(&mut store, &old);
        let next = recovery(&old);
        assert_eq!(store.append_cctp_recovery(&old, &next).unwrap(), next);
        assert_eq!(next.transit, old.transit);
        assert_eq!(next.steps[..2], old.steps);
        assert_eq!(next.approved_bounds, old.approved_bounds);
        assert!(next.steps[2].plan_authority.is_none());
        assert!(matches!(
            store.append_cctp_recovery(&old, &next),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(store.list_records().unwrap().len(), 2);
    }
    #[test]
    fn cctp_recovery_rejects_uncertainty_and_all_scope_changes() {
        for case in [
            "principal",
            "destination",
            "sender",
            "transmitter",
            "calldata",
            "message",
            "source",
            "dependency",
            "new_authority",
            "parent_operation",
            "parent_pending",
            "unsubmitted_recovery",
        ] {
            let old = parent();
            let mut next = recovery(&old);
            match case {
                "principal" => {
                    next.quote_revisions[1].payload["plan"]["steps"][2]["maximum_debit"] =
                        json!([{"amount":"1"}])
                }
                "destination" => {
                    next.quote_revisions[1].payload["plan"]["steps"][2]["call"]["chain_id"] =
                        json!(8453)
                }
                "sender" => {
                    next.quote_revisions[1].payload["plan"]["steps"][2]["expected_sender"] =
                        json!("other")
                }
                "transmitter" => {
                    next.quote_revisions[1].payload["plan"]["steps"][2]["call"]["to"] =
                        json!("other")
                }
                "calldata" => {
                    next.quote_revisions[1].payload["plan"]["steps"][2]["call"]["data"] =
                        json!("0x1234")
                }
                "message" => {
                    next.quote_revisions[1].payload["cctp_recovery"]["message_hash"] = json!(TX)
                }
                "source" => next.transit.as_mut().unwrap().evidence_id = "other".into(),
                "dependency" => next.steps[2].depends_on = vec!["mint".into()],
                "new_authority" => next.steps[2].grant_id = Some("other".into()),
                "parent_operation" => {
                    next.quote_revisions[1].payload["cctp_recovery"]["parent_operation_id"] =
                        json!("other")
                }
                "parent_pending" | "unsubmitted_recovery" => {}
                _ => unreachable!(),
            }
            next.quote_revisions[1].quote_hash =
                gofin_domain::binding_hash(&next.quote_revisions[1].payload).unwrap();
            let mut store = Store::in_memory().unwrap();
            seed(&mut store, &old);
            if case == "parent_pending" {
                store
                    .conn
                    .execute(
                        "UPDATE operations SET status='pending' WHERE id='SYNTHETIC-mint-op'",
                        [],
                    )
                    .unwrap();
            }
            if case == "unsubmitted_recovery" {
                let mut pending = old.clone();
                pending.steps[1].state = S::Unsubmitted;
                assert!(validate_recovery_append(&pending, &next).is_err());
                continue;
            }
            assert!(store.append_cctp_recovery(&old, &next).is_err(), "{case}");
            assert_eq!(store.bridge_transfer(&old.id).unwrap().unwrap(), old);
        }
    }
    fn solana_parent() -> BridgeTransferRecord {
        let mut record = parent();
        let locator = record
            .transit
            .as_mut()
            .unwrap()
            .destination_locator
            .as_mut()
            .unwrap();
        locator.destination_chain_id = None;
        locator.destination_network = Some("solana:mainnet-beta".into());
        let payload = &mut record.quote_revisions[0].payload;
        payload["quote"]["intent"]
            .as_object_mut()
            .unwrap()
            .remove("destination_chain_id");
        payload["quote"]["intent"]["solana_destination"] = json!({"network":"solana:mainnet-beta","wallet_address":"SYNTHETIC-owner","recipient_token_account":"SYNTHETIC-ATA"});
        payload["plan"]["steps"][1]
            .as_object_mut()
            .unwrap()
            .remove("call");
        payload["plan"]["steps"][1]["solana_mint"] = json!({"destination":payload["quote"]["intent"]["solana_destination"],"message_transmitter":"SYNTHETIC-transmitter","token_messenger_minter":"SYNTHETIC-messenger"});
        payload["plan"]["steps"][1]["expected_sender"] = json!("SYNTHETIC-owner");
        record
    }

    #[test]
    fn solana_locator_requires_exact_network_and_full_signature() {
        let record = solana_parent();
        let transit = record.transit.as_ref().unwrap();
        assert!(valid_transit_locator(transit));
        let signature = bs58::encode([7u8; 64]).into_string();
        let proof = gofin_domain::BridgeSettlementProof {
            transfer_id: record.id.clone(),
            outcome: gofin_domain::BridgeSettlementOutcome::Destination,
            evidence_id: "SYNTHETIC-mint".into(),
            effects: vec![],
            evidence: json!({"network":"solana:mainnet-beta","transaction_hash":signature,"message_hash":HASH,"source_evidence_id":"SYNTHETIC-source-evidence"}),
            observed_at: "2026-01-01T00:00:00Z".into(),
            proof_digest: "SYNTHETIC-proof".into(),
        };
        assert!(settlement_locator_matches(Some(transit), &proof));
        for (key, value) in [
            ("network", json!("solana:devnet")),
            ("transaction_hash", json!(TX)),
            (
                "transaction_hash",
                json!(bs58::encode([7u8; 32]).into_string()),
            ),
            ("chain_id", json!(1)),
            ("message_hash", json!(TX)),
            ("source_evidence_id", json!("unrelated")),
        ] {
            let mut bad = proof.clone();
            bad.evidence[key] = value;
            assert!(!settlement_locator_matches(Some(transit), &bad), "{key}");
        }
        let mut mixed = transit.clone();
        mixed
            .destination_locator
            .as_mut()
            .unwrap()
            .destination_chain_id = Some(8453);
        assert!(!valid_transit_locator(&mixed));
    }

    #[test]
    fn solana_recovery_preserves_destination_and_original_burn() {
        let old = solana_parent();
        let next = recovery(&old);
        assert!(validate_recovery_append(&old, &next).is_ok());
        for key in ["network", "wallet_address", "recipient_token_account"] {
            let mut bad = next.clone();
            bad.quote_revisions[1].payload["plan"]["steps"][2]["solana_mint"]["destination"][key] =
                json!("changed");
            bad.quote_revisions[1].quote_hash =
                gofin_domain::binding_hash(&bad.quote_revisions[1].payload).unwrap();
            assert!(validate_recovery_append(&old, &bad).is_err());
        }
    }

    #[test]
    fn v17_interrupted_upgrade_retains_evm_and_solana_pending_records() {
        for mut old in [parent(), solana_parent()] {
            old.id = "SYNTHETIC-cctp".into();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("journal.sqlite");
            {
                let mut store = Store::open(&path).unwrap();
                seed(&mut store, &old);
                store
                    .conn
                    .execute_batch("DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides;")
                    .unwrap();
                store.conn.pragma_update(None, "user_version", 17).unwrap();
                store
                    .conn
                    .execute_batch("BEGIN IMMEDIATE; DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=18;")
                    .unwrap();
            }
            let store = Store::open(&path).unwrap();
            assert_eq!(store.bridge_transfer(&old.id).unwrap().unwrap(), old);
            assert_eq!(
                store
                    .conn
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    .unwrap(),
                SCHEMA_VERSION
            );
            assert_eq!(store.list_records().unwrap().len(), 2);
        }
    }

    #[test]
    fn v16_marker_upgrade_and_interrupted_migration_preserve_pending_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite");
        let old = parent();
        {
            let mut store = Store::open(&path).unwrap();
            seed(&mut store, &old);
            store
                .conn
                .execute(
                    "UPDATE operations SET status='pending' WHERE id='SYNTHETIC-mint-op'",
                    [],
                )
                .unwrap();
            store
                .conn
                .execute_batch("DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides;")
                .unwrap();
            store.conn.pragma_update(None, "user_version", 16).unwrap();
            // Model interruption before committing the version marker.
            store
                .conn
                .execute_batch("BEGIN IMMEDIATE; DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=17;")
                .unwrap();
        }
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened
                .conn
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(reopened.bridge_transfer(&old.id).unwrap().unwrap(), old);
        let status: String = reopened
            .conn
            .query_row(
                "SELECT status FROM operations WHERE id='SYNTHETIC-mint-op'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(reopened.list_records().unwrap().len(), 2);
    }
}
