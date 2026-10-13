use super::*;
use fin_domain::{AssetId, PendingRequestCreditBound, PendingRequestCreditProgress, Quantity};

fn fixture() -> (EffectConstraint, ResourceId, AssetId, AssetId) {
    let wallet = ResourceId("solana:mainnet-beta:wallet:synthetic".into());
    let request = ResourceId("solana:mainnet-beta:request:synthetic".into());
    let position_asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "jupiter:position-usd:synthetic-market".into(),
        decimals: 6,
    };
    let payout_asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "synthetic-usdc".into(),
        decimals: 6,
    };
    let constraint = EffectConstraint::PendingRequestLifecycle {
        wallet_resource: wallet.clone(),
        request_resource: request,
        request_id: "synthetic-request-pda".into(),
        request_kind: "take_profit".into(),
        conditional_positions: vec![fin_domain::PendingRequestPositionBound {
            resource: wallet.clone(),
            position: "synthetic-position-pda".into(),
            position_kind: PositionKind::DerivativeLong,
            asset: position_asset.clone(),
            minimum_delta: fin_domain::SignedQuantity::new("-1000").unwrap(),
            maximum_delta: fin_domain::SignedQuantity::new("0").unwrap(),
        }],
        conditional_credits: vec![PendingRequestCreditBound {
            asset: payout_asset.clone(),
            minimum_credit: Quantity::new("25").unwrap(),
            maximum_credit: Some(Quantity::new("100").unwrap()),
        }],
        conditional_pnl: vec![],
        protection: None,
    };
    (constraint, wallet, position_asset, payout_asset)
}

fn progress(
    state: fin_domain::PendingRequestState,
    wallet: &ResourceId,
    position_asset: &AssetId,
    payout_asset: &AssetId,
    position_delta: Option<&str>,
    payout: Option<&str>,
) -> Vec<FinancialEffect> {
    let position = "synthetic-position-pda".to_owned();
    let anchor = position_delta.map(|delta| PositionAnchor {
        resource: wallet.clone(),
        position: position.clone(),
        position_kind: PositionKind::DerivativeLong,
        principal: Amount {
            asset: position_asset.clone(),
            atoms: Quantity::new(delta.trim_start_matches('-')).unwrap(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"slot":99,"request":"synthetic-request-pda"}),
        observed_at: "2026-10-12T10:00:00Z".into(),
    });
    let mut effects = vec![FinancialEffect::PendingRequestProgress {
        wallet_resource: wallet.clone(),
        request_resource: ResourceId("solana:mainnet-beta:request:synthetic".into()),
        request_id: "synthetic-request-pda".into(),
        request_kind: "take_profit".into(),
        state,
        positions: position_delta
            .zip(anchor.as_ref())
            .map(|(delta, anchor)| {
                vec![fin_domain::PendingRequestPositionProgress {
                    resource: wallet.clone(),
                    position: position.clone(),
                    position_kind: PositionKind::DerivativeLong,
                    asset: position_asset.clone(),
                    delta: fin_domain::SignedQuantity::new(delta).unwrap(),
                    anchor: anchor.clone(),
                }]
            })
            .unwrap_or_default(),
        credits: payout
            .map(|amount| {
                vec![PendingRequestCreditProgress {
                    asset: payout_asset.clone(),
                    amount: Quantity::new(amount).unwrap(),
                }]
            })
            .unwrap_or_default(),
        realized_pnl: vec![],
        protection: None,
    }];
    if let (Some(delta), Some(anchor)) = (position_delta, anchor) {
        effects.push(FinancialEffect::PositionChange {
            resource: wallet.clone(),
            position,
            asset: position_asset.clone(),
            delta: fin_domain::SignedQuantity::new(delta).unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor),
        });
    }
    if let Some(amount) = payout {
        effects.push(FinancialEffect::UsableCredit {
            to: wallet.clone(),
            amount: Amount {
                asset: payout_asset.clone(),
                atoms: Quantity::new(amount).unwrap(),
            },
        });
    }
    effects
}

