use super::*;
use gofin_domain::{
    AssetId, LendingPositionEvidence, LendingTransitionBinding, LiquidityCompositionAmount,
    Quantity, SignedQuantity, VaultBackingValuation,
};

fn fixture(delta: i64) -> (PreparedOperation, VerifiedEffects, PreparationContext) {
    let resource = ResourceId("solana:mainnet-beta:owner".into());
    let asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "collateral-mint".into(),
        decimals: 6,
    };
    let underlying = AssetId {
        reference: "underlying-mint".into(),
        ..asset.clone()
    };
    let position = "kamino:lending:market:obligation:reserve:supply".to_owned();
    let submission = Submission {
        kind: "solana.transaction".into(),
        reference: "finalized-signature".into(),
        data: json!({"message_sha256":"original-message"}),
    };
    let post = Quantity::new((100 + delta).to_string()).unwrap();
    let delta = SignedQuantity::new(delta.to_string()).unwrap();
    let evidence = LendingPositionEvidence {
        version: 1,
        market: "market".into(),
        obligation: "obligation".into(),
        reserve: "reserve".into(),
        owner: "owner".into(),
        collateral_asset: asset.clone(),
        collateral_token_program: "token-program".into(),
        collateral_atoms: post.clone(),
        underlying: LiquidityCompositionAmount {
            asset: underlying,
            token_program: "token-program".into(),
            atoms: Some(Quantity::from_u64(140)),
        },
        observed_slot: 42,
        observed_at: now(),
        historical_principal_known: false,
        backing_valuation: Some(VaultBackingValuation {
            total_usd_micros: Quantity::from_u64(140_000_000),
            observed_at: now(),
            source: "synthetic underlying price".into(),
            complete: true,
            basis: "verified_underlying_exposure".into(),
            excluded_components: vec![],
        }),
        pre_transaction_collateral_atoms: Some(Quantity::from_u64(100)),
        finalized_submission: Some(submission.clone()),
        collateral_delta_atoms: Some(delta.clone()),
    };
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: position.clone(),
        position_kind: PositionKind::Supply,
        principal: Amount {
            asset: asset.clone(),
            atoms: post,
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"lending_position":evidence}),
        observed_at: now(),
    };
    let binding = LendingTransitionBinding {
        version: 1,
        market: "market".into(),
        obligation: "obligation".into(),
        reserve: "reserve".into(),
        owner: "owner".into(),
        collateral_token_program: "token-program".into(),
        resource: resource.clone(),
        position: position.clone(),
        asset: asset.clone(),
        pre_transaction_collateral_atoms: Quantity::from_u64(100),
    };
    let prepared = PreparedOperation {
        operation: "kamino.lending.supply".into(),
        input: json!({}),
        binding: json!({gofin_domain::LENDING_TRANSITION_BINDING_KEY:binding}),
        summary: "synthetic imported collateral".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: now(),
        expires_at: now(),
        valuation: None,
        intent: FinancialIntent::default(),
        action_deadline: None,
        operation_version: 1,
    };
    let mut prior = anchor.clone();
    prior.principal.atoms = Quantity::from_u64(50);
    prior.evidence = json!({"synthetic_prior_balance":true});
    let verified = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects: vec![FinancialEffect::PositionChange {
            resource,
            position,
            asset,
            delta,
            scaled_receipt: None,
            anchor: Some(anchor),
        }],
        evidence: json!({"synthetic_native_finalization":true}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    (
        prepared,
        verified,
        PreparationContext {
            positions: vec![prior],
            ..Default::default()
        },
    )
}

fn anchor_mut(verified: &mut VerifiedEffects) -> &mut PositionAnchor {
    match &mut verified.effects[0] {
        FinancialEffect::PositionChange { anchor, .. } => anchor.as_mut().unwrap(),
        _ => unreachable!(),
    }
}

#[test]
fn imported_collateral_rebases_current_units_without_yield_or_historical_basis() {
    for delta in [10, -10] {
        let (prepared, verified, prior) = fixture(delta);
        validate_anchor_transitions_with_candidate(&verified, &prior, Some(&prepared)).unwrap();
        assert_eq!(verified.effects.len(), 1, "no fabricated accrual or cash");
        let persisted: VerifiedEffects =
            serde_json::from_value(serde_json::to_value(&verified).unwrap()).unwrap();
        validate_anchor_transitions_with_candidate(&persisted, &prior, Some(&prepared)).unwrap();
        assert!(validate_anchor_transitions(&verified, &prior).is_err());
    }
}

