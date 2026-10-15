use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fin_bridge::relay::solana::{
    DEPOSITORY_PROGRAM, ExpectedPayout, VerifiedHubPayoutWitness, relay_vault_address,
    verify_payout_receipt,
};
use fin_solana::transaction::NormalizedAccountMeta;
use fin_solana::{FinalizedReceiptProof, NormalizedInstruction};
use sha2::{Digest, Sha256};

#[tokio::test]
async fn verified_relay_solana_payout_passes_engine_settlement_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("relay-solana-settlement.sqlite3");
    let engine = synthetic_engine(&path, Arc::new(SyntheticAdapter::new()));
    let source_operation = engine
        .execute(ordinary_request("relay-solana-source-budget-op"))
        .await
        .unwrap();
    let recipient = bs58::encode([7u8; 32]).into_string();
    let signature = bs58::encode([9u8; 64]).into_string();
    let vault = relay_vault_address().unwrap();
    let order_id = format!("0x{}", "11".repeat(32));
    let transfer_id = "SYNTHETIC-relay-solana-payout".to_owned();
    let resource = ResourceId(format!("solana:mainnet-beta:{recipient}"));
    let sol = asset("solana:mainnet-beta", "native", 9);
    let source = Amount {
        asset: asset("ethereum:1", "native", 18),
        atoms: quantity("10000000000000000"),
    };
    let transit = fin_domain::TransitResource {
        transfer_id: transfer_id.clone(),
        amount: source,
        evidence_id: "SYNTHETIC-source-receipt".into(),
        destination_locator: None,
        proof_digest: "SYNTHETIC-source-proof".into(),
        observed_at: now(),
        spendable: false,
    };
    let transfer = fin_domain::BridgeTransferRecord {
        version: 1,
        id: transfer_id.clone(),
        grant_id: None,
        plan_scope: None,
        metadata: json!({"fixture":"synthetic Relay Solana payout"}),
        settlement_expectation: Some(fin_domain::BridgeSettlementExpectation {
            destination: vec![fin_domain::BridgeSettlementTarget {
                resource: resource.clone(),
                asset: sol.clone(),
                minimum_atoms: quantity("5000000"),
            }],
            refund: vec![],
        }),
        approved_bounds: Some(json!({"synthetic":"approved bounds"})),
        prospective_debit: None,
        state: fin_domain::BridgeTransferState::InTransit,
        quote_revisions: vec![],
        steps: vec![],
        allocation_owner: fin_domain::BridgeAllocationOwner::Transfer {
            transfer_id: transfer_id.clone(),
        },
        transit: Some(transit),
        applied_proofs: vec!["SYNTHETIC-source-proof".into()],
        settlement_proofs: vec![],
    };
    {
        let mut store = engine.store.lock().unwrap();
        store.save_bridge_transfer(&transfer).unwrap();
        store
            .record_bridge_transit(transfer.transit.as_ref().unwrap())
            .unwrap();
        store
            .handoff_bridge_allocation(
                &source_operation.id,
                &transfer.id,
                "1000000",
                &source_operation.updated_at,
            )
            .unwrap();
    }

    let mut payload = vec![7; 32];
    payload.extend(bs58::decode(&recipient).into_vec().unwrap());
    payload.push(0); // native SOL
    payload.extend(5_000_000u64.to_le_bytes());
    payload.extend(42u64.to_le_bytes());
    payload.extend(1_800_000_000i64.to_le_bytes());
    payload.extend(bs58::decode(&vault).into_vec().unwrap());
    let witness = VerifiedHubPayoutWitness {
        order_id: order_id.clone(),
        order_address: format!("0x{}", "22".repeat(20)),
        withdraw_request_hash: format!("0x{}", "33".repeat(32)),
        payload_hex: format!("0x{}", hex::encode(&payload)),
        payload_sha256: hex::encode(Sha256::digest(&payload)),
    };
    let mut instruction_data = Sha256::digest(b"global:execute_transfer")[..8].to_vec();
    instruction_data.extend(payload);
    let account = |address: String| NormalizedAccountMeta {
        address,
        signer: false,
        writable: true,
    };
    let mut accounts = vec![account(fin_bridge::relay::solana::SYSTEM_PROGRAM.into()); 12];
    accounts[2] = account(recipient.clone());
    accounts[3] = account(vault);
    let proof = FinalizedReceiptProof {
        signature: signature.clone(),
        slot: 10,
        fee_lamports: 5000,
        failed: false,
        message_sha256: "SYNTHETIC-message".into(),
        account_keys: vec![
            recipient.clone(),
            fin_bridge::relay::solana::SYSTEM_PROGRAM.into(),
        ],
        message_instructions: vec![
            NormalizedInstruction {
                program_id: "Ed25519SigVerify111111111111111111111111111".into(),
                accounts: vec![],
                data_base64: "".into(),
            },
            NormalizedInstruction {
                program_id: DEPOSITORY_PROGRAM.into(),
                accounts,
                data_base64: STANDARD.encode(instruction_data),
            },
        ],
        required_signatures: vec![signature.clone()],
        pre_balances_lamports: vec![100, 100],
        post_balances_lamports: vec![5_000_100, 100],
        pre_token_balances: vec![],
        post_token_balances: vec![],
        loaded_addresses: json!({"writable":[],"readonly":[]}),
        inner_instructions: vec![],
        log_messages: vec![],
        error: None,
    };
    let payout = verify_payout_receipt(
        &proof,
        &witness,
        &ExpectedPayout {
            recipient: recipient.clone(),
            mint: None,
            token_program: None,
            decimals: 9,
            minimum_amount_atoms: quantity("5000000"),
            order_id,
        },
    )
    .unwrap();
    assert_eq!(payout.instruction_index, 1);
    let evidence_id = format!(
        "solana:mainnet-beta:{signature}:{}",
        payout.instruction_index
    );
    let evidence = json!({
        "transfer_id":transfer_id,"evidence_id":evidence_id,
        "network":"solana:mainnet-beta","receipt_scope":"instruction",
        "transaction_hash":signature,"instruction_index":payout.instruction_index,
        "recipient":recipient,"recipient_resource":resource,
        "asset":sol,"amount_atoms":payout.net_amount_atoms,
        "net_amount_atoms":payout.net_amount_atoms,"correlation":payout.order_id,
        "hub_witness":witness,"payout":payout,"proof":proof,
    });
    let mut settlement = fin_domain::BridgeSettlementProof {
        transfer_id: transfer_id.clone(),
        outcome: fin_domain::BridgeSettlementOutcome::Destination,
        evidence_id: evidence_id.clone(),
        effects: vec![FinancialEffect::UsableCredit {
            to: resource,
            amount: Amount {
                asset: sol,
                atoms: payout.net_amount_atoms.clone(),
            },
        }],
        evidence,
        observed_at: now(),
        proof_digest: String::new(),
    };
    settlement.proof_digest = fin_domain::bridge_settlement_digest(&settlement).unwrap();
    let mut wrong_index = settlement.clone();
    wrong_index.evidence["instruction_index"] = json!(2);
    wrong_index.proof_digest = fin_domain::bridge_settlement_digest(&wrong_index).unwrap();
    assert_eq!(
        engine
            .commit_bridge_external_settlement(wrong_index)
            .unwrap_err()
            .code,
        "BRIDGE_SETTLEMENT_INVALID"
    );
    let settled = engine
        .commit_bridge_external_settlement(settlement)
        .unwrap();
    assert_eq!(settled.state, fin_domain::BridgeTransferState::Completed);
    assert!(settled.transit.is_none());
}

