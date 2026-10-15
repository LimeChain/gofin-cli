use super::*;

fn usd_asset() -> gofin_domain::AssetId {
    asset("solana:mainnet-beta", "jupiter:margin-usd", 6)
}

fn cash_fixture() -> (AccountingLocation, Vec<FinancialEffect>) {
    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let location = AccountingLocation::Position {
        resource: wallet.clone(),
        position: "perps:request:collateral".into(),
        position_kind: PositionKind::LockedCollateral,
    };
    let input = asset("solana:mainnet-beta", "USDC", 6);
    let margin = usd_asset();
    let gross_debit = Amount {
        asset: input,
        atoms: quantity("11000000"),
    };
    let gross_converted = Amount {
        asset: margin.clone(),
        atoms: quantity("11000000"),
    };
    let net_position = Amount {
        asset: margin.clone(),
        atoms: quantity("10999000"),
    };
    let included_cost = Amount {
        asset: margin.clone(),
        atoms: quantity("1000"),
    };
    let anchor = PositionAnchor {
        resource: wallet.clone(),
        position: "perps:request:collateral".into(),
        position_kind: PositionKind::LockedCollateral,
        principal: net_position.clone(),
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"event":"synthetic finalized collateral delta"}),
        observed_at: "2026-01-01T00:00:30Z".into(),
    };
    let effects = vec![
        FinancialEffect::CashDebit {
            resource: wallet.clone(),
            amount: gross_debit.clone(),
        },
        FinancialEffect::OriginMovement {
            from: AccountingLocation::Cash {
                resource: wallet.clone(),
            },
            to: location.clone(),
            debit: gross_debit.clone(),
            credit: net_position.clone(),
        },
        FinancialEffect::PositionChange {
            resource: wallet.clone(),
            position: "perps:request:collateral".into(),
            asset: margin.clone(),
            delta: SignedQuantity::new("10999000").unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor),
        },
        FinancialEffect::Cost {
            location: location.clone(),
            amount: included_cost.clone(),
            category: "protocol_fee".into(),
            treatment: CostTreatment::IncludedInDebit,
        },
        FinancialEffect::IncludedCostConversion {
            location: location.clone(),
            source: gofin_domain::IncludedCostSource::Cash { resource: wallet },
            gross_debit,
            gross_converted,
            net_position,
            included_cost,
            evidence_id: "synthetic-finalized-event-1".into(),
        },
    ];
    (location, effects)
}

fn escrow_fixture() -> (AccountingLocation, Vec<FinancialEffect>) {
    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let escrow = resource("solana:mainnet-beta:request-ata");
    let location = AccountingLocation::Position {
        resource: wallet.clone(),
        position: "perps:request:collateral".into(),
        position_kind: PositionKind::LockedCollateral,
    };
    let input = asset("solana:mainnet-beta", "USDC", 6);
    let margin = usd_asset();
    let net = quantity("999000");
    let fee = quantity("1000");
    let anchor = PositionAnchor {
        resource: wallet.clone(),
        position: "perps:request:collateral".into(),
        position_kind: PositionKind::LockedCollateral,
        principal: Amount {
            asset: margin.clone(),
            atoms: net.clone(),
        },
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"event":"synthetic finalized fill"}),
        observed_at: "2026-01-01T00:00:30Z".into(),
    };
    let effects = vec![
        FinancialEffect::EscrowPositionProgress {
            wallet_resource: wallet.clone(),
            escrow_resource: escrow.clone(),
            input_asset: input.clone(),
            wallet_debit: quantity("2000000"),
            escrow_deposit: quantity("2000000"),
            input_transfer_fee: Quantity::zero(),
            input_filled: quantity("1000000"),
            input_refunded: Quantity::zero(),
            remaining_input: quantity("1000000"),
            position_resource: wallet.clone(),
            position: "perps:request:collateral".into(),
            position_kind: PositionKind::LockedCollateral,
            position_asset: margin.clone(),
            position_change: net.clone(),
            state: gofin_domain::EscrowLifecycleState::Open,
            position_anchor: Some(anchor.clone()),
            conditional_position_change: Quantity::zero(),
            conditional_position_anchor: None,
        },
        FinancialEffect::PositionChange {
            resource: wallet.clone(),
            position: "perps:request:collateral".into(),
            asset: margin.clone(),
            delta: SignedQuantity::new("999000").unwrap(),
            scaled_receipt: None,
            anchor: Some(anchor),
        },
        FinancialEffect::Cost {
            location: location.clone(),
            amount: Amount {
                asset: margin.clone(),
                atoms: fee.clone(),
            },
            category: "protocol_fee".into(),
            treatment: CostTreatment::IncludedInDebit,
        },
        FinancialEffect::IncludedCostConversion {
            location: location.clone(),
            source: gofin_domain::IncludedCostSource::Escrow {
                wallet_resource: wallet,
                escrow_resource: escrow,
            },
            gross_debit: Amount {
                asset: input,
                atoms: quantity("1000000"),
            },
            gross_converted: Amount {
                asset: margin.clone(),
                atoms: quantity("1000000"),
            },
            net_position: Amount {
                asset: margin.clone(),
                atoms: net,
            },
            included_cost: Amount {
                asset: margin,
                atoms: fee,
            },
            evidence_id: "synthetic-finalized-fill-1".into(),
        },
    ];
    (location, effects)
}

