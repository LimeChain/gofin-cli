// Counterexamples for exact noncash source authority; no venue-name branches.
fn non_cash_receipt_fixture() -> (
    financial_plans::FinancialPlanStepSpec,
    FinancialPlanStepPermission,
    PreparedOperation,
) {
    let resource = ResourceId("acct".into());
    let token = asset("ETH");
    let position = "synthetic:supply".to_string();
    let step = financial_plans::FinancialPlanStepSpec {
        step_id: "redeem".into(),
        operation: "synthetic.redeem".into(),
        input: json!({"quantity":"100"}),
        permitted_variations: json!({}),
        completion: financial_plans::FinancialStepCompletion {
            principal_spend: None,
            minimum_net_receipt: Some(resource_amount("acct", "ETH", "99")),
            operation_succeeded: true,
        },
        funding_dependencies: vec![],
        action_deadline_seconds: None,
    };
    let permission = FinancialPlanStepPermission {
        step_id: step.step_id.clone(),
        permission: None,
        exact: Some(FinancialPlanExactPermission {
            scope: FinancialPlanExactScope {
                mode: financial_plans::FinancialPlanExactMode::Exact,
                maximum_debits: vec![],
                maximum_costs: vec![],
                possible_credits: vec![FinancialPlanFundingIdentity {
                    resource: resource.clone(),
                    asset: token.clone(),
                }],
            },
            definition_hash: "synthetic-definition".into(),
            connections_hash: "synthetic-identity".into(),
        }),
        generated_input_fields: vec![],
    };
    let prepared = PreparedOperation {
        operation: step.operation.clone(),
        input: step.input.clone(),
        summary: "Redeem finite existing supply".into(),
        binding: json!({"source":"synthetic:supply","quantity":"100"}),
        intent: FinancialIntent {
            debits: vec![],
            maximum_costs: vec![],
            expected: vec![
                EffectConstraint::Position {
                    resource: resource.clone(),
                    position: position.clone(),
                    asset: token.clone(),
                    direction: Direction::Decrease,
                    maximum_change: Quantity::from_u64(101),
                },
                EffectConstraint::OriginMovement {
                    from: AccountingLocation::Position {
                        resource: resource.clone(),
                        position,
                        position_kind: fin_domain::PositionKind::Supply,
                    },
                    to: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    debit_asset: token.clone(),
                    credit_asset: token.clone(),
                    maximum_debit: Quantity::from_u64(101),
                    minimum_credit: Quantity::from_u64(100),
                },
                EffectConstraint::Credit {
                    to: resource.clone(),
                    asset: token,
                    minimum_credit: Quantity::from_u64(99),
                },
            ],
            locks: vec![resource],
        },
        valuation: None,
        observed_at: now(),
        expires_at: (Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
        action_deadline: None,
            operation_version: 1,
        checks: vec![],
        warnings: vec![],
    };
    (step, permission, prepared)
}
#[test]
fn exact_non_cash_receipt_is_initially_eligible_but_possible_credit_is_not_source_authority() {
    let (step, permission, p) = non_cash_receipt_fixture();
    assert!(!step_authority_cannot_complete(&step, &permission, &[], &[], &[], &[]).unwrap());
    validate_exact_non_cash_receipt_candidate(&step, &permission, &p, &[]).unwrap();
    let mut absent = p.clone();
    absent
        .intent
        .expected
        .retain(|c| matches!(c, EffectConstraint::Credit { .. }));
    assert!(validate_exact_non_cash_receipt_candidate(&step, &permission, &absent, &[]).is_err());
    for index in 0..8 {
        let mut bad = p.clone();
        match index {
            0 => {
                if let EffectConstraint::OriginMovement { maximum_debit, .. } =
                    &mut bad.intent.expected[1]
                {
                    *maximum_debit = Quantity::zero();
                }
            }
            1 => {
                if let EffectConstraint::OriginMovement {
                    from: AccountingLocation::Position { position_kind, .. },
                    ..
                } = &mut bad.intent.expected[1]
                {
                    *position_kind = fin_domain::PositionKind::VariableDebt;
                }
            }
            2 => {
                if let EffectConstraint::OriginMovement { to, .. } = &mut bad.intent.expected[1] {
                    *to = AccountingLocation::Cash {
                        resource: ResourceId("unapproved".into()),
                    };
                }
            }
            3 => {
                if let EffectConstraint::OriginMovement { credit_asset, .. } =
                    &mut bad.intent.expected[1]
                {
                    *credit_asset = asset("USDT");
                }
            }
            4 => {
                if let EffectConstraint::Position { direction, .. } = &mut bad.intent.expected[0] {
                    *direction = Direction::Increase;
                }
            }
            5 => {
                if let EffectConstraint::Position { maximum_change, .. } =
                    &mut bad.intent.expected[0]
                {
                    *maximum_change = Quantity::from_u64(200);
                }
            }
            6 => bad.intent.expected.push(EffectConstraint::Credit {
                to: ResourceId("unapproved".into()),
                asset: asset("ETH"),
                minimum_credit: Quantity::from_u64(1),
            }),
            7 => bad.intent.debits.push(resource_amount("acct", "ETH", "1")),
            _ => unreachable!(),
        }
        assert!(
            validate_exact_non_cash_receipt_candidate(&step, &permission, &bad, &[]).is_err(),
            "counterexample {index}"
        );
    }
    let mut boundary = permission.clone();
    boundary.exact = None;
    assert!(step_authority_cannot_complete(&step, &boundary, &[], &[], &[], &[]).unwrap());
}
#[test]
fn finite_non_cash_step_blocks_after_positive_effects_but_zero_effect_failure_can_retry() {
    let (step, permission, p) = non_cash_receipt_fixture();
    let zero = FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: ResourceId("acct".into()),
        },
        amount: Amount {
            asset: asset("ETH"),
            atoms: Quantity::zero(),
        },
        category: "synthetic-no-fee".into(),
        treatment: CostTreatment::DeductedFromCredit,
    };
    assert!(
        !step_authority_cannot_complete(
            &step,
            &permission,
            &[],
            &[],
            &[],
            std::slice::from_ref(&zero)
        )
        .unwrap()
    );
    validate_exact_non_cash_receipt_candidate(&step, &permission, &p, &[zero]).unwrap();
    let positive = vec![
        FinancialEffect::PositionChange {
            resource: ResourceId("acct".into()),
            position: "synthetic:supply".into(),
            asset: asset("ETH"),
            delta: fin_domain::SignedQuantity::new("-50").unwrap(),
            scaled_receipt: None,
            anchor: None,
        },
        FinancialEffect::Cost {
            location: AccountingLocation::Cash {
                resource: ResourceId("acct".into()),
            },
            amount: Amount {
                asset: asset("ETH"),
                atoms: Quantity::from_u64(1),
            },
            category: "synthetic-paid-cost".into(),
            treatment: CostTreatment::DeductedFromCredit,
        },
    ];
    for effect in positive {
        assert!(
            step_authority_cannot_complete(
                &step,
                &permission,
                &[],
                &[],
                &[],
                std::slice::from_ref(&effect)
            )
            .unwrap()
        );
        assert!(
            validate_exact_non_cash_receipt_candidate(&step, &permission, &p, &[effect]).is_err()
        );
    }
    // Existing cash-funded cumulative capacity retains its original behavior.
    let mut cash = permission;
    cash.exact.as_mut().unwrap().scope.maximum_debits = vec![resource_amount("acct", "ETH", "200")];
    let spent = vec![resource_amount("acct", "ETH", "50")];
    let effects = [FinancialEffect::CashDebit {
        resource: ResourceId("acct".into()),
        amount: Amount {
            asset: asset("ETH"),
            atoms: Quantity::from_u64(50),
        },
    }];
    assert!(
        !step_authority_cannot_complete(
            &step,
            &cash,
            &spent,
            &[resource_amount("acct", "ETH", "150")],
            &[],
            &effects
        )
        .unwrap()
    );
}
