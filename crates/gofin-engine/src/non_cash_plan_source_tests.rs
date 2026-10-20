// Counterexamples for exact noncash source authority; no venue-name branches.
#[test]
fn bridge_receipt_scope_retains_frozen_target_source_proof_and_exclusive_child_claim() {
    use gofin_domain::{
        BridgeAllocationOwner, BridgeSettlementExpectation, BridgeSettlementTarget, BridgeStepKind,
        BridgeStepLink, BridgeStepState, BridgeTransferRecord, BridgeTransferState,
        TransitResource,
    };
    let (mut step, permission, mut prepared) = non_cash_receipt_fixture();
    step.operation = "bridge.transfer".into();
    step.input = json!({"transferId":"bridge-1","stepId":"mint-1","stepKind":"mint"});
    prepared.operation = step.operation.clone();
    prepared.input = step.input.clone();
    prepared.intent.expected.clear();
    let expectation = BridgeSettlementExpectation {
        destination: vec![BridgeSettlementTarget {
            resource: ResourceId("acct".into()),
            asset: asset("ETH"),
            minimum_atoms: Quantity::from_u64(100),
        }],
        refund: vec![],
    };
    let bounds = json!({"maximum_source_debit":"100","minimum_output":"100"});
    prepared.binding = json!({
        (gofin_domain::BRIDGE_STEP_METADATA_KEY):{
            "transfer_id":"bridge-1","step_id":"mint-1","kind":"mint","grant_id":null,
            "bounds":bounds,"settlement_expectation":expectation
        }
    });
    let transfer = BridgeTransferRecord {
        version: 1,
        id: "bridge-1".into(),
        grant_id: None,
        plan_scope: None,
        metadata: json!({}),
        approved_bounds: Some(bounds),
        settlement_expectation: Some(expectation),
        prospective_debit: None,
        state: BridgeTransferState::InTransit,
        quote_revisions: vec![],
        steps: vec![
            BridgeStepLink {
                transfer_id: "bridge-1".into(),
                grant_id: None,
                plan_authority: None,
                step_id: "burn-1".into(),
                operation_id: Some("original-burn".into()),
                kind: BridgeStepKind::Deposit,
                depends_on: vec![],
                state: BridgeStepState::Settled,
            },
            BridgeStepLink {
                transfer_id: "bridge-1".into(),
                grant_id: None,
                plan_authority: None,
                step_id: "mint-1".into(),
                operation_id: None,
                kind: BridgeStepKind::Mint,
                depends_on: vec!["burn-1".into()],
                state: BridgeStepState::Unsubmitted,
            },
        ],
        allocation_owner: BridgeAllocationOwner::Transfer {
            transfer_id: "bridge-1".into(),
        },
        transit: Some(TransitResource {
            transfer_id: "bridge-1".into(),
            amount: Amount {
                asset: asset("source-USDC"),
                atoms: Quantity::from_u64(100),
            },
            evidence_id: "original-source-proof".into(),
            destination_locator: None,
            proof_digest: "verified-source-digest".into(),
            observed_at: now(),
            spendable: false,
        }),
        applied_proofs: vec![],
        settlement_proofs: vec![],
    };
    let check =
        |candidate: &PreparedOperation, transfer: &BridgeTransferRecord, existing: Option<&str>| {
            let mut store = Store::in_memory().unwrap();
            let mut initial = transfer.clone();
            for link in &mut initial.steps {
                link.operation_id = None;
                link.state = BridgeStepState::Unsubmitted;
            }
            store.save_bridge_transfer(&initial).unwrap();
            for link in &transfer.steps {
                if let Some(id) = &link.operation_id {
                    let mut operation = settled_effect_record(vec![]);
                    operation.id = id.clone();
                    operation.request_id = id.clone();
                    store
                        .create_request(
                            id,
                            id,
                            id,
                            "bridge.transfer",
                            None,
                            "completed",
                            &to_value(&operation).unwrap(),
                            &now(),
                        )
                        .unwrap();
                }
                store.link_bridge_step(link).unwrap();
            }
            let request = OperationRequest {
                request_id: "receipt-candidate".into(),
                operation: candidate.operation.clone(),
                input: candidate.input.clone(),
                run_id: None,
            };
            validate_plan_bridge_attempt_scope(
                &request,
                candidate,
                "plan-1",
                "revision-1",
                &step.step_id,
                existing,
                &store,
            )
        };
    let original = check(&prepared, &transfer, None).unwrap().unwrap();
    assert_eq!(original.kind, BridgeStepKind::Mint);
    assert_eq!(
        original.updated_transfer.steps[1].state,
        BridgeStepState::Unsubmitted
    );
    assert_eq!(
        original.updated_transfer.steps[1].operation_id, None,
        "validation never claims"
    );
    assert_eq!(original.updated_transfer.transit, transfer.transit);

    let mut changed = prepared.clone();
    changed.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["settlement_expectation"]["destination"]
        [0]["resource"] = json!("other-wallet");
    assert_eq!(
        check(&changed, &transfer, None).err().unwrap().code,
        "PLAN_BRIDGE_SCOPE_CONFLICT"
    );
    changed = prepared.clone();
    changed.input["stepId"] = json!("different-mint");
    changed.binding[gofin_domain::BRIDGE_STEP_METADATA_KEY]["step_id"] = json!("different-mint");
    assert_eq!(
        check(&changed, &transfer, None).err().unwrap().code,
        "PLAN_BRIDGE_STEP_NOT_FOUND"
    );
    let mut missing_proof = transfer.clone();
    missing_proof.transit = None;
    assert_eq!(
        check(&prepared, &missing_proof, None).err().unwrap().code,
        "PLAN_BRIDGE_SOURCE_PROOF_REQUIRED"
    );
    let mut unsettled = transfer.clone();
    unsettled.steps[0].state = BridgeStepState::Submitted;
    assert_eq!(
        check(&prepared, &unsettled, None).err().unwrap().code,
        "PLAN_BRIDGE_STEP_UNAVAILABLE"
    );
    let mut claimed = transfer.clone();
    claimed.steps[1].operation_id = Some("another-attempt".into());
    claimed.steps[1].state = BridgeStepState::Submitted;
    assert_eq!(
        check(&prepared, &claimed, None).err().unwrap().code,
        "PLAN_BRIDGE_STEP_UNAVAILABLE"
    );
    assert_eq!(
        check(&prepared, &claimed, Some("new-attempt"))
            .err()
            .unwrap()
            .code,
        "PLAN_BRIDGE_STEP_UNAVAILABLE"
    );

    let mut record = prior_record();
    record.revision.spec.steps = vec![step.clone()];
    record.revision.permissions = vec![permission.clone()];
    let substantive = [FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: ResourceId("acct".into()),
        },
        amount: Amount {
            asset: asset("ETH"),
            atoms: Quantity::from_u64(1),
        },
        category: "gas".into(),
        treatment: CostTreatment::AdditionalDebit,
    }];
    assert_eq!(
        non_cash_plan_sources::validate_exact_bridge_receipt_candidate(
            &record,
            &step,
            &permission,
            &prepared,
            &substantive,
            &Store::in_memory().unwrap(),
        )
        .unwrap_err()
        .code,
        "PLAN_NON_CASH_SOURCE_INVALID",
    );
    // A credit-only promise remains insufficient even with a bridge marker.
    let mut extra = prepared.clone();
    extra.intent.expected.push(EffectConstraint::Credit {
        to: ResourceId("acct".into()),
        asset: asset("ETH"),
        minimum_credit: Quantity::from_u64(100),
    });
    assert!(
        non_cash_plan_sources::validate_exact_bridge_receipt_candidate(
            &record,
            &step,
            &permission,
            &extra,
            &[],
            &Store::in_memory().unwrap(),
        )
        .is_err()
    );
}

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
                        position_kind: gofin_domain::PositionKind::Supply,
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
                    *position_kind = gofin_domain::PositionKind::VariableDebt;
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
            delta: gofin_domain::SignedQuantity::new("-50").unwrap(),
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
