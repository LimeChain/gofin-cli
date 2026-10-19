use super::*;

fn resource(name: &str) -> ResourceId {
    ResourceId(name.to_owned())
}

fn asset(name: &str) -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: "synthetic".into(),
        reference: name.into(),
        decimals: 0,
    }
}

fn q(value: u64) -> gofin_domain::Quantity {
    gofin_domain::Quantity::from_u64(value)
}

fn delta(value: i64) -> gofin_domain::SignedQuantity {
    gofin_domain::SignedQuantity::new(value.to_string()).unwrap()
}

fn cash(resource: &ResourceId, asset: &gofin_domain::AssetId, amount: u64) -> ProjectedCash {
    ProjectedCash {
        resource: resource.clone(),
        asset: asset.clone(),
        amount: QuantityInterval {
            minimum: q(amount),
            maximum: q(amount),
        },
    }
}

fn proof(expected: Vec<EffectConstraint>, effects: Vec<ProjectedEffect>) -> ReturnRouteProof {
    ReturnRouteProof {
        step_index: 0,
        operation: "synthetic".into(),
        input_hash: "synthetic".into(),
        observed_at: "2026-01-01T00:00:00Z".into(),
        expires_at: "2026-01-01T00:01:00Z".into(),
        intent: FinancialIntent {
            expected,
            ..Default::default()
        },
        effects,
        requires: vec![],
        evidence: serde_json::Value::Null,
    }
}

fn cash_delta(
    resource: &ResourceId,
    asset: &gofin_domain::AssetId,
    minimum: i64,
    maximum: i64,
) -> ProjectedEffect {
    ProjectedEffect::CashDelta {
        resource: resource.clone(),
        asset: asset.clone(),
        minimum_delta: delta(minimum),
        maximum_delta: delta(maximum),
    }
}

fn transfer_proof(
    from: &ResourceId,
    to: &ResourceId,
    asset: &gofin_domain::AssetId,
    debit: u64,
    credit: u64,
) -> ReturnRouteProof {
    let mut result = proof(
        vec![EffectConstraint::Transfer {
            from: from.clone(),
            to: to.clone(),
            debit_asset: asset.clone(),
            credit_asset: asset.clone(),
            maximum_debit: q(debit),
            minimum_credit: q(credit),
        }],
        vec![
            cash_delta(from, asset, -(debit as i64), -(debit as i64)),
            cash_delta(to, asset, credit as i64, credit as i64),
        ],
    );
    result.intent.debits = vec![ResourceAmount {
        resource: from.clone(),
        amount: Amount {
            asset: asset.clone(),
            atoms: q(debit),
        },
    }];
    result
}

#[test]
fn paired_transfer_and_conversion_require_their_own_legs() {
    let a = resource("a");
    let b = resource("b");
    let c = resource("c");
    let x = asset("X");
    let y = asset("Y");
    let z = asset("Z");
    let mut proof = transfer_proof(&a, &b, &x, 10, 10);
    proof.intent.expected.push(EffectConstraint::Conversion {
        resource: c.clone(),
        input: y.clone(),
        maximum_input: q(5),
        output: z.clone(),
        minimum_output: q(4),
    });
    let d = resource("d");
    proof.intent.expected.push(EffectConstraint::Conversion {
        resource: d.clone(),
        input: z.clone(),
        maximum_input: q(5),
        output: y.clone(),
        minimum_output: q(4),
    });
    proof.effects.extend([
        cash_delta(&c, &y, -5, -5),
        cash_delta(&c, &z, 4, 4),
        cash_delta(&d, &z, -5, -5),
        cash_delta(&d, &y, 4, 4),
    ]);
    proof.effects.retain(|effect| {
        !matches!(effect, ProjectedEffect::CashDelta { resource, asset, .. } if resource == &a && asset == &x)
    });
    let state = ProjectedState {
        cash: vec![cash(&a, &x, 10), cash(&c, &y, 5), cash(&d, &z, 5)],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&state, &proof).is_err());
}