#[test]
fn cash_conversion_binds_cross_unit_gross_net_cost_and_anchor() {
    let (location, mut effects) = cash_fixture();
    let fee = quantity("1000").integer();
    assert!(included_cost_conversion_is_paired(&location, &usd_asset(), &fee, &effects).unwrap());

    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let funding = asset("solana:mainnet-beta", "USDC", 6);
    let margin = usd_asset();
    let fee_token = Amount {
        asset: funding.clone(),
        atoms: quantity("1000"),
    };
    effects.push(FinancialEffect::CostDenomination {
        location: location.clone(),
        cost_asset: margin.clone(),
        category: "protocol_fee".into(),
        treatment: CostTreatment::IncludedInDebit,
        denomination: fee_token.clone(),
        evidence_id: "synthetic-finalized-event-1".into(),
    });
    let approved = vec![
        EffectConstraint::Debit {
            resource: wallet.clone(),
            asset: funding.clone(),
            maximum_debit: quantity("11000000"),
        },
        EffectConstraint::OriginMovement {
            from: AccountingLocation::Cash {
                resource: wallet.clone(),
            },
            to: location.clone(),
            debit_asset: funding.clone(),
            credit_asset: margin.clone(),
            maximum_debit: quantity("11000000"),
            minimum_credit: quantity("10999000"),
        },
        EffectConstraint::PositionOfKind {
            resource: wallet,
            position: "perps:request:collateral".into(),
            asset: margin.clone(),
            position_kind: PositionKind::LockedCollateral,
            direction: Direction::Increase,
            maximum_change: quantity("10999000"),
        },
        EffectConstraint::CostDenomination {
            location: location.clone(),
            cost_asset: margin.clone(),
            category: "protocol_fee".into(),
            treatment: CostTreatment::IncludedInDebit,
            denomination_asset: funding,
            maximum_amount: quantity("1000"),
        },
    ];
    let mut record = record(
        approved,
        vec![CostBound {
            location: location.clone(),
            amount: Amount {
                asset: margin,
                atoms: quantity("1000"),
            },
            treatment: CostTreatment::IncludedInDebit,
        }],
    );
    record.preview.as_mut().unwrap().intent.debits = vec![ResourceAmount {
        resource: resource("solana:mainnet-beta:wallet:owner"),
        amount: Amount {
            asset: asset("solana:mainnet-beta", "USDC", 6),
            atoms: quantity("11000000"),
        },
    }];
    let prepared = record.preview.as_ref().unwrap();
    record.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(prepared)).unwrap();
    let intent = &record.preview.as_ref().unwrap().intent;
    validate_cost_bound_contract(intent).unwrap();
    let verified = proof(&record, effects.clone());
    assert_eq!(
        validate_effects(&record, &verified, &Status::Completed),
        Ok(())
    );

    let mut over_cap_denomination = effects.clone();
    let Some(FinancialEffect::CostDenomination { denomination, .. }) =
        over_cap_denomination.last_mut()
    else {
        panic!("denomination evidence missing");
    };
    denomination.atoms = quantity("1001");
    let over_cap_proof = proof(&record, over_cap_denomination);
    assert_eq!(
        validate_effects(&record, &over_cap_proof, &Status::Completed)
            .unwrap_err()
            .code,
        "EFFECT_EXCEEDS_RESERVATION"
    );

    let mut wrong_event = effects.clone();
    let Some(FinancialEffect::CostDenomination { evidence_id, .. }) = wrong_event.last_mut() else {
        panic!("denomination evidence missing");
    };
    *evidence_id = "other-finalized-event".into();
    assert_eq!(
        validate_effects(&record, &proof(&record, wrong_event), &Status::Completed)
            .unwrap_err()
            .code,
        "COST_DENOMINATION_MISMATCH"
    );

    let mut malformed = effects.clone();
    let Some(FinancialEffect::IncludedCostConversion {
        gross_converted, ..
    }) = malformed.get_mut(4)
    else {
        panic!("fixture marker missing");
    };
    gross_converted.atoms = quantity("11000001");
    assert!(
        !included_cost_conversion_is_paired(&location, &usd_asset(), &fee, &malformed).unwrap()
    );

    let mut foreign_anchor = effects.clone();
    let Some(FinancialEffect::PositionChange { anchor, .. }) = foreign_anchor.get_mut(2) else {
        panic!("fixture position change missing");
    };
    anchor.as_mut().unwrap().position = "other-request:collateral".into();
    assert!(
        !included_cost_conversion_is_paired(&location, &usd_asset(), &fee, &foreign_anchor)
            .unwrap()
    );

    let mut missing_anchor = effects;
    let Some(FinancialEffect::PositionChange { anchor, .. }) = missing_anchor.get_mut(2) else {
        panic!("fixture position change missing");
    };
    *anchor = None;
    let malformed_proof = proof(&record, missing_anchor);
    assert_eq!(
        validate_effects(&record, &malformed_proof, &Status::Completed)
            .unwrap_err()
            .code,
        "POSITION_ANCHOR_MISSING"
    );
}

