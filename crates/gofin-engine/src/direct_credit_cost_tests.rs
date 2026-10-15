use super::*;
use gofin_domain::Quantity;

fn fixture() -> (FinancialIntent, ResourceId, gofin_domain::AssetId) {
    let resource = ResourceId("synthetic:cash-recipient".into());
    let asset = gofin_domain::AssetId {
        network: "synthetic".into(),
        reference: "USD".into(),
        decimals: 0,
    };
    let intent = FinancialIntent {
        debits: vec![],
        locks: vec![resource.clone()],
        expected: vec![],
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset.clone(),
                atoms: Quantity::from_u64(2),
            },
            treatment: CostTreatment::DeductedFromCredit,
        }],
    };
    (intent, resource, asset)
}

#[test]
fn direct_credit_and_cash_origin_back_only_the_exact_output_fee_identity() {
    let (mut intent, resource, asset) = fixture();
    let direct = EffectConstraint::Credit {
        to: resource.clone(),
        asset: asset.clone(),
        minimum_credit: Quantity::from_u64(8),
    };
    let origin = EffectConstraint::OriginMovement {
        from: AccountingLocation::Position {
            resource: ResourceId("synthetic:supply".into()),
            position: "supply".into(),
            position_kind: gofin_domain::PositionKind::Supply,
        },
        to: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        debit_asset: asset.clone(),
        credit_asset: asset.clone(),
        maximum_debit: Quantity::from_u64(10),
        minimum_credit: Quantity::from_u64(8),
    };
    for expected in [direct, origin] {
        intent.expected = vec![expected];
        validate_cost_bound_contract(&intent).unwrap();
        assert!(amendment_credit_bound(&intent, &resource, &asset));

        let mut wrong_resource = intent.clone();
        wrong_resource.maximum_costs[0].location = AccountingLocation::Cash {
            resource: ResourceId("synthetic:another-recipient".into()),
        };
        assert!(validate_cost_bound_contract(&wrong_resource).is_err());
        let mut wrong_asset = intent.clone();
        wrong_asset.maximum_costs[0].amount.asset.reference = "OTHER".into();
        assert!(validate_cost_bound_contract(&wrong_asset).is_err());
    }
}

#[test]
fn source_only_or_position_destination_cannot_back_a_cash_withholding() {
    let (mut intent, resource, asset) = fixture();
    intent.expected = vec![EffectConstraint::Debit {
        resource: resource.clone(),
        asset: asset.clone(),
        maximum_debit: Quantity::from_u64(10),
    }];
    assert!(validate_cost_bound_contract(&intent).is_err());
    intent.expected = vec![EffectConstraint::OriginMovement {
        from: AccountingLocation::Cash { resource },
        to: AccountingLocation::Position {
            resource: ResourceId("synthetic:supply".into()),
            position: "supply".into(),
            position_kind: gofin_domain::PositionKind::Supply,
        },
        debit_asset: asset.clone(),
        credit_asset: asset,
        maximum_debit: Quantity::from_u64(10),
        minimum_credit: Quantity::from_u64(8),
    }];
    assert!(validate_cost_bound_contract(&intent).is_err());
}