#[test]
fn accepts_fully_paired_transfer_and_conversion() {
    let a = resource("a");
    let b = resource("b");
    let x = asset("X");
    let mut transfer = transfer_proof(&a, &b, &x, 10, 9);
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&a, &x, 10)],
                ..Default::default()
            },
            &transfer
        )
        .is_ok()
    );

    let y = asset("Y");
    let mut conversion = proof(
        vec![EffectConstraint::Conversion {
            resource: a.clone(),
            input: x.clone(),
            maximum_input: q(10),
            output: y.clone(),
            minimum_output: q(8),
        }],
        vec![cash_delta(&a, &x, -10, -9), cash_delta(&a, &y, 8, 9)],
    );
    conversion.intent.debits = vec![ResourceAmount {
        resource: a.clone(),
        amount: Amount {
            asset: x.clone(),
            atoms: q(10),
        },
    }];
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&a, &x, 10)],
                ..Default::default()
            },
            &conversion
        )
        .is_ok()
    );

    // The typed debit cap and the actual projected worst-case debit must both
    // fit the selected source; a larger effect cannot borrow another flow's cap.
    transfer.effects[0] = cash_delta(&a, &x, -11, -11);
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&a, &x, 20)],
                ..Default::default()
            },
            &transfer
        )
        .is_err()
    );
}

#[test]
fn rejects_credit_below_minimum_zero_source_overspend_and_detached_credit() {
    let a = resource("a");
    let b = resource("b");
    let x = asset("X");

    let mut below_minimum = transfer_proof(&a, &b, &x, 5, 5);
    below_minimum.effects[1] = cash_delta(&b, &x, 4, 5);
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&a, &x, 5)],
                ..Default::default()
            },
            &below_minimum
        )
        .is_err()
    );

    let valid = transfer_proof(&a, &b, &x, 5, 5);
    assert!(validate_projected_cash_legs(&ProjectedState::default(), &valid).is_err());
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&a, &x, 4)],
                ..Default::default()
            },
            &valid
        )
        .is_err()
    );

    let mut detached_credit = transfer_proof(&a, &b, &x, 5, 5);
    let c = resource("c");
    detached_credit.effects.push(cash_delta(&c, &x, 1, 1));
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&a, &x, 5)],
                ..Default::default()
            },
            &detached_credit
        )
        .is_err()
    );
}

#[test]
fn accepts_supply_borrow_and_repay_with_redundant_typed_claims() {
    let wallet = resource("wallet");
    let pool = resource("pool");
    let x = asset("X");
    let supply_location = AccountingLocation::Position {
        resource: pool.clone(),
        position: "supply:X".into(),
        position_kind: PositionKind::Supply,
    };
    let mut supply = proof(
        vec![
            EffectConstraint::Debit {
                resource: wallet.clone(),
                asset: x.clone(),
                maximum_debit: q(10),
            },
            EffectConstraint::Position {
                resource: pool.clone(),
                position: "supply:X".into(),
                asset: x.clone(),
                direction: Direction::Increase,
                maximum_change: q(10),
            },
            EffectConstraint::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                to: supply_location.clone(),
                debit_asset: x.clone(),
                credit_asset: x.clone(),
                maximum_debit: q(10),
                minimum_credit: q(9),
            },
        ],
        vec![
            cash_delta(&wallet, &x, -10, -10),
            ProjectedEffect::PositionDelta {
                resource: pool.clone(),
                position: "supply:X".into(),
                asset: x.clone(),
                position_kind: PositionKind::Supply,
                minimum_delta: delta(9),
                maximum_delta: delta(10),
            },
        ],
    );
    supply.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: x.clone(),
            atoms: q(10),
        },
    }];
    let supply_state = ProjectedState {
        cash: vec![cash(&wallet, &x, 10)],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&supply_state, &supply).is_ok());

    let debt_key = (
        pool.clone(),
        "debt:X".to_owned(),
        x.clone(),
        PositionKind::VariableDebt,
    );
    let debt_location = AccountingLocation::Position {
        resource: pool.clone(),
        position: "debt:X".into(),
        position_kind: PositionKind::VariableDebt,
    };
    let borrow = proof(
        vec![
            EffectConstraint::Debt {
                resource: pool.clone(),
                debt: "debt:X".into(),
                asset: x.clone(),
                direction: Direction::Increase,
                maximum_change: q(10),
            },
            EffectConstraint::Credit {
                to: wallet.clone(),
                asset: x.clone(),
                minimum_credit: q(9),
            },
            EffectConstraint::OriginMovement {
                from: debt_location.clone(),
                to: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                debit_asset: x.clone(),
                credit_asset: x.clone(),
                maximum_debit: q(10),
                minimum_credit: q(9),
            },
        ],
        vec![
            cash_delta(&wallet, &x, 9, 9),
            ProjectedEffect::PositionDelta {
                resource: debt_key.0.clone(),
                position: debt_key.1.clone(),
                asset: x.clone(),
                position_kind: debt_key.3.clone(),
                minimum_delta: delta(9),
                maximum_delta: delta(10),
            },
        ],
    );
    assert!(validate_projected_cash_legs(&ProjectedState::default(), &borrow).is_ok());

    let mut repay = proof(
        vec![
            EffectConstraint::Debit {
                resource: wallet.clone(),
                asset: x.clone(),
                maximum_debit: q(10),
            },
            EffectConstraint::Debt {
                resource: pool.clone(),
                debt: "debt:X".into(),
                asset: x.clone(),
                direction: Direction::Decrease,
                maximum_change: q(10),
            },
            EffectConstraint::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                to: debt_location.clone(),
                debit_asset: x.clone(),
                credit_asset: x.clone(),
                maximum_debit: q(10),
                minimum_credit: q(1),
            },
        ],
        vec![
            cash_delta(&wallet, &x, -10, -8),
            ProjectedEffect::PositionClose {
                resource: pool.clone(),
                position: "debt:X".into(),
                asset: x.clone(),
                position_kind: PositionKind::VariableDebt,
                maximum_amount: q(10),
                source_step: 0,
            },
        ],
    );
    repay.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: x.clone(),
            atoms: q(10),
        },
    }];
    let repay_state = ProjectedState {
        cash: vec![cash(&wallet, &x, 10)],
        positions: vec![ProjectedPosition {
            resource: pool,
            position: "debt:X".into(),
            asset: x,
            position_kind: PositionKind::VariableDebt,
            amount: QuantityInterval {
                minimum: q(9),
                maximum: q(10),
            },
            backing_valuation: None,
            vault_backing_valuation: None,
            lending_backing_valuation: None,
        }],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&repay_state, &repay).is_ok());
}

