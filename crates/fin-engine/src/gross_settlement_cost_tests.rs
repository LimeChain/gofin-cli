use super::*;
fn fixture() -> (
    FinancialIntent,
    Vec<FinancialEffect>,
    Vec<fin_domain::AssetPrice>,
) {
    let resource = ResourceId("synthetic-collateral-wallet".into());
    let asset = fin_domain::AssetId {
        network: "synthetic".into(),
        reference: "USD".into(),
        decimals: 0,
    };
    let amount = |n| Amount {
        asset: asset.clone(),
        atoms: fin_domain::Quantity::from_u64(n),
    };
    let location = AccountingLocation::Cash {
        resource: resource.clone(),
    };
    let settlement = |category, minimum: &str| EffectConstraint::Settlement {
        resource: resource.clone(),
        asset: asset.clone(),
        category,
        minimum_delta: fin_domain::SignedQuantity::new(minimum).unwrap(),
        maximum_delta: fin_domain::SignedQuantity::new("100").unwrap(),
    };
    let intent = FinancialIntent {
        debits: vec![],
        locks: vec![resource.clone()],
        expected: vec![
            settlement(fin_domain::SettlementCategory::RealizedPnl, "-25"),
            settlement(fin_domain::SettlementCategory::Funding, "-5"),
        ],
        maximum_costs: vec![
            CostBound {
                location: location.clone(),
                amount: amount(30),
                treatment: CostTreatment::IncludedInDebit,
            },
            CostBound {
                location: location.clone(),
                amount: amount(15),
                treatment: CostTreatment::AdditionalDebit,
            },
        ],
    };
    let mut effects = vec![
        FinancialEffect::Settlement {
            resource: resource.clone(),
            asset: asset.clone(),
            category: fin_domain::SettlementCategory::RealizedPnl,
            delta: fin_domain::SignedQuantity::new("20").unwrap(),
        },
        FinancialEffect::Settlement {
            resource: resource.clone(),
            asset: asset.clone(),
            category: fin_domain::SettlementCategory::Funding,
            delta: fin_domain::SignedQuantity::new("1").unwrap(),
        },
        FinancialEffect::UsableCredit {
            to: resource,
            amount: amount(15),
        },
    ];
    for (category, n, treatment) in [
        ("proved_gross_pnl_loss", 20, CostTreatment::IncludedInDebit),
        (
            "proved_gross_funding_loss",
            2,
            CostTreatment::IncludedInDebit,
        ),
        (
            "proved_positive_commission",
            12,
            CostTreatment::AdditionalDebit,
        ),
    ] {
        effects.push(FinancialEffect::Cost {
            location: location.clone(),
            amount: amount(n),
            category: category.into(),
            treatment,
        });
    }
    let prices = vec![fin_domain::AssetPrice {
        asset,
        usd_micros_per_unit: fin_domain::Quantity::from_u64(1_000_000),
        unit_atoms: fin_domain::Quantity::from_u64(1),
        observed_at: now(),
        source: "synthetic exact USD unit".into(),
    }];
    (intent, effects, prices)
}
#[test]
fn gross_settlement_losses_remain_bounded_and_consumed_after_net_gains_and_rebates() {
    let (intent, effects, prices) = fixture();
    for effect in effects
        .iter()
        .filter(|effect| matches!(effect, FinancialEffect::Cost { .. }))
    {
        assert!(cost_within_intent(effect, &intent, &effects, Some(&prices)).unwrap());
    }
    assert_eq!(
        ordinary_policy::actual_effect_charge(&effects, &prices).unwrap(),
        BigUint::from(34_000_000u64)
    );
    let mut no_loss = intent.clone();
    for scope in &mut no_loss.expected {
        if let EffectConstraint::Settlement { minimum_delta, .. } = scope {
            *minimum_delta = fin_domain::SignedQuantity::new("0").unwrap();
        }
    }
    assert!(!cost_within_intent(&effects[3], &no_loss, &effects, Some(&prices)).unwrap());
    let mut no_settlement = effects.clone();
    no_settlement.retain(|effect| !matches!(effect, FinancialEffect::Settlement { .. }));
    assert!(!cost_within_intent(&effects[3], &intent, &no_settlement, Some(&prices)).unwrap());
    let mut exceeded = effects.clone();
    if let FinancialEffect::Cost { amount, .. } = &mut exceeded[3] {
        amount.atoms = fin_domain::Quantity::from_u64(31);
    }
    assert!(!cost_within_intent(&exceeded[3], &intent, &exceeded, Some(&prices)).unwrap());
}
#[test]
fn included_settlement_cost_is_not_charged_again_when_a_gross_cash_debit_contains_it() {
    let (_, mut effects, prices) = fixture();
    let FinancialEffect::Cost {
        location: AccountingLocation::Cash { resource },
        amount,
        ..
    } = &effects[3]
    else {
        unreachable!()
    };
    effects.push(FinancialEffect::CashDebit {
        resource: resource.clone(),
        amount: Amount {
            asset: amount.asset.clone(),
            atoms: fin_domain::Quantity::from_u64(100),
        },
    });
    assert_eq!(
        ordinary_policy::actual_effect_charge(&effects, &prices).unwrap(),
        BigUint::from(112_000_000u64)
    );
}