#[test]
fn pending_request_open_has_no_fabricated_payout_and_filled_payout_is_exact() {
    let (constraint, wallet, position_asset, payout_asset) = fixture();
    let open = progress(
        fin_domain::PendingRequestState::Open,
        &wallet,
        &position_asset,
        &payout_asset,
        None,
        None,
    );
    validate_pending_request_pairs(std::slice::from_ref(&constraint), &open, true).unwrap();

    let filled_without_credit = progress(
        fin_domain::PendingRequestState::Filled,
        &wallet,
        &position_asset,
        &payout_asset,
        Some("-50"),
        None,
    );
    assert!(
        validate_pending_request_pairs(
            std::slice::from_ref(&constraint),
            &filled_without_credit,
            false,
        )
        .is_err()
    );

    let filled = progress(
        fin_domain::PendingRequestState::Filled,
        &wallet,
        &position_asset,
        &payout_asset,
        Some("-50"),
        Some("25"),
    );
    validate_pending_request_pairs(std::slice::from_ref(&constraint), &filled, false).unwrap();
    assert!(
        filled
            .iter()
            .filter(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
            .all(|effect| pending_request_credit_is_paired(
                std::slice::from_ref(&constraint),
                &filled,
                effect,
            ))
    );
}

#[test]
fn pending_request_credit_rejects_recipient_asset_amount_and_duplicate_tampering() {
    let (constraint, wallet, position_asset, payout_asset) = fixture();
    for amount in ["24", "101"] {
        let effects = progress(
            fin_domain::PendingRequestState::Filled,
            &wallet,
            &position_asset,
            &payout_asset,
            Some("-50"),
            Some(amount),
        );
        assert!(
            validate_pending_request_pairs(std::slice::from_ref(&constraint), &effects, false,)
                .is_err()
        );
    }

    let mut wrong_recipient = progress(
        fin_domain::PendingRequestState::Filled,
        &wallet,
        &position_asset,
        &payout_asset,
        Some("-50"),
        Some("25"),
    );
    if let Some(FinancialEffect::UsableCredit { to, .. }) = wrong_recipient
        .iter_mut()
        .find(|effect| matches!(effect, FinancialEffect::UsableCredit { .. }))
    {
        *to = ResourceId("solana:mainnet-beta:wallet:attacker".into());
    }
    assert!(
        validate_pending_request_pairs(std::slice::from_ref(&constraint), &wrong_recipient, false,)
            .is_err()
    );

    let mut wrong_asset = progress(
        fin_domain::PendingRequestState::Filled,
        &wallet,
        &position_asset,
        &payout_asset,
        Some("-50"),
        Some("25"),
    );
    if let Some(FinancialEffect::PendingRequestProgress { credits, .. }) = wrong_asset
        .iter_mut()
        .find(|effect| matches!(effect, FinancialEffect::PendingRequestProgress { .. }))
    {
        credits[0].asset.reference = "wrong-token".into();
    }
    assert!(
        validate_pending_request_pairs(std::slice::from_ref(&constraint), &wrong_asset, false,)
            .is_err()
    );

    let mut duplicate = progress(
        fin_domain::PendingRequestState::Filled,
        &wallet,
        &position_asset,
        &payout_asset,
        Some("-50"),
        Some("25"),
    );
    duplicate.push(duplicate.last().unwrap().clone());
    assert!(
        validate_pending_request_pairs(std::slice::from_ref(&constraint), &duplicate, false,)
            .is_err()
    );
}

#[test]
fn pending_request_origin_movement_is_only_paired_provenance() {
    let wallet = ResourceId("solana:mainnet-beta:wallet:origin-test".into());
    let request = ResourceId("solana:mainnet-beta:request:origin-test".into());
    let collateral_asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "jupiter:margin-usd".into(),
        decimals: 6,
    };
    let payout_asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "synthetic-usdc".into(),
        decimals: 6,
    };
    let position = "request-collateral".to_owned();
    let anchor = PositionAnchor {
        resource: wallet.clone(),
        position: position.clone(),
        position_kind: PositionKind::LockedCollateral,
        principal: Amount {
            asset: collateral_asset.clone(),
            atoms: Quantity::new("75").unwrap(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"slot":101,"request":"origin-test"}),
        observed_at: "2026-10-12T10:00:00Z".into(),
    };
    let constraint = EffectConstraint::PendingRequestLifecycle {
        wallet_resource: wallet.clone(),
        request_resource: request.clone(),
        request_id: "origin-test".into(),
        request_kind: "conditional_close".into(),
        conditional_positions: vec![fin_domain::PendingRequestPositionBound {
            resource: wallet.clone(),
            position: position.clone(),
            position_kind: PositionKind::LockedCollateral,
            asset: collateral_asset.clone(),
            minimum_delta: fin_domain::SignedQuantity::new("-100").unwrap(),
            maximum_delta: fin_domain::SignedQuantity::new("0").unwrap(),
        }],
        conditional_credits: vec![PendingRequestCreditBound {
            asset: payout_asset.clone(),
            minimum_credit: Quantity::new("10").unwrap(),
            maximum_credit: Some(Quantity::new("100").unwrap()),
        }],
        conditional_pnl: vec![],
        protection: None,
    };
    let progress = FinancialEffect::PendingRequestProgress {
        wallet_resource: wallet.clone(),
        request_resource: request,
        request_id: "origin-test".into(),
        request_kind: "conditional_close".into(),
        state: fin_domain::PendingRequestState::Filled,
        positions: vec![fin_domain::PendingRequestPositionProgress {
            resource: wallet.clone(),
            position: position.clone(),
            position_kind: PositionKind::LockedCollateral,
            asset: collateral_asset.clone(),
            delta: fin_domain::SignedQuantity::new("-25").unwrap(),
            anchor: anchor.clone(),
        }],
        credits: vec![PendingRequestCreditProgress {
            asset: payout_asset.clone(),
            amount: Quantity::new("80").unwrap(),
        }],
        realized_pnl: vec![],
        protection: None,
    };
    let origin = FinancialEffect::OriginMovement {
        from: AccountingLocation::Position {
            resource: wallet.clone(),
            position: position.clone(),
            position_kind: PositionKind::LockedCollateral,
        },
        to: AccountingLocation::Cash {
            resource: wallet.clone(),
        },
        debit: Amount {
            asset: collateral_asset.clone(),
            atoms: Quantity::new("25").unwrap(),
        },
        credit: Amount {
            asset: payout_asset.clone(),
            atoms: Quantity::new("80").unwrap(),
        },
    };
    let mut effects = vec![
        progress,
        FinancialEffect::PositionChange {
            resource: wallet.clone(),
            position,
            asset: collateral_asset,
            delta: fin_domain::SignedQuantity::new("-25").unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor),
        },
        FinancialEffect::UsableCredit {
            to: wallet,
            amount: Amount {
                asset: payout_asset,
                atoms: Quantity::new("80").unwrap(),
            },
        },
        origin.clone(),
    ];

    validate_pending_request_pairs(std::slice::from_ref(&constraint), &effects, false).unwrap();
    assert!(pending_request_origin_movement_is_paired(
        std::slice::from_ref(&constraint),
        &effects,
        &origin,
    ));

    effects.push(origin.clone());
    assert!(!pending_request_origin_movement_is_paired(
        std::slice::from_ref(&constraint),
        &effects,
        &origin,
    ));
    effects.pop();

    let mut mismatched = origin.clone();
    if let FinancialEffect::OriginMovement { debit, .. } = &mut mismatched {
        debit.atoms = Quantity::new("26").unwrap();
    }
    assert!(!pending_request_origin_movement_is_paired(
        std::slice::from_ref(&constraint),
        &effects,
        &mismatched,
    ));

    let mut wrong_request = effects.clone();
    if let Some(FinancialEffect::PendingRequestProgress { request_id, .. }) = wrong_request
        .iter_mut()
        .find(|effect| matches!(effect, FinancialEffect::PendingRequestProgress { .. }))
    {
        *request_id = "different-request".into();
    }
    assert!(!pending_request_origin_movement_is_paired(
        std::slice::from_ref(&constraint),
        &wrong_request,
        &origin,
    ));
}