#[test]
fn collateral_rebase_rejects_forged_or_mismatched_finalization() {
    let (prepared, verified, prior) = fixture(10);
    for (field, changed) in [
        ("version", json!(2)),
        ("market", json!("other-market")),
        ("obligation", json!("other-obligation")),
        ("reserve", json!("other-reserve")),
        ("owner", json!("other-owner")),
        ("collateral_token_program", json!("other-program")),
        ("collateral_atoms", json!("111")),
        ("pre_transaction_collateral_atoms", json!("99")),
        ("collateral_delta_atoms", json!("11")),
        ("finalized_submission", json!(null)),
        ("historical_principal_known", json!(true)),
    ] {
        let mut corrupt = verified.clone();
        anchor_mut(&mut corrupt).evidence["lending_position"][field] = changed;
        assert!(
            validate_anchor_transitions_with_candidate(&corrupt, &prior, Some(&prepared)).is_err(),
            "field {field}"
        );
    }
    for field in ["reference", "data"] {
        let mut corrupt = verified.clone();
        anchor_mut(&mut corrupt).evidence["lending_position"]["finalized_submission"][field] =
            json!("other-submission");
        assert!(
            validate_anchor_transitions_with_candidate(&corrupt, &prior, Some(&prepared)).is_err()
        );
    }
    for field in ["network", "reference", "decimals"] {
        let mut corrupt = verified.clone();
        anchor_mut(&mut corrupt).evidence["lending_position"]["collateral_asset"][field] =
            if field == "decimals" {
                json!(9)
            } else {
                json!("other-asset")
            };
        assert!(
            validate_anchor_transitions_with_candidate(&corrupt, &prior, Some(&prepared)).is_err()
        );
    }
    let mut corrupt = verified.clone();
    anchor_mut(&mut corrupt).position_kind = PositionKind::VariableDebt;
    assert!(validate_anchor_transitions_with_candidate(&corrupt, &prior, Some(&prepared)).is_err());
    let mut corrupt = verified.clone();
    anchor_mut(&mut corrupt).resource = ResourceId("foreign-wallet".into());
    assert!(validate_anchor_transitions_with_candidate(&corrupt, &prior, Some(&prepared)).is_err());
    let mut corrupt = prepared;
    corrupt.binding[gofin_domain::LENDING_TRANSITION_BINDING_KEY]["pre_transaction_collateral_atoms"] =
        json!("99");
    assert!(validate_anchor_transitions_with_candidate(&verified, &prior, Some(&corrupt)).is_err());
}

#[test]
fn external_collateral_movement_cannot_be_relabelled_as_accrual() {
    let (prepared, mut verified, prior) = fixture(10);
    let anchor = anchor_mut(&mut verified).clone();
    if let FinancialEffect::PositionChange { delta, .. } = &mut verified.effects[0] {
        *delta = SignedQuantity::new("9").unwrap();
    }
    verified.effects.push(FinancialEffect::Accrual {
        resource: anchor.resource,
        position: anchor.position,
        asset: anchor.principal.asset,
        position_kind: PositionKind::Supply,
        delta: SignedQuantity::new("1").unwrap(),
    });
    // Although pre + (principal + purported yield) equals post, receipt units
    // have no exchange-rate accrual and the native principal delta was ten.
    assert!(
        validate_anchor_transitions_with_candidate(&verified, &prior, Some(&prepared)).is_err()
    );
}