#[test]
fn all_conversion_sweeps_use_prior_intervals_and_position_close_uses_bounded_stock() {
    let wallet = resource("wallet");
    let x = asset("X");
    let y = asset("Y");
    let mut sweep = proof(
        vec![EffectConstraint::Conversion {
            resource: wallet.clone(),
            input: x.clone(),
            maximum_input: q(10),
            output: y.clone(),
            minimum_output: q(6),
        }],
        vec![
            ProjectedEffect::CashSweep {
                resource: wallet.clone(),
                asset: x.clone(),
                maximum_amount: q(10),
                source_step: 0,
            },
            cash_delta(&wallet, &y, 6, 9),
        ],
    );
    sweep.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: x.clone(),
            atoms: q(10),
        },
    }];
    let funded = ProjectedState {
        cash: vec![ProjectedCash {
            resource: wallet.clone(),
            asset: x.clone(),
            amount: QuantityInterval {
                minimum: q(7),
                maximum: q(9),
            },
        }],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&funded, &sweep).is_ok());
    let over_source_cap = ProjectedState {
        cash: vec![ProjectedCash {
            resource: wallet.clone(),
            asset: x.clone(),
            amount: QuantityInterval {
                minimum: q(7),
                maximum: q(11),
            },
        }],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&over_source_cap, &sweep).is_err());

    let pool = resource("pool");
    let debt = AccountingLocation::Position {
        resource: pool.clone(),
        position: "debt:X".into(),
        position_kind: PositionKind::VariableDebt,
    };
    let mut close = proof(
        vec![
            EffectConstraint::Debit {
                resource: wallet.clone(),
                asset: x.clone(),
                maximum_debit: q(22),
            },
            EffectConstraint::Debt {
                resource: pool.clone(),
                debt: "debt:X".into(),
                asset: x.clone(),
                direction: Direction::Decrease,
                maximum_change: q(22),
            },
            EffectConstraint::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                to: debt.clone(),
                debit_asset: x.clone(),
                credit_asset: x.clone(),
                maximum_debit: q(22),
                minimum_credit: q(1),
            },
        ],
        vec![
            cash_delta(&wallet, &x, -8, -7),
            ProjectedEffect::PositionClose {
                resource: pool.clone(),
                position: "debt:X".into(),
                asset: x.clone(),
                position_kind: PositionKind::VariableDebt,
                maximum_amount: q(22),
                source_step: 0,
            },
        ],
    );
    close.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: x.clone(),
            atoms: q(22),
        },
    }];
    let close_state = ProjectedState {
        cash: vec![cash(&wallet, &x, 8)],
        positions: vec![ProjectedPosition {
            resource: pool.clone(),
            position: "debt:X".into(),
            asset: x.clone(),
            position_kind: PositionKind::VariableDebt,
            amount: QuantityInterval {
                minimum: q(7),
                maximum: q(8),
            },
            backing_valuation: None,
            vault_backing_valuation: None,
            lending_backing_valuation: None,
        }],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&close_state, &close).is_ok());
}