#[test]
fn pending_short_protection_reduces_with_negative_position_delta() {
    let wallet = ResourceId("solana:mainnet-beta:wallet:short-protection".into());
    let position_asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "jupiter:position-usd:btc".into(),
        decimals: 6,
    };
    let anchor = PositionAnchor {
        resource: wallet.clone(),
        position: "short-position-pda".into(),
        position_kind: PositionKind::DerivativeShort,
        principal: Amount {
            asset: position_asset.clone(),
            atoms: Quantity::new("1000").unwrap(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"slot":100}),
        observed_at: "2026-10-12T10:00:00Z".into(),
    };
    let protection = fin_domain::PendingRequestProtection {
        position_anchor: anchor.clone(),
        position_lock_resource: ResourceId("position:short-position-pda".into()),
        maximum_reduction: Quantity::new("1000").unwrap(),
        trigger_direction: fin_domain::PendingRequestTriggerDirection::Above,
        trigger_price: Amount {
            asset: AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "jupiter:margin-usd".into(),
                decimals: 6,
            },
            atoms: Quantity::new("50000").unwrap(),
        },
        reduce_only: true,
    };
    let mut sibling = protection.clone();
    sibling.trigger_direction = fin_domain::PendingRequestTriggerDirection::Below;
    sibling.trigger_price.atoms = Quantity::new("45000").unwrap();
    sibling.position_anchor.observed_at = "2026-10-12T10:01:00Z".into();
    sibling.position_anchor.evidence = json!({"slot":101});
    assert!(protection.compatible_sibling(&sibling));
    sibling.trigger_direction = protection.trigger_direction;
    assert!(!protection.compatible_sibling(&sibling));
    sibling.trigger_direction = fin_domain::PendingRequestTriggerDirection::Below;
    sibling.position_anchor.principal.atoms = Quantity::new("1001").unwrap();
    assert!(!protection.compatible_sibling(&sibling));
    let bounds = vec![fin_domain::PendingRequestPositionBound {
        resource: wallet.clone(),
        position: anchor.position.clone(),
        position_kind: PositionKind::DerivativeShort,
        asset: position_asset.clone(),
        minimum_delta: fin_domain::SignedQuantity::new("-1000").unwrap(),
        maximum_delta: fin_domain::SignedQuantity::new("0").unwrap(),
    }];
    let make_progress = |delta: &str| {
        let remaining = if delta.starts_with('-') {
            Quantity::zero()
        } else {
            Quantity::new("2000").unwrap()
        };
        vec![fin_domain::PendingRequestPositionProgress {
            resource: wallet.clone(),
            position: anchor.position.clone(),
            position_kind: PositionKind::DerivativeShort,
            asset: position_asset.clone(),
            delta: fin_domain::SignedQuantity::new(delta).unwrap(),
            anchor: PositionAnchor {
                principal: Amount {
                    asset: position_asset.clone(),
                    atoms: remaining,
                },
                ..anchor.clone()
            },
        }]
    };

    assert!(protection_request_progress_valid(
        &protection,
        &bounds,
        fin_domain::PendingRequestState::Filled,
        &make_progress("-1000"),
    ));
    assert!(!protection_request_progress_valid(
        &protection,
        &bounds,
        fin_domain::PendingRequestState::Filled,
        &make_progress("1000"),
    ));
}

#[test]
fn pending_request_cumulative_payout_cannot_regress_after_restart() {
    let (constraint, wallet, position_asset, payout_asset) = fixture();
    let previous = progress(
        fin_domain::PendingRequestState::Open,
        &wallet,
        &position_asset,
        &payout_asset,
        Some("-10"),
        Some("25"),
    );
    let current = progress(
        fin_domain::PendingRequestState::Open,
        &wallet,
        &position_asset,
        &payout_asset,
        Some("-20"),
        Some("24"),
    );
    validate_pending_request_pairs(std::slice::from_ref(&constraint), &previous, true).unwrap();
    assert!(validate_cumulative_settlement_components(Some(&previous), &current).is_err());
}
