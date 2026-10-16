use super::*;
use gofin_domain::Quantity;

fn asset(reference: &str, decimals: u8) -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: "solana:mainnet-beta".into(),
        reference: reference.into(),
        decimals,
    }
}

fn pair() -> (
    EffectConstraint,
    FinancialEffect,
    FinancialEffect,
    FinancialEffect,
) {
    let wallet = ResourceId("solana:mainnet-beta:wallet:owner".into());
    let escrow = ResourceId("solana:mainnet-beta:jupiter-request:req".into());
    let collateral = asset("jupiter:margin-usd", 6);
    let position = "request-pda:collateral".to_owned();
    let anchor = PositionAnchor {
        resource: wallet.clone(),
        position: position.clone(),
        position_kind: PositionKind::LockedCollateral,
        principal: Amount {
            asset: collateral.clone(),
            atoms: Quantity::new("1050").unwrap(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"finalized_position":"synthetic"}),
        observed_at: "2026-10-12T10:00:00Z".into(),
    };
    let exposure_asset = asset("jupiter:position-usd:market", 6);
    let exposure_position = "request-pda:exposure".to_owned();
    let exposure_anchor = PositionAnchor {
        resource: wallet.clone(),
        position: exposure_position.clone(),
        position_kind: PositionKind::DerivativeLong,
        principal: Amount {
            asset: exposure_asset.clone(),
            atoms: Quantity::new("2000").unwrap(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"finalized_position":"synthetic"}),
        observed_at: "2026-10-12T10:00:00Z".into(),
    };
    let constraint = EffectConstraint::EscrowToPosition {
        wallet_resource: wallet.clone(),
        escrow_resource: escrow.clone(),
        input_asset: asset("So11111111111111111111111111111111111111112", 9),
        position_resource: wallet.clone(),
        position: position.clone(),
        position_kind: PositionKind::LockedCollateral,
        position_asset: collateral.clone(),
        maximum_wallet_debit: Quantity::new("200").unwrap(),
        maximum_position_change: Quantity::new("100").unwrap(),
        minimum_position_per_input_numerator: Quantity::zero(),
        minimum_position_per_input_denominator: Quantity::new("1").unwrap(),
        conditional_position: Some(gofin_domain::EscrowConditionalPosition {
            resource: wallet.clone(),
            position: exposure_position.clone(),
            position_kind: PositionKind::DerivativeLong,
            asset: exposure_asset.clone(),
            maximum_change: Quantity::new("3000").unwrap(),
        }),
    };
    let progress = FinancialEffect::EscrowPositionProgress {
        wallet_resource: wallet.clone(),
        escrow_resource: escrow,
        input_asset: asset("So11111111111111111111111111111111111111112", 9),
        wallet_debit: Quantity::new("200").unwrap(),
        escrow_deposit: Quantity::new("200").unwrap(),
        input_transfer_fee: Quantity::zero(),
        input_filled: Quantity::new("100").unwrap(),
        input_refunded: Quantity::zero(),
        remaining_input: Quantity::new("100").unwrap(),
        position_resource: wallet.clone(),
        position: position.clone(),
        position_kind: PositionKind::LockedCollateral,
        position_asset: collateral.clone(),
        position_change: Quantity::new("50").unwrap(),
        state: gofin_domain::EscrowLifecycleState::Open,
        position_anchor: Some(anchor.clone()),
        conditional_position_change: Quantity::new("2000").unwrap(),
        conditional_position_anchor: Some(exposure_anchor.clone()),
    };
    let movement = FinancialEffect::PositionChange {
        resource: wallet.clone(),
        position,
        asset: collateral,
        delta: gofin_domain::SignedQuantity::new("50").unwrap(),
        scaled_receipt: None,
        anchor: Some(anchor),
    };
    let exposure_movement = FinancialEffect::PositionChange {
        resource: wallet,
        position: exposure_position,
        asset: exposure_asset,
        delta: gofin_domain::SignedQuantity::new("2000").unwrap(),
        scaled_receipt: None,
        anchor: Some(exposure_anchor),
    };
    (constraint, progress, movement, exposure_movement)
}