#[test]
fn escrow_conversion_binds_only_the_filled_input_and_rejects_reuse_or_cost_mismatch() {
    let (location, effects) = escrow_fixture();
    let fee = quantity("1000").integer();
    assert!(included_cost_conversion_is_paired(&location, &usd_asset(), &fee, &effects).unwrap());

    let mut duplicate_source = effects.clone();
    duplicate_source.push(effects[3].clone());
    assert!(
        !included_cost_conversion_is_paired(&location, &usd_asset(), &fee, &duplicate_source)
            .unwrap()
    );

    let mut cost_mismatch = effects.clone();
    let Some(FinancialEffect::Cost { amount, .. }) = cost_mismatch.get_mut(2) else {
        panic!("fixture cost missing");
    };
    amount.atoms = quantity("999");
    assert!(
        !included_cost_conversion_is_paired(&location, &usd_asset(), &fee, &cost_mismatch).unwrap()
    );

    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let escrow = resource("solana:mainnet-beta:request-ata");
    let input = asset("solana:mainnet-beta", "USDC", 6);
    let margin = usd_asset();
    let expected = vec![EffectConstraint::EscrowToPosition {
        wallet_resource: wallet.clone(),
        escrow_resource: escrow,
        input_asset: input.clone(),
        position_resource: wallet.clone(),
        position: "perps:request:collateral".into(),
        position_kind: PositionKind::LockedCollateral,
        position_asset: margin.clone(),
        maximum_wallet_debit: quantity("2000000"),
        maximum_position_change: quantity("1000000"),
        minimum_position_per_input_numerator: Quantity::zero(),
        minimum_position_per_input_denominator: quantity("1"),
        conditional_position: None,
    }];
    let mut record = record(
        expected,
        vec![CostBound {
            location,
            amount: Amount {
                asset: margin,
                atoms: quantity("1000"),
            },
            treatment: CostTreatment::IncludedInDebit,
        }],
    );
    record.preview.as_mut().unwrap().intent.debits = vec![ResourceAmount {
        resource: wallet,
        amount: Amount {
            asset: input,
            atoms: quantity("2000000"),
        },
    }];
    let prepared = record.preview.as_ref().unwrap();
    record.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(prepared)).unwrap();
    validate_cost_bound_contract(&prepared.intent).unwrap();
    let fill_digest = "a1".repeat(32);
    let verified = progress_proof(
        &record,
        1,
        "request-fill-1",
        &[("keeper-fill-1", &fill_digest)],
        effects,
    );
    assert_eq!(
        validate_effects_mode(&record, &verified, &Status::Pending, true),
        Ok(())
    );
}