#[test]
fn additional_cost_can_fund_an_authority_only_step_without_a_principal_leg() {
    let wallet = resource("wallet");
    let native = asset("native");
    let mut proof = proof(
        vec![EffectConstraint::Authority {
            resource: wallet.clone(),
            authority: "router".into(),
            before: "0".into(),
            after: "1".into(),
        }],
        vec![cash_delta(&wallet, &native, -2, 0)],
    );
    proof.intent.maximum_costs.push(CostBound {
        location: AccountingLocation::Cash {
            resource: wallet.clone(),
        },
        amount: Amount {
            asset: native.clone(),
            atoms: q(2),
        },
        treatment: CostTreatment::AdditionalDebit,
    });
    let state = ProjectedState {
        cash: vec![cash(&wallet, &native, 2)],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&state, &proof).is_ok());
}

#[test]
fn conversion_cannot_carry_an_unlinked_cash_debit_constraint() {
    let wallet = resource("wallet");
    let input = asset("input");
    let output = asset("output");
    let mut proof = proof(
        vec![
            EffectConstraint::Conversion {
                resource: wallet.clone(),
                input: input.clone(),
                maximum_input: q(10),
                output: output.clone(),
                minimum_output: q(8),
            },
            EffectConstraint::Debit {
                resource: wallet.clone(),
                asset: input.clone(),
                maximum_debit: q(10),
            },
        ],
        vec![
            cash_delta(&wallet, &input, -10, -10),
            cash_delta(&wallet, &output, 8, 9),
        ],
    );
    proof.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: input.clone(),
            atoms: q(10),
        },
    }];
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&wallet, &input, 10)],
                ..Default::default()
            },
            &proof
        )
        .is_err()
    );
}

#[test]
fn additional_debit_is_removed_once_before_matching_principal() {
    let from = resource("from");
    let to = resource("to");
    let token = asset("token");
    let mut transfer = proof(
        vec![EffectConstraint::Transfer {
            from: from.clone(),
            to: to.clone(),
            debit_asset: token.clone(),
            credit_asset: token.clone(),
            maximum_debit: q(3),
            minimum_credit: q(3),
        }],
        vec![
            cash_delta(&from, &token, -5, -3),
            cash_delta(&to, &token, 3, 3),
        ],
    );
    transfer.intent.debits = vec![ResourceAmount {
        resource: from.clone(),
        amount: Amount {
            asset: token.clone(),
            atoms: q(3),
        },
    }];
    transfer.intent.maximum_costs = vec![CostBound {
        location: AccountingLocation::Cash {
            resource: from.clone(),
        },
        amount: Amount {
            asset: token.clone(),
            atoms: q(2),
        },
        treatment: CostTreatment::AdditionalDebit,
    }];
    let state = ProjectedState {
        cash: vec![cash(&from, &token, 5)],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&state, &transfer).is_ok());

    transfer.effects[0] = cash_delta(&from, &token, -6, -3);
    assert!(validate_projected_cash_legs(&state, &transfer).is_err());
}

#[test]
fn unwrap_credit_floor_is_gross_but_native_gas_stays_net_and_pre_funded() {
    let wallet = resource("wallet");
    let weth = asset("weth");
    let native = asset("native");
    let mut unwrap = proof(
        vec![EffectConstraint::Conversion {
            resource: wallet.clone(),
            input: weth.clone(),
            maximum_input: q(10),
            output: native.clone(),
            minimum_output: q(10),
        }],
        vec![
            ProjectedEffect::CashSweep {
                resource: wallet.clone(),
                asset: weth.clone(),
                maximum_amount: q(10),
                source_step: 0,
            },
            // Gross unwrap credit [10, 12], less the separately bounded 2 gas
            // debit in the same native asset gives net projection [8, 12].
            cash_delta(&wallet, &native, 8, 12),
        ],
    );
    unwrap.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: weth.clone(),
            atoms: q(10),
        },
    }];
    unwrap.intent.maximum_costs = vec![CostBound {
        location: AccountingLocation::Cash {
            resource: wallet.clone(),
        },
        amount: Amount {
            asset: native.clone(),
            atoms: q(2),
        },
        treatment: CostTreatment::AdditionalDebit,
    }];
    let state = ProjectedState {
        cash: vec![cash(&wallet, &native, 3), cash(&wallet, &weth, 10)],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&state, &unwrap).is_ok());

    let underfunded_gas = ProjectedState {
        cash: vec![cash(&wallet, &native, 1), cash(&wallet, &weth, 10)],
        ..Default::default()
    };
    assert!(validate_projected_cash_legs(&underfunded_gas, &unwrap).is_err());

    unwrap.effects[1] = cash_delta(&wallet, &native, 7, 12);
    assert!(validate_projected_cash_legs(&state, &unwrap).is_err());
}

