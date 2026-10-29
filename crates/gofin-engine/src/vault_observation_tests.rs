use super::*;
use gofin_domain::{
    Amount, AssetId, Quantity, SignedQuantity, VaultBackingValuation, VaultPositionEvidence,
    VaultTransitionBinding,
};

fn fixture() -> (PreparedOperation, VerifiedEffects, PositionAnchor) {
    let resource = ResourceId("solana:mainnet-beta:wallet".into());
    let asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "share-mint".into(),
        decimals: 6,
    };
    let position = "kamino:lending:vault:shares".to_owned();
    let submission = Submission {
        kind: "solana.transaction".into(),
        reference: "finalized-signature".into(),
        data: json!({}),
    };
    let evidence = VaultPositionEvidence {
        version: 1,
        family: "lending".into(),
        vault: "vault".into(),
        share_asset: asset.clone(),
        share_token_program: "token-program".into(),
        wallet_shares_atoms: Quantity::from_u64(80),
        staked_shares_atoms: Quantity::from_u64(30),
        pending_shares_atoms: Quantity::zero(),
        underlying: vec![],
        historical_principal_known: false,
        observed_slot: Some(42),
        observed_at: now(),
        backing_valuation: Some(VaultBackingValuation {
            total_usd_micros: Quantity::from_u64(123_000_000),
            observed_at: now(),
            source: "fixture".into(),
            complete: true,
            basis: "verified_underlying_exposure".into(),
            excluded_components: vec![],
        }),
        anchor_position_id: Some(position.clone()),
        pre_transaction_shares_atoms: Some(Quantity::from_u64(100)),
        finalized_submission: Some(submission.clone()),
        shares_delta_atoms: Some(SignedQuantity::new("10").unwrap()),
    };
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: position.clone(),
        position_kind: PositionKind::Investment,
        principal: Amount {
            asset: asset.clone(),
            atoms: Quantity::from_u64(110),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"vault_position":evidence}),
        observed_at: now(),
    };
    let binding = VaultTransitionBinding {
        version: 1,
        resource,
        position,
        asset,
        position_kind: PositionKind::Investment,
        pre_transaction_shares_atoms: Quantity::from_u64(100),
    };
    let prepared = PreparedOperation {
        operation: "kamino.deposit".into(),
        input: json!({}),
        binding: json!({gofin_domain::VAULT_TRANSITION_BINDING_KEY:binding}),
        summary: "fixture".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: now(),
        expires_at: now(),
        valuation: None,
        intent: FinancialIntent::default(),
        action_deadline: None,
        operation_version: 1,
    };
    let verified = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects: vec![],
        evidence: json!({}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    (prepared, verified, anchor)
}

#[test]
fn imported_vault_shares_rebase_only_from_bound_finalized_delta() {
    let (prepared, verified, anchor) = fixture();
    assert!(
        validate_vault_transition_rebase(
            &prepared,
            &verified,
            &anchor,
            &BigInt::from(10),
            &BigInt::from(110)
        )
        .unwrap()
    );
    for field in ["pre_transaction_shares_atoms", "shares_delta_atoms"] {
        let mut corrupt = anchor.clone();
        corrupt.evidence["vault_position"][field] = json!("11");
        assert!(
            validate_vault_transition_rebase(
                &prepared,
                &verified,
                &corrupt,
                &BigInt::from(10),
                &BigInt::from(110)
            )
            .is_err()
        );
    }
    let mut corrupt = anchor.clone();
    corrupt.evidence["vault_position"]["finalized_submission"]["reference"] =
        json!("another-signature");
    assert!(
        validate_vault_transition_rebase(
            &prepared,
            &verified,
            &corrupt,
            &BigInt::from(10),
            &BigInt::from(110)
        )
        .is_err()
    );
    let mut corrupt = anchor.clone();
    corrupt.evidence["vault_position"]["historical_principal_known"] = json!(true);
    assert!(
        validate_vault_transition_rebase(
            &prepared,
            &verified,
            &corrupt,
            &BigInt::from(10),
            &BigInt::from(110)
        )
        .is_err()
    );
}

#[test]
fn rounded_farm_entitlement_keeps_native_custody_out_of_owned_share_units() {
    let (prepared, verified, mut anchor) = fixture();
    anchor.principal.atoms = Quantity::from_u64(109);
    anchor.evidence["vault_position"]["wallet_shares_atoms"] = json!("80");
    anchor.evidence["vault_position"]["staked_shares_atoms"] = json!("29");
    anchor.evidence["vault_position"]["shares_delta_atoms"] = json!("9");
    anchor.evidence["native_shares_delta"] = json!("10");
    assert!(
        validate_vault_transition_rebase(
            &prepared,
            &verified,
            &anchor,
            &BigInt::from(9),
            &BigInt::from(109),
        )
        .unwrap()
    );
    // The native custody mint cannot stand in for the independently proven
    // integer farm entitlement, even though it came from this transaction.
    anchor.principal.atoms = Quantity::from_u64(110);
    assert!(
        validate_vault_transition_rebase(
            &prepared,
            &verified,
            &anchor,
            &BigInt::from(9),
            &BigInt::from(110),
        )
        .is_err()
    );
}

#[test]
fn projected_vault_backing_is_separate_from_shares_and_invalidated_by_changes() {
    let (_, _, anchor) = fixture();
    let snapshot = StateSnapshot {
        state: gofin_domain::FinancialState {
            positions: vec![gofin_domain::PositionState {
                resource: anchor.resource.clone(),
                position: anchor.position.clone(),
                kind: anchor.position_kind.clone(),
                amount: anchor.principal.clone(),
                scaled_receipt: None,
                anchor: Some(anchor.clone()),
            }],
            ..Default::default()
        },
        observed_at: now(),
        complete: true,
        coverage: vec![],
        pending: vec![],
    };
    let projected = projected_state(&snapshot).unwrap();
    assert!(projected.cash.is_empty());
    assert_eq!(
        projected.positions[0].amount.maximum,
        Quantity::from_u64(110)
    );
    assert_eq!(
        projected.positions[0]
            .vault_backing_valuation
            .as_ref()
            .unwrap()
            .total_usd_micros,
        Quantity::from_u64(123_000_000)
    );
    let updated = apply_projected_effects(
        &projected,
        &[ProjectedEffect::PositionDelta {
            resource: anchor.resource,
            position: anchor.position,
            asset: anchor.principal.asset,
            position_kind: anchor.position_kind,
            minimum_delta: SignedQuantity::new("-10").unwrap(),
            maximum_delta: SignedQuantity::new("-10").unwrap(),
        }],
        0,
    )
    .unwrap();
    assert_eq!(updated.positions[0].amount.maximum, Quantity::from_u64(100));
    assert!(updated.positions[0].vault_backing_valuation.is_none());
    let mut historical = serde_json::to_value(&projected).unwrap();
    historical["positions"][0]
        .as_object_mut()
        .unwrap()
        .remove("vault_backing_valuation");
    assert!(
        serde_json::from_value::<ProjectedState>(historical)
            .unwrap()
            .positions[0]
            .vault_backing_valuation
            .is_none()
    );
}