#[test]
fn relay_solana_source_instruction_identity_becomes_transit_principal() {
    let wallet = bs58::encode([5u8; 32]).into_string();
    let signature = bs58::encode([6u8; 64]).into_string();
    let transfer_id = "SYNTHETIC-relay-solana-source";
    let evidence_id = format!("solana:mainnet-beta:{signature}:2");
    let resource = ResourceId(format!("solana:mainnet-beta:{wallet}"));
    let amount = Amount {
        asset: asset("solana:mainnet-beta", "native", 9),
        atoms: quantity("5000000"),
    };
    let mut operation = record(vec![], vec![]);
    let mut verified = proof(
        &operation,
        vec![
            FinancialEffect::CashDebit {
                resource: resource.clone(),
                amount: amount.clone(),
            },
            FinancialEffect::TransitPrincipal {
                transfer_id: transfer_id.into(),
                amount: amount.clone(),
            },
        ],
    );
    verified.evidence = json!({
        "transfer_id":transfer_id,"evidence_id":evidence_id,
        "network":"solana:mainnet-beta","receipt_scope":"instruction",
        "transaction_hash":signature,"instruction_index":2,
        "recipient":wallet,"recipient_resource":resource,"resource":resource,
        "asset":amount.asset,"amount_atoms":amount.atoms,
        "correlation":format!("0x{}", "11".repeat(32)),
    });
    verified.proof_digest = fin_domain::verified_effect_digest(&verified).unwrap();
    assert!(bridge_event_identity_matches(
        &verified.evidence,
        transfer_id,
        &evidence_id
    ));
    assert!(bridge_evidence_matches_resource(
        &verified.evidence,
        &resource
    ));
    operation.effects = Some(verified);
    let transit = transfer_principal(&operation).unwrap().unwrap();
    assert_eq!(transit.evidence_id, evidence_id);
    assert_eq!(transit.amount, amount);
}
