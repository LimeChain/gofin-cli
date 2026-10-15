use super::*;

fn pnl_fixture(delta: &str, maximum_loss: &str) -> (OperationRecord, VerifiedEffects) {
    let wallet = resource("solana:mainnet-beta:wallet:test");
    let position = "perps-position-pda".to_owned();
    let notional = asset(
        "solana:mainnet-beta",
        "position-usd:So11111111111111111111111111111111111111112",
        6,
    );
    let pnl_asset = asset("solana:mainnet-beta", "jupiter:margin-usd", 6);
    let mut anchor = position_anchor(
        wallet.0.as_str(),
        &position,
        PositionKind::DerivativeLong,
        "5000000",
    );
    anchor.principal.asset = notional.clone();
    let expected = vec![
        EffectConstraint::DerivativePosition {
            resource: wallet.clone(),
            position: position.clone(),
            asset: notional.clone(),
            position_kind: PositionKind::DerivativeLong,
            minimum_delta: SignedQuantity::new("-5000000").unwrap(),
            maximum_delta: SignedQuantity::new("0").unwrap(),
        },
        EffectConstraint::PositionRealizedPnl {
            resource: wallet.clone(),
            position: position.clone(),
            position_kind: PositionKind::DerivativeLong,
            asset: pnl_asset.clone(),
            maximum_loss: quantity(maximum_loss),
        },
    ];
    let mut record = record(expected, vec![]);
    let effects = vec![
        FinancialEffect::PositionChange {
            resource: wallet.clone(),
            position: position.clone(),
            asset: notional,
            delta: SignedQuantity::new("-1000000").unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor.clone()),
        },
        FinancialEffect::PositionRealizedPnl {
            resource: wallet,
            position,
            position_kind: PositionKind::DerivativeLong,
            asset: pnl_asset,
            event_id: "keeper-signature:event-1".into(),
            delta: SignedQuantity::new(delta).unwrap(),
            anchor: Box::new(anchor),
        },
    ];
    let verified = proof(&record, effects);
    record.status = Status::Completed;
    (record, verified)
}

#[test]
fn position_realized_pnl_is_non_cash_and_binds_distinct_usd_unit_to_position_anchor() {
    let (record, verified) = pnl_fixture("250000", "5000000");
    validate_effects(&record, &verified, &Status::Completed).unwrap();
    assert!(
        actual_debits_and_costs(&verified.effects)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn position_realized_pnl_bounds_losses_and_preserves_proof_for_attention() {
    let (record, verified) = pnl_fixture("-5000001", "5000000");
    let error = validate_effects(&record, &verified, &Status::Completed).unwrap_err();
    assert_eq!(error.code, "EFFECT_EXCEEDS_RESERVATION");
}

#[test]
fn cumulative_position_realized_pnl_events_are_append_only_and_immutable() {
    let (record, first) = pnl_fixture("-200000", "5000000");
    let mut second = first.clone();
    second.effects.push(FinancialEffect::PositionRealizedPnl {
        resource: resource("solana:mainnet-beta:wallet:test"),
        position: "perps-position-pda".into(),
        position_kind: PositionKind::DerivativeLong,
        asset: asset("solana:mainnet-beta", "jupiter:margin-usd", 6),
        event_id: "keeper-signature:event-2".into(),
        delta: SignedQuantity::new("100000").unwrap(),
        anchor: match &first.effects[1] {
            FinancialEffect::PositionRealizedPnl { anchor, .. } => anchor.clone(),
            _ => unreachable!(),
        },
    });
    second.proof_digest = verified_effect_digest(&second).unwrap();
    validate_cumulative_settlement_components(Some(&first.effects), &second.effects).unwrap();

    let mut regressed = second.effects.clone();
    regressed.pop();
    assert!(validate_cumulative_settlement_components(Some(&second.effects), &regressed).is_err());
    let mut changed = second.effects.clone();
    if let FinancialEffect::PositionRealizedPnl { delta, .. } = &mut changed[1] {
        *delta = SignedQuantity::new("-300000").unwrap();
    }
    assert!(validate_cumulative_settlement_components(Some(&first.effects), &changed).is_err());
    let _ = record;
}
