//! Independently encoded synthetic vectors using Foundry cast and the reviewed
//! Base/OP Stack contract ABI. No RPC or financial writes occur in these tests.
use fin_bridge::{BridgeIntent, base_canonical::*, canonical::CanonicalRawLog};
use fin_domain::Quantity;
use serde_json::{Value, json};

fn cases() -> Vec<Value> {
    let data: Value =
        serde_json::from_str(include_str!("fixtures/base-canonical-vectors.json")).unwrap();
    assert_eq!(
        data["provenance"]["contracts_revision"],
        "5f915b1e471457fa924b8daabdc5289eca361f24"
    );
    assert_eq!(data["provenance"]["synthetic"], true);
    data["vectors"].as_array().unwrap().clone()
}
fn intent(case: &Value) -> BridgeIntent {
    serde_json::from_value(case["intent"].clone()).unwrap()
}
fn logs(case: &Value, name: &str) -> Vec<CanonicalRawLog> {
    serde_json::from_value(case[name].clone()).unwrap()
}

#[test]
fn independent_native_and_erc20_source_destination_vectors_both_directions() {
    let cases = cases();
    assert_eq!(cases.len(), 4);
    for case in cases {
        let intent = intent(&case);
        let source = logs(&case, "source_logs");
        let destination = logs(&case, "destination_logs");
        assert_eq!(
            verify_destination_logs(
                &destination,
                &intent,
                case["message_hash"].as_str().unwrap()
            )
            .unwrap(),
            8
        );
        if intent.source.as_ref().unwrap().chain_id == 1 {
            let proof = verify_deposit_source(
                &source,
                &intent,
                case["source_block_hash"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(proof.source_hash, case["source_hash"]);
            assert_eq!(proof.transaction_hash, case["transaction_hash"]);
            assert_eq!(proof.message_hash, case["message_hash"]);
            let tx = json!({"type":"0x7e","hash":proof.transaction_hash,"sourceHash":proof.source_hash,"from":proof.from,"to":proof.to,"mint":format!("{:#x}",fin_evm::u256(&proof.mint).unwrap()),"value":format!("{:#x}",fin_evm::u256(&proof.value).unwrap()),"gas":format!("{:#x}",proof.gas),"input":proof.data,"isSystemTx":false,"nonce":"0x123"});
            verify_deposit_transaction(&tx, &proof).unwrap();
            // Public OP RPCs may omit false isSystemTx; the reconstructed
            // type-0x7e envelope hash still binds the false flag.
            let mut omitted = tx.clone();
            omitted.as_object_mut().unwrap().remove("isSystemTx");
            verify_deposit_transaction(&omitted, &proof).unwrap();
            for malformed in [json!(null), json!("false"), json!(0)] {
                let mut bad = omitted.clone();
                bad["isSystemTx"] = malformed;
                assert!(verify_deposit_transaction(&bad, &proof).is_err());
            }
            // OP receipt nonce is irrelevant; type/sourceHash are indispensable.
            let mut bad = tx.clone();
            bad["type"] = json!("0x64");
            assert!(verify_deposit_transaction(&bad, &proof).is_err());
            let mut bad = tx.clone();
            bad["sourceHash"] = json!(format!("0x{}", "aa".repeat(32)));
            assert!(verify_deposit_transaction(&bad, &proof).is_err());
            let mut bad = tx.clone();
            bad["isSystemTx"] = json!(true);
            assert!(verify_deposit_transaction(&bad, &proof).is_err());
            let mut tampered = proof.clone();
            tampered.portal_log_index += 1;
            assert!(verify_deposit_transaction(&tx, &tampered).is_err());
        } else {
            let proof = verify_withdrawal_source(&source, &intent).unwrap();
            assert_eq!(proof.withdrawal_hash, case["withdrawal_hash"]);
            assert_eq!(proof.message_hash, case["message_hash"]);
            assert_eq!(
                format!(
                    "{:#x}",
                    withdrawal_storage_key(proof.withdrawal.hash().unwrap())
                ),
                case["storage_key"]
            );
            assert_eq!(
                serde_json::to_value(&proof.withdrawal).unwrap(),
                case["withdrawal"]
            );
            assert_eq!(
                encode_finalize_call(&proof.withdrawal, &intent.source_address)
                    .unwrap()
                    .data,
                case["finalize_call"]
            );
            let witness: BaseWithdrawalWitness =
                serde_json::from_value(case["witness"].clone()).unwrap();
            assert_eq!(
                format!("{:#x}", witness.root().unwrap()),
                case["witness"]["output_root"]
            );
            assert_eq!(
                encode_prove_call(&proof.withdrawal, &witness).unwrap().data,
                case["prove_call"]
            );
            let mut changed = witness.clone();
            changed.version = format!("0x{}01", "00".repeat(31));
            assert!(encode_prove_call(&proof.withdrawal, &changed).is_err());
            let mut changed = witness.clone();
            changed.withdrawal_proof = vec![format!("0x{}", "00".repeat(65_537))];
            assert!(encode_prove_call(&proof.withdrawal, &changed).is_err());
            let mut modified = proof.withdrawal.clone();
            modified.gas_limit = Quantity::from_u64(1);
            assert_ne!(modified.hash().unwrap(), proof.withdrawal.hash().unwrap());
        }
    }
}

#[test]
fn source_correlations_reject_changed_principal_recipient_asset_and_missing_or_duplicate_events() {
    for case in cases() {
        let intent = intent(&case);
        let source = logs(&case, "source_logs");
        let verify = |logs: &[CanonicalRawLog], intent: &BridgeIntent| {
            if intent.source.as_ref().unwrap().chain_id == 1 {
                verify_deposit_source(logs, intent, case["source_block_hash"].as_str().unwrap())
                    .map(|_| ())
            } else {
                verify_withdrawal_source(logs, intent).map(|_| ())
            }
        };
        let mut altered = intent.clone();
        altered.amount = Quantity::from_u64(10);
        assert!(verify(&source, &altered).is_err());
        let mut altered = intent.clone();
        altered.recipient = "0x9999999999999999999999999999999999999999".into();
        assert!(verify(&source, &altered).is_err());
        assert!(verify(&source[..1], &intent).is_err());
        let mut duplicate = source.clone();
        duplicate.push(source[1].clone());
        assert!(verify(&duplicate, &intent).is_err());
        let mut altered = source.clone();
        altered[1].address = "0x9999999999999999999999999999999999999999".into();
        assert!(verify(&altered, &intent).is_err());
        let mut altered = source.clone();
        altered[1].log_index = altered[0].log_index;
        assert!(verify(&altered, &intent).is_err());
        if intent.source.as_ref().unwrap().token.is_some() {
            let mut altered = intent.clone();
            altered.destination_assets[0].token =
                Some("0x9999999999999999999999999999999999999999".into());
            assert!(verify(&source, &altered).is_err());
        }
    }
}

#[test]
fn destination_requires_source_message_and_exact_finalized_event() {
    for case in cases() {
        let intent = intent(&case);
        let dest = logs(&case, "destination_logs");
        let hash = case["message_hash"].as_str().unwrap();
        assert!(verify_destination_logs(&dest[..1], &intent, hash).is_err());
        assert!(
            verify_destination_logs(&dest, &intent, &format!("0x{}", "66".repeat(32))).is_err()
        );
        let mut bad = dest.clone();
        bad[0].address = PORTAL.into();
        assert!(verify_destination_logs(&bad, &intent, hash).is_err());
        let mut bad = intent.clone();
        bad.amount = Quantity::from_u64(1);
        assert!(verify_destination_logs(&dest, &bad, hash).is_err());
        let mut duplicate = dest.clone();
        duplicate.push(dest[1].clone());
        assert!(verify_destination_logs(&duplicate, &intent, hash).is_err());
    }
}

#[test]
fn successful_receipt_alone_does_not_finalize_a_withdrawal() {
    let case = cases()
        .into_iter()
        .find(|c| c["intent"]["source"]["chain_id"] == 8453)
        .unwrap();
    let hash = case["withdrawal_hash"].as_str().unwrap();
    // Event signature independently pinned to Solidity's ABI.
    let signature = format!(
        "{:#x}",
        alloy::primitives::keccak256("WithdrawalFinalized(bytes32,bool)")
    );
    let mut finalized = CanonicalRawLog {
        log_index: 1,
        address: PORTAL.into(),
        topics: vec![signature, hash.into()],
        data: format!("0x{}01", "00".repeat(31)),
    };
    assert_eq!(
        verify_withdrawal_finalized(&[finalized.clone()], hash).unwrap(),
        1
    );
    finalized.data = format!("0x{}", "00".repeat(32));
    assert!(verify_withdrawal_finalized(&[finalized], hash).is_err());
    assert!(verify_withdrawal_finalized(&[], hash).is_err());
}

#[test]
fn proof_receipt_binds_the_immutable_withdrawal_and_exact_submitter() {
    let case = cases()
        .into_iter()
        .find(|c| c["intent"]["source"]["chain_id"] == 8453)
        .unwrap();
    let withdrawal: BaseWithdrawal = serde_json::from_value(case["withdrawal"].clone()).unwrap();
    let wallet = "0x1111111111111111111111111111111111111111";
    let address_word = |address: &str| format!("0x{}{}", "00".repeat(12), &address[2..]);
    let proven = CanonicalRawLog {
        log_index: 11,
        address: PORTAL.into(),
        topics: vec![
            format!(
                "{:#x}",
                alloy::primitives::keccak256("WithdrawalProven(bytes32,address,address)")
            ),
            case["withdrawal_hash"].as_str().unwrap().into(),
            address_word(&withdrawal.sender),
            address_word(&withdrawal.target),
        ],
        data: "0x".into(),
    };
    let extension = CanonicalRawLog {
        log_index: 12,
        address: PORTAL.into(),
        topics: vec![
            format!(
                "{:#x}",
                alloy::primitives::keccak256("WithdrawalProvenExtension1(bytes32,address)")
            ),
            case["withdrawal_hash"].as_str().unwrap().into(),
            address_word(wallet),
        ],
        data: "0x".into(),
    };
    assert_eq!(
        verify_withdrawal_proven(&[proven.clone(), extension.clone()], &withdrawal, wallet)
            .unwrap(),
        11
    );
    assert!(verify_withdrawal_proven(std::slice::from_ref(&proven), &withdrawal, wallet).is_err());
    assert!(
        verify_withdrawal_proven(
            &[proven.clone(), extension.clone()],
            &withdrawal,
            "0x9999999999999999999999999999999999999999"
        )
        .is_err()
    );
    let mut modified = withdrawal.clone();
    modified.nonce = Quantity::from_u64(10);
    assert!(verify_withdrawal_proven(&[proven, extension], &modified, wallet).is_err());
}
