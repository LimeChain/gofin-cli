// Explicit zero caps preserve a priced fee identity without financial authority.
#[test]
fn exact_zero_fee_scope_keeps_identity_and_rejects_positive_candidate_without_widening() {
    let fee = CostBound {
        location: AccountingLocation::Cash {
            resource: ResourceId("acct".into()),
        },
        amount: Amount {
            asset: asset("ETH"),
            atoms: Quantity::zero(),
        },
        treatment: CostTreatment::DeductedFromCredit,
    };
    let scope = FinancialPlanExactScope {
        mode: financial_plans::FinancialPlanExactMode::Exact,
        maximum_debits: vec![],
        maximum_costs: vec![fee.clone()],
        possible_credits: vec![],
    };
    validate_exact_scope(&scope).unwrap();
    let mut prepared = non_cash_receipt_fixture().2;
    prepared.intent.expected.clear();
    prepared.intent.maximum_costs = vec![fee.clone()];
    let auth = BoundaryAuthorization {
        id: "synthetic-zero-fee".into(),
        intent_id: "synthetic-zero-fee".into(),
        operation: prepared.operation.clone(),
        bounds: Value::Null,
        spec: BoundarySpec {
            fixed: Value::Null,
            parameters: Value::Null,
            maximum_debits: vec![],
            maximum_costs: scope.maximum_costs.clone(),
            summary: "Exact zero fee authority".into(),
        },
        approved_at: now(),
        expires_at: prepared.expires_at.clone(),
        valid_for_seconds: 60,
        status: BoundaryAuthorizationStatus::Active,
        remaining_debits: vec![],
        remaining_costs: scope.maximum_costs.clone(),
        spent_debits: vec![],
        spent_costs: vec![],
        replaces: None,
    };
    validate_candidate_caps(&auth, &prepared).unwrap();
    assert_eq!(auth.remaining_costs[0].amount.atoms, Quantity::zero());
    prepared.intent.maximum_costs[0].amount.atoms = Quantity::from_u64(1);
    assert!(validate_candidate_caps(&auth, &prepared).is_err());
    assert_eq!(auth.remaining_costs, vec![fee.clone()]);
    prepared.intent.maximum_costs[0] = fee.clone();
    prepared.intent.maximum_costs[0].treatment = CostTreatment::AdditionalDebit;
    // Upstream fee-free candidates require no fee identity; zero grants no cost
    // authority. A positive candidate still needs the exact approved treatment.
    validate_candidate_caps(&auth, &prepared).unwrap();
    prepared.intent.maximum_costs[0].amount.atoms = Quantity::from_u64(1);
    assert!(validate_candidate_caps(&auth, &prepared).is_err());
    let mut debit_scope = scope.clone();
    debit_scope.maximum_debits = vec![resource_amount("acct", "ETH", "0")];
    assert!(validate_exact_scope(&debit_scope).is_err());
    let mut duplicate = scope.clone();
    duplicate.maximum_costs.push(fee.clone());
    assert!(validate_exact_scope(&duplicate).is_err());
    for index in 0..5 {
        let mut invalid = scope.clone();
        match index {
            0 => invalid.maximum_costs[0].amount.asset.network.clear(),
            1 => invalid.maximum_costs[0].amount.asset.reference = " ETH".into(),
            2 => {
                invalid.maximum_costs[0].location = AccountingLocation::Cash {
                    resource: ResourceId(" ".into()),
                }
            }
            3 => {
                invalid.maximum_costs[0].location = AccountingLocation::Position {
                    resource: ResourceId("acct".into()),
                    position: String::new(),
                    position_kind: fin_domain::PositionKind::Supply,
                }
            }
            4 => {
                invalid.maximum_costs[0].location = AccountingLocation::Position {
                    resource: ResourceId("acct".into()),
                    position: "supply".into(),
                    position_kind: fin_domain::PositionKind::Supply,
                };
                invalid.maximum_costs[0].treatment = CostTreatment::AdditionalDebit;
            }
            _ => unreachable!(),
        };
        assert!(
            validate_exact_scope(&invalid).is_err(),
            "malformed zero-fee identity {index}"
        );
    }
}