#[test]
fn tight_debit_credit_and_position_endpoint_refinements_are_enforced() {
    let wallet = resource("wallet");
    let pool = resource("pool");
    let token = asset("token");

    let mut transfer = transfer_proof(&wallet, &pool, &token, 100, 100);
    transfer.intent.debits[0].amount.atoms = q(100);
    transfer.effects[0] = cash_delta(&wallet, &token, -1, -1);
    transfer.effects[1] = cash_delta(&pool, &token, 100, 100);
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&wallet, &token, 1)],
                ..Default::default()
            },
            &transfer
        )
        .is_err()
    );

    let supply_position = AccountingLocation::Position {
        resource: pool.clone(),
        position: "supply".into(),
        position_kind: PositionKind::Supply,
    };
    let mut tighter_debit = proof(
        vec![
            EffectConstraint::Debit {
                resource: wallet.clone(),
                asset: token.clone(),
                maximum_debit: q(1),
            },
            EffectConstraint::Position {
                resource: pool.clone(),
                position: "supply".into(),
                asset: token.clone(),
                direction: Direction::Increase,
                maximum_change: q(10),
            },
            EffectConstraint::OriginMovement {
                from: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                to: supply_position.clone(),
                debit_asset: token.clone(),
                credit_asset: token.clone(),
                maximum_debit: q(10),
                minimum_credit: q(1),
            },
        ],
        vec![
            cash_delta(&wallet, &token, -10, -10),
            ProjectedEffect::PositionDelta {
                resource: pool.clone(),
                position: "supply".into(),
                asset: token.clone(),
                position_kind: PositionKind::Supply,
                minimum_delta: delta(1),
                maximum_delta: delta(10),
            },
        ],
    );
    tighter_debit.intent.debits = vec![ResourceAmount {
        resource: wallet.clone(),
        amount: Amount {
            asset: token.clone(),
            atoms: q(10),
        },
    }];
    assert!(
        validate_projected_cash_legs(
            &ProjectedState {
                cash: vec![cash(&wallet, &token, 10)],
                ..Default::default()
            },
            &tighter_debit
        )
        .is_err()
    );

    let tighter_credit = proof(
        vec![
            EffectConstraint::Debt {
                resource: pool.clone(),
                debt: "debt".into(),
                asset: token.clone(),
                direction: Direction::Increase,
                maximum_change: q(10),
            },
            EffectConstraint::Credit {
                to: wallet.clone(),
                asset: token.clone(),
                minimum_credit: q(100),
            },
            EffectConstraint::OriginMovement {
                from: AccountingLocation::Position {
                    resource: pool.clone(),
                    position: "debt".into(),
                    position_kind: PositionKind::VariableDebt,
                },
                to: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                debit_asset: token.clone(),
                credit_asset: token.clone(),
                maximum_debit: q(10),
                minimum_credit: q(1),
            },
        ],
        vec![
            cash_delta(&wallet, &token, 1, 1),
            ProjectedEffect::PositionDelta {
                resource: pool.clone(),
                position: "debt".into(),
                asset: token.clone(),
                position_kind: PositionKind::VariableDebt,
                minimum_delta: delta(1),
                maximum_delta: delta(10),
            },
        ],
    );
    assert!(validate_projected_cash_legs(&ProjectedState::default(), &tighter_credit).is_err());

    let wrong_supply_source = proof(
        vec![
            EffectConstraint::Position {
                resource: pool.clone(),
                position: "supply".into(),
                asset: token.clone(),
                direction: Direction::Increase,
                maximum_change: q(10),
            },
            EffectConstraint::OriginMovement {
                from: supply_position.clone(),
                to: AccountingLocation::Cash {
                    resource: wallet.clone(),
                },
                debit_asset: token.clone(),
                credit_asset: token.clone(),
                maximum_debit: q(10),
                minimum_credit: q(1),
            },
        ],
        vec![
            cash_delta(&wallet, &token, 1, 1),
            ProjectedEffect::PositionDelta {
                resource: pool,
                position: "supply".into(),
                asset: token.clone(),
                position_kind: PositionKind::Supply,
                minimum_delta: delta(1),
                maximum_delta: delta(10),
            },
        ],
    );
    assert!(
        validate_projected_cash_legs(&ProjectedState::default(), &wrong_supply_source).is_err()
    );
}