#[test]
fn optional_fee_denomination_allows_verified_no_fill_without_fake_cost() {
    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let location = AccountingLocation::Position {
        resource: wallet,
        position: "perps:request:collateral".into(),
        position_kind: PositionKind::LockedCollateral,
    };
    let margin = usd_asset();
    let token = asset("solana:mainnet-beta", "USDC", 6);
    let mut record = record(
        vec![EffectConstraint::CostDenomination {
            location: location.clone(),
            cost_asset: margin.clone(),
            category: "protocol_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
            denomination_asset: token,
            maximum_amount: quantity("1000"),
        }],
        vec![CostBound {
            location: location.clone(),
            amount: Amount {
                asset: margin,
                atoms: quantity("1000"),
            },
            treatment: CostTreatment::AdditionalDebit,
        }],
    );
    let prepared = record.preview.as_ref().unwrap();
    record.approval.as_mut().unwrap().binding_hash =
        binding_hash(&authorization_binding(prepared)).unwrap();
    let verified = proof(&record, Vec::new());
    assert_eq!(
        validate_effects(&record, &verified, &Status::Completed),
        Ok(())
    );

    // Incomplete progress can omit a fee that has not occurred yet, but it
    // cannot omit denomination evidence after reporting an actual fee Cost.
    let pending_cost_without_marker = progress_proof(
        &record,
        1,
        "request-fee-progress-1",
        &[("fee-event-1", &"ab".repeat(32))],
        vec![FinancialEffect::Cost {
            location: location.clone(),
            amount: Amount {
                asset: usd_asset(),
                atoms: quantity("1"),
            },
            category: "protocol_fee".into(),
            treatment: CostTreatment::AdditionalDebit,
        }],
    );
    assert_eq!(
        validate_effects_mode(
            &record,
            &pending_cost_without_marker,
            &Status::Pending,
            true,
        )
        .unwrap_err()
        .code,
        "COST_DENOMINATION_MISMATCH"
    );
}

#[test]
fn zero_usd_rounding_does_not_erase_a_positive_token_fee_denomination() {
    let wallet = resource("solana:mainnet-beta:wallet:owner");
    let location = AccountingLocation::Position {
        resource: wallet,
        position: "perps:request:collateral".into(),
        position_kind: PositionKind::LockedCollateral,
    };
    let margin = usd_asset();
    let token = asset("solana:mainnet-beta", "USDC", 6);
    let expected = vec![EffectConstraint::CostDenomination {
        location: location.clone(),
        cost_asset: margin.clone(),
        category: "protocol_fee".into(),
        treatment: CostTreatment::IncludedInDebit,
        denomination_asset: token.clone(),
        maximum_amount: quantity("10"),
    }];
    let effects = vec![
        FinancialEffect::Cost {
            location: location.clone(),
            amount: Amount {
                asset: margin.clone(),
                atoms: Quantity::zero(),
            },
            category: "protocol_fee".into(),
            treatment: CostTreatment::IncludedInDebit,
        },
        FinancialEffect::CostDenomination {
            location: location.clone(),
            cost_asset: margin.clone(),
            category: "protocol_fee".into(),
            treatment: CostTreatment::IncludedInDebit,
            denomination: Amount {
                asset: token.clone(),
                atoms: quantity("1"),
            },
            evidence_id: "rounding-event-1".into(),
        },
        FinancialEffect::IncludedCostConversion {
            location: location.clone(),
            source: gofin_domain::IncludedCostSource::Escrow {
                wallet_resource: resource("solana:mainnet-beta:wallet:owner"),
                escrow_resource: resource("solana:mainnet-beta:request-ata"),
            },
            gross_debit: Amount {
                asset: token.clone(),
                atoms: quantity("100"),
            },
            gross_converted: Amount {
                asset: margin.clone(),
                atoms: quantity("100"),
            },
            net_position: Amount {
                asset: margin.clone(),
                atoms: quantity("100"),
            },
            included_cost: Amount {
                asset: margin.clone(),
                atoms: Quantity::zero(),
            },
            evidence_id: "rounding-event-1".into(),
        },
    ];
    validate_cost_denominations(&expected, &effects).unwrap();

    let missing_denomination = vec![FinancialEffect::Cost {
        location: location.clone(),
        amount: Amount {
            asset: margin.clone(),
            atoms: Quantity::zero(),
        },
        category: "protocol_fee".into(),
        treatment: CostTreatment::IncludedInDebit,
    }];
    assert_eq!(
        validate_cost_denominations(&expected, &missing_denomination)
            .unwrap_err()
            .code,
        "COST_DENOMINATION_MISMATCH"
    );
}