#[test]
fn projected_collateral_preserves_units_and_separate_current_backing() {
    let (_, mut verified, _) = fixture(10);
    let anchor = anchor_mut(&mut verified).clone();
    let snapshot = StateSnapshot {
        state: gofin_domain::FinancialState {
            positions: vec![gofin_domain::PositionState {
                resource: anchor.resource.clone(),
                position: anchor.position.clone(),
                kind: PositionKind::Supply,
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
            .lending_backing_valuation
            .as_ref()
            .unwrap()
            .total_usd_micros,
        Quantity::from_u64(140_000_000)
    );
    assert!(projected.positions[0].vault_backing_valuation.is_none());
    let changed = apply_projected_effects(
        &projected,
        &[ProjectedEffect::PositionDelta {
            resource: anchor.resource,
            position: anchor.position,
            asset: anchor.principal.asset,
            position_kind: PositionKind::Supply,
            minimum_delta: SignedQuantity::new("-10").unwrap(),
            maximum_delta: SignedQuantity::new("-10").unwrap(),
        }],
        0,
    )
    .unwrap();
    assert_eq!(changed.positions[0].amount.maximum, Quantity::from_u64(100));
    assert!(changed.positions[0].lending_backing_valuation.is_none());
}

#[test]
fn borrowing_fee_is_included_once_in_gross_debt_to_net_cash() {
    let resource = ResourceId("solana:mainnet-beta:owner".into());
    let asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "underlying-mint".into(),
        decimals: 6,
    };
    let debt = AccountingLocation::Position {
        resource: resource.clone(),
        position: "exact-obligation-reserve-debt".into(),
        position_kind: PositionKind::VariableDebt,
    };
    let cash = AccountingLocation::Cash {
        resource: resource.clone(),
    };
    let amount = |n| Amount {
        asset: asset.clone(),
        atoms: Quantity::from_u64(n),
    };
    let cost = CostBound {
        location: debt.clone(),
        amount: amount(2),
        treatment: CostTreatment::IncludedInDebit,
    };
    let mut record = tests::record(
        vec![
            EffectConstraint::Debt {
                resource: resource.clone(),
                debt: "exact-obligation-reserve-debt".into(),
                asset: asset.clone(),
                direction: Direction::Increase,
                maximum_change: Quantity::from_u64(100),
            },
            EffectConstraint::Credit {
                to: resource.clone(),
                asset: asset.clone(),
                minimum_credit: Quantity::from_u64(98),
            },
            EffectConstraint::OriginMovement {
                from: debt.clone(),
                to: cash.clone(),
                debit_asset: asset.clone(),
                credit_asset: asset.clone(),
                maximum_debit: Quantity::from_u64(100),
                minimum_credit: Quantity::from_u64(98),
            },
        ],
        vec![cost],
    );
    let prepared = record.preview.as_mut().unwrap();
    prepared.intent.debits.clear();
    record.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(prepared)).unwrap();
    validate_cost_bound_contract(&prepared.intent).unwrap();
    assert!(
        cash_reservation_amounts(&prepared.intent.debits, &prepared.intent.maximum_costs)
            .unwrap()
            .is_empty()
    );
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: record.submission.clone().unwrap(),
        effects: vec![
            FinancialEffect::DebtChange {
                resource: resource.clone(),
                debt: "exact-obligation-reserve-debt".into(),
                asset: asset.clone(),
                delta: SignedQuantity::new("100").unwrap(),
                anchor: Some(PositionAnchor {
                    resource: resource.clone(),
                    position: "exact-obligation-reserve-debt".into(),
                    position_kind: PositionKind::VariableDebt,
                    principal: amount(100),
                    scaled_receipt: None,
                    evidence_version: 1,
                    evidence: json!({"synthetic_debt_evidence":true}),
                    observed_at: now(),
                }),
            },
            FinancialEffect::UsableCredit {
                to: resource,
                amount: amount(98),
            },
            FinancialEffect::OriginMovement {
                from: debt.clone(),
                to: cash,
                debit: amount(100),
                credit: amount(98),
            },
            FinancialEffect::Cost {
                location: debt,
                amount: amount(2),
                category: "origination_fee".into(),
                treatment: CostTreatment::IncludedInDebit,
            },
        ],
        evidence: json!({"synthetic_verified_gross_net_fee":true}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    verified.proof_digest = verified_effect_digest(&verified).unwrap();
    validate_effects(&record, &verified, &Status::Completed).unwrap();
    validate_origin_links(&verified.effects).unwrap();
    validate_anchor_transitions(&verified, &PreparationContext::default()).unwrap();
    let cost = verified.effects.last().unwrap().clone();
    verified.effects.push(cost);
    verified.proof_digest = verified_effect_digest(&verified).unwrap();
    assert!(
        validate_effects(&record, &verified, &Status::Completed).is_err(),
        "fee cannot be booked twice"
    );
}

#[test]
fn zero_debt_principal_observation_requires_exact_zero_scope_and_typed_anchor() {
    let resource = ResourceId("solana:mainnet-beta:owner".into());
    let asset = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "other-reserve-debt-mint".into(),
        decimals: 6,
    };
    let position = "other-reserve-debt".to_owned();
    let mut record = tests::record(
        vec![
            EffectConstraint::Debt {
                resource: resource.clone(),
                debt: position.clone(),
                asset: asset.clone(),
                direction: Direction::Decrease,
                maximum_change: Quantity::zero(),
            },
            EffectConstraint::Accrual {
                resource: resource.clone(),
                position: position.clone(),
                asset: asset.clone(),
                position_kind: PositionKind::VariableDebt,
                maximum_change: Quantity::from_u64(5),
            },
        ],
        vec![],
    );
    record.preview.as_mut().unwrap().intent.debits.clear();
    record.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(record.preview.as_ref().unwrap())).unwrap();
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: position.clone(),
        position_kind: PositionKind::VariableDebt,
        principal: Amount {
            asset: asset.clone(),
            atoms: Quantity::from_u64(105),
        },
        scaled_receipt: Some(Quantity::from_u64(105)),
        evidence_version: 1,
        evidence: json!({"native_debt_index_and_fraction":"synthetic"}),
        observed_at: now(),
    };
    let mut prior = anchor.clone();
    prior.principal.atoms = Quantity::from_u64(100);
    let prior = PreparationContext {
        positions: vec![prior],
        ..Default::default()
    };
    let mut verified = VerifiedEffects {
        evidence_version: 1,
        submission: record.submission.clone().unwrap(),
        effects: vec![
            FinancialEffect::Accrual {
                resource: resource.clone(),
                position: position.clone(),
                asset: asset.clone(),
                position_kind: PositionKind::VariableDebt,
                delta: SignedQuantity::new("5").unwrap(),
            },
            FinancialEffect::DebtChange {
                resource,
                debt: position,
                asset,
                delta: SignedQuantity::new("0").unwrap(),
                anchor: Some(anchor),
            },
        ],
        evidence: json!({"synthetic_refreshed_debt_observation":true}),
        observed_at: now(),
        proof_digest: String::new(),
    };
    verified.proof_digest = verified_effect_digest(&verified).unwrap();
    validate_effects(&record, &verified, &Status::Completed).unwrap();
    validate_origin_links(&verified.effects).unwrap();
    validate_anchor_transitions(&verified, &prior).unwrap();
    for case in 0..9 {
        let mut record = record.clone();
        let mut corrupt = verified.clone();
        let FinancialEffect::DebtChange { anchor, delta, .. } = &mut corrupt.effects[1] else {
            unreachable!()
        };
        match case {
            0 => *anchor = None,
            1 => anchor.as_mut().unwrap().position_kind = PositionKind::Supply,
            2 => anchor.as_mut().unwrap().resource = ResourceId("foreign-owner".into()),
            3 => anchor.as_mut().unwrap().position = "foreign-reserve".into(),
            4 => anchor.as_mut().unwrap().principal.asset.decimals = 9,
            5 => anchor.as_mut().unwrap().evidence_version = 0,
            6 => *delta = SignedQuantity::new("1").unwrap(),
            7 => *delta = SignedQuantity::new("-1").unwrap(),
            8 => {
                let EffectConstraint::Debt { maximum_change, .. } =
                    &mut record.preview.as_mut().unwrap().intent.expected[0]
                else {
                    unreachable!()
                };
                *maximum_change = Quantity::from_u64(1);
                record.approval.as_mut().unwrap().binding_hash =
                    binding_hash(&authorization_binding(record.preview.as_ref().unwrap())).unwrap();
            }
            _ => unreachable!(),
        }
        corrupt.proof_digest = verified_effect_digest(&corrupt).unwrap();
        assert!(
            validate_effects(&record, &corrupt, &Status::Completed).is_err(),
            "case {case}"
        );
    }
    let FinancialEffect::DebtChange { anchor, .. } = &mut verified.effects[1] else {
        unreachable!()
    };
    anchor.as_mut().unwrap().principal.atoms = Quantity::from_u64(106);
    assert!(
        validate_anchor_transitions(&verified, &prior).is_err(),
        "zero principal cannot erase an unexplained debt change"
    );
}
