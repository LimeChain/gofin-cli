use super::*;

fn asset(network: &str, reference: &str) -> fin_domain::AssetId {
    fin_domain::AssetId {
        network: network.to_owned(),
        reference: reference.to_owned(),
        decimals: 0,
    }
}

fn quantity(value: &str) -> fin_domain::Quantity {
    fin_domain::Quantity::new(value).unwrap()
}

fn price(asset: fin_domain::AssetId, observed_at: DateTime<Utc>) -> fin_domain::AssetPrice {
    fin_domain::AssetPrice {
        asset,
        unit_atoms: quantity("100"),
        usd_micros_per_unit: quantity("1000000"),
        observed_at: observed_at.to_rfc3339(),
        source: "synthetic frozen-price fixture".into(),
    }
}

fn valuation(
    observed_at: DateTime<Utc>,
    prices: Vec<fin_domain::AssetPrice>,
) -> fin_domain::Valuation {
    fin_domain::Valuation {
        movement_usd_micros: quantity("0"),
        maximum_fee_usd_micros: quantity("0"),
        additional_allocation_usd_micros: quantity("0"),
        observed_at: observed_at.to_rfc3339(),
        source: "synthetic adapter valuation".into(),
        prices,
    }
}

fn cost_effect(asset: fin_domain::AssetId, atoms: &str) -> FinancialEffect {
    FinancialEffect::Cost {
        location: AccountingLocation::Cash {
            resource: ResourceId("synthetic-wallet".into()),
        },
        amount: Amount {
            asset,
            atoms: quantity(atoms),
        },
        category: "synthetic fee".into(),
        treatment: CostTreatment::AdditionalDebit,
    }
}

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2020-10-02T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + chrono::Duration::seconds(seconds)
}

#[test]
fn later_frozen_rows_value_cost_and_convert_attribution_without_wall_clock_dependency() {
    let fee_asset = asset("chain:1", "fee");
    let origin_asset = asset("venue", "origin");
    let frozen = valuation(
        at(0),
        vec![
            price(fee_asset.clone(), at(6)),
            price(origin_asset.clone(), at(6)),
        ],
    );

    // Actual fee 10/100 units at $1 per unit is $0.10 = 100,000 micro-USD.
    assert_eq!(
        actual_cost_usd(&[cost_effect(fee_asset.clone(), "10")], &frozen).unwrap(),
        BigUint::from(100_000u32)
    );
    // Both assets have the same frozen unit price, so 10 fee atoms map to 10 origin atoms.
    assert_eq!(
        convert_cost_to_origin(&fee_asset, &origin_asset, &quantity("10"), &frozen).unwrap(),
        quantity("10")
    );
}

#[test]
fn frozen_validation_rejects_rows_more_than_two_minutes_apart() {
    let fee_asset = asset("chain:1", "fee");
    let origin_asset = asset("venue", "origin");
    let frozen = valuation(
        at(0),
        vec![
            price(fee_asset.clone(), at(0)),
            price(origin_asset.clone(), at(121)),
        ],
    );

    let error =
        convert_cost_to_origin(&fee_asset, &origin_asset, &quantity("1"), &frozen).unwrap_err();
    assert_eq!(error.code, "COST_VALUATION_STALE");
}

#[test]
fn unrelated_new_price_cannot_mask_a_stale_required_cost_price() {
    let fee_asset = asset("chain:1", "fee");
    let unrelated_asset = asset("chain:1", "unrelated");
    let frozen = valuation(
        at(0),
        vec![
            price(fee_asset.clone(), at(-121)),
            price(unrelated_asset, at(6)),
        ],
    );

    let error = actual_cost_usd(&[cost_effect(fee_asset, "1")], &frozen).unwrap_err();
    assert_eq!(error.code, "COST_VALUATION_STALE");
}

#[test]
fn settlement_price_rows_still_require_exact_unique_positive_rows() {
    let fee_asset = asset("chain:1", "fee");
    let effect = [cost_effect(fee_asset.clone(), "1")];

    let missing = valuation(at(0), vec![]);
    assert_eq!(
        actual_cost_usd(&effect, &missing).unwrap_err().code,
        "COST_VALUATION_MISSING"
    );

    let duplicate = valuation(
        at(0),
        vec![
            price(fee_asset.clone(), at(0)),
            price(fee_asset.clone(), at(6)),
        ],
    );
    assert_eq!(
        actual_cost_usd(&effect, &duplicate).unwrap_err().code,
        "COST_VALUATION_MISSING"
    );

    let mut zero_price = price(fee_asset.clone(), at(0));
    zero_price.usd_micros_per_unit = quantity("0");
    let zero = valuation(at(0), vec![zero_price]);
    assert_eq!(
        actual_cost_usd(&effect, &zero).unwrap_err().code,
        "COST_VALUATION_INVALID"
    );
}

#[test]
fn a_frozen_price_remains_usable_during_late_reconciliation() {
    let fee_asset = asset("chain:1", "fee");
    let origin_asset = asset("venue", "origin");
    let frozen = valuation(
        at(0),
        vec![
            price(fee_asset.clone(), at(6)),
            price(origin_asset.clone(), at(6)),
        ],
    );

    // Settlement uses the approved frozen observation, not the current wall clock.
    assert_eq!(
        actual_cost_usd(&[cost_effect(fee_asset.clone(), "10")], &frozen).unwrap(),
        BigUint::from(100_000u32)
    );
    assert_eq!(
        convert_cost_to_origin(&fee_asset, &origin_asset, &quantity("10"), &frozen).unwrap(),
        quantity("10")
    );
}