#[test]
fn escrow_position_progress_pairs_one_anchored_change_without_crediting_margin() {
    let (constraint, progress, movement, exposure_movement) = pair();
    let effects = vec![
        progress.clone(),
        movement.clone(),
        exposure_movement.clone(),
    ];
    assert!(escrow_position_progress_matches(
        &constraint,
        &progress,
        true
    ));
    validate_escrow_position_pairs(std::slice::from_ref(&constraint), &effects, true).unwrap();

    let delta = cumulative_boundary_delta(None, &VerifiedEffects {
        evidence_version: 1,
        submission: Submission {
            kind: "synthetic".into(),
            reference: "request-message".into(),
            data: json!({}),
        },
        effects,
        evidence: json!({"settlement_progress":{"version":1,"sequence":1,"snapshot_id":"fill-1","event_digests":{"fill-1":"digest"}}}),
        observed_at: "2026-10-12T10:00:00Z".into(),
        proof_digest: "unused-before-canonicalization".into(),
    }).unwrap();
    assert_eq!(
        delta
            .effects
            .iter()
            .filter(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
            .count(),
        0
    );
    assert!(delta.effects.iter().any(|effect| matches!(effect, FinancialEffect::CashDebit { amount, .. } if amount.atoms.as_str() == "200")));
    assert!(matches!(
        pair().2,
        FinancialEffect::PositionChange { delta, .. } if delta.magnitude().as_str() == "50"
    ));
}

#[test]
fn escrow_position_pair_rejects_divergent_delta_or_anchor() {
    let (constraint, progress, movement, exposure_movement) = pair();
    let mut wrong_delta = movement.clone();
    if let FinancialEffect::PositionChange { delta, .. } = &mut wrong_delta {
        *delta = gofin_domain::SignedQuantity::new("49").unwrap();
    }
    assert!(
        validate_escrow_position_pairs(
            std::slice::from_ref(&constraint),
            &[progress.clone(), wrong_delta, exposure_movement.clone()],
            true,
        )
        .is_err()
    );

    let mut wrong_anchor = movement;
    if let FinancialEffect::PositionChange {
        anchor: Some(anchor),
        ..
    } = &mut wrong_anchor
    {
        anchor.principal.atoms = Quantity::new("1051").unwrap();
    }
    assert!(
        validate_escrow_position_pairs(
            std::slice::from_ref(&constraint),
            &[progress, wrong_anchor, exposure_movement],
            true,
        )
        .is_err()
    );
}

#[test]
fn escrow_position_pair_rejects_tampered_conditional_exposure() {
    let (constraint, progress, movement, mut exposure_movement) = pair();
    if let FinancialEffect::PositionChange { delta, .. } = &mut exposure_movement {
        *delta = gofin_domain::SignedQuantity::new("1999").unwrap();
    }
    assert!(
        validate_escrow_position_pairs(
            std::slice::from_ref(&constraint),
            &[progress.clone(), movement.clone(), exposure_movement],
            true,
        )
        .is_err()
    );

    let mut missing_anchor = progress;
    if let FinancialEffect::EscrowPositionProgress {
        conditional_position_anchor,
        ..
    } = &mut missing_anchor
    {
        *conditional_position_anchor = None;
    }
    assert!(
        validate_escrow_position_pairs(
            std::slice::from_ref(&constraint),
            &[missing_anchor, movement, pair().3],
            true,
        )
        .is_err()
    );
}

#[test]
fn escrow_position_capture_covers_existing_collateral_and_exposure() {
    let (constraint, _, _, _) = pair();
    let intent = FinancialIntent {
        debits: vec![],
        maximum_costs: vec![],
        expected: vec![constraint],
        locks: vec![],
    };
    let requirements = position_anchor_requirements(&intent);
    assert_eq!(requirements.len(), 2);
    assert!(requirements.iter().any(|requirement| {
        requirement.position == "request-pda:collateral"
            && requirement.kind == Some(PositionKind::LockedCollateral)
            && requirement.asset.reference == "jupiter:margin-usd"
    }));
    assert!(requirements.iter().any(|requirement| {
        requirement.position == "request-pda:exposure"
            && requirement.kind == Some(PositionKind::DerivativeLong)
            && requirement.asset.reference == "jupiter:position-usd:market"
    }));
}

#[test]
fn conditional_exposure_pairing_is_scoped_to_its_exact_escrow() {
    let (first_constraint, first_progress, first_collateral, first_exposure) = pair();
    let mut second_constraint = first_constraint.clone();
    let mut second_progress = first_progress.clone();
    let mut second_collateral = first_collateral.clone();
    let mut second_exposure = first_exposure.clone();
    if let EffectConstraint::EscrowToPosition {
        escrow_resource,
        position,
        conditional_position: Some(conditional),
        ..
    } = &mut second_constraint
    {
        *escrow_resource = ResourceId("solana:mainnet-beta:jupiter-request:req-2".into());
        *position = "request-pda-2:collateral".into();
        conditional.position = "request-pda-2:exposure".into();
    }
    if let FinancialEffect::EscrowPositionProgress {
        escrow_resource,
        position,
        position_anchor: Some(position_anchor),
        conditional_position_anchor: Some(conditional_anchor),
        ..
    } = &mut second_progress
    {
        *escrow_resource = ResourceId("solana:mainnet-beta:jupiter-request:req-2".into());
        *position = "request-pda-2:collateral".into();
        position_anchor.position = "request-pda-2:collateral".into();
        conditional_anchor.position = "request-pda-2:exposure".into();
    }
    if let FinancialEffect::PositionChange {
        position,
        anchor: Some(anchor),
        ..
    } = &mut second_collateral
    {
        *position = "request-pda-2:collateral".into();
        anchor.position = "request-pda-2:collateral".into();
    }
    if let FinancialEffect::PositionChange {
        position,
        anchor: Some(anchor),
        ..
    } = &mut second_exposure
    {
        *position = "request-pda-2:exposure".into();
        anchor.position = "request-pda-2:exposure".into();
    }
    validate_escrow_position_pairs(
        &[first_constraint, second_constraint],
        &[
            first_progress,
            first_collateral,
            first_exposure,
            second_progress,
            second_collateral,
            second_exposure,
        ],
        true,
    )
    .unwrap();
}

#[test]
fn escrow_position_allows_zero_fill_open_and_no_fill_cancellation() {
    let (mut constraint, mut progress, _, _) = pair();
    if let EffectConstraint::EscrowToPosition {
        minimum_position_per_input_numerator,
        ..
    } = &mut constraint
    {
        *minimum_position_per_input_numerator = Quantity::new("9").unwrap();
    }
    let FinancialEffect::EscrowPositionProgress {
        input_filled,
        input_refunded,
        remaining_input,
        position_change,
        position_anchor,
        conditional_position_change,
        conditional_position_anchor,
        state,
        ..
    } = &mut progress
    else {
        unreachable!()
    };
    *input_filled = Quantity::zero();
    *input_refunded = Quantity::zero();
    *remaining_input = Quantity::new("200").unwrap();
    *position_change = Quantity::zero();
    *position_anchor = None;
    *conditional_position_change = Quantity::zero();
    *conditional_position_anchor = None;
    *state = gofin_domain::EscrowLifecycleState::Open;
    assert!(escrow_position_progress_matches(
        &constraint,
        &progress,
        true
    ));
    validate_escrow_position_pairs(std::slice::from_ref(&constraint), &[progress.clone()], true)
        .unwrap();

    if let FinancialEffect::EscrowPositionProgress {
        input_refunded,
        remaining_input,
        state,
        ..
    } = &mut progress
    {
        *input_refunded = Quantity::new("200").unwrap();
        *remaining_input = Quantity::zero();
        *state = gofin_domain::EscrowLifecycleState::Cancelled;
    }
    assert!(escrow_position_progress_matches(
        &constraint,
        &progress,
        false
    ));
    validate_escrow_position_pairs(std::slice::from_ref(&constraint), &[progress], false).unwrap();
}

#[test]
fn escrow_position_increase_preserves_existing_collateral_anchor() {
    let (_, _, movement, exposure_movement) = pair();
    let mut prior_anchor = match &movement {
        FinancialEffect::PositionChange {
            anchor: Some(anchor),
            ..
        } => anchor.clone(),
        _ => unreachable!(),
    };
    prior_anchor.principal.atoms = Quantity::new("1000").unwrap();
    let mut prior_exposure = match &exposure_movement {
        FinancialEffect::PositionChange {
            anchor: Some(anchor),
            ..
        } => anchor.clone(),
        _ => unreachable!(),
    };
    prior_exposure.principal.atoms = Quantity::new("100").unwrap();
    let mut exposure_movement = exposure_movement;
    if let FinancialEffect::PositionChange {
        anchor: Some(anchor),
        ..
    } = &mut exposure_movement
    {
        anchor.principal.atoms = Quantity::new("2100").unwrap();
    }
    let verified = VerifiedEffects {
        evidence_version: 1,
        submission: Submission {
            kind: "synthetic".into(),
            reference: "request-message".into(),
            data: json!({}),
        },
        effects: vec![movement, exposure_movement],
        evidence: json!({"finalized":true}),
        observed_at: "2026-10-12T10:00:00Z".into(),
        proof_digest: String::new(),
    };
    validate_anchor_transitions(
        &verified,
        &PreparationContext {
            logical_operation_id: None,
            positions: vec![prior_anchor, prior_exposure],
        },
    )
    .unwrap();
}
