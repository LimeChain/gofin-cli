use super::*;

fn asset(network: &str, reference: &str) -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: network.to_owned(),
        reference: reference.to_owned(),
        decimals: 0,
    }
}

fn quantity(value: &str) -> gofin_domain::Quantity {
    gofin_domain::Quantity::new(value).unwrap()
}

fn price(asset: gofin_domain::AssetId, observed_at: DateTime<Utc>) -> gofin_domain::AssetPrice {
    gofin_domain::AssetPrice {
        asset,
        unit_atoms: quantity("100"),
        usd_micros_per_unit: quantity("1000000"),
        observed_at: observed_at.to_rfc3339(),
        source: "synthetic frozen-price fixture".into(),
    }
}

fn valuation(
    observed_at: DateTime<Utc>,
    prices: Vec<gofin_domain::AssetPrice>,
) -> gofin_domain::Valuation {
    gofin_domain::Valuation {
        movement_usd_micros: quantity("0"),
        maximum_fee_usd_micros: quantity("0"),
        additional_allocation_usd_micros: quantity("0"),
        observed_at: observed_at.to_rfc3339(),
        source: "synthetic adapter valuation".into(),
        prices,
    }
}

fn cost_effect(asset: gofin_domain::AssetId, atoms: &str) -> FinancialEffect {
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

fn composite_price(
    asset: gofin_domain::AssetId,
    updated: DateTime<Utc>,
    retrieved: &str,
) -> gofin_domain::AssetPrice {
    let mut row = price(asset, updated);
    row.source = format!(
        "Stablecoin/USD conversion; exchange=coinbase; exact_asset=USDC; CoinGecko public USD quote; coin_id=usd-coin; updated:{}; conversion retrieval; updated:{retrieved}; updated:{}",
        updated.to_rfc3339(),
        updated.to_rfc3339()
    );
    row
}

#[test]
fn frozen_composite_retrieval_preserves_submillisecond_precision_without_repricing() {
    let fee_asset = asset("coinbase", "USDC");
    let origin_asset = asset("chain:1", "origin");
    let retrieved = at(47) + chrono::Duration::microseconds(265547);
    let rounded = at(47) + chrono::Duration::milliseconds(265);
    let row = composite_price(fee_asset.clone(), at(0), &retrieved.to_rfc3339());
    // Live observation remains strictly rejected even 547us into the future.
    assert_eq!(
        validate_price_rows(
            std::slice::from_ref(&row),
            &BTreeSet::from([fee_asset.clone()]),
            rounded
        )
        .unwrap_err()
        .code,
        "COST_VALUATION_STALE"
    );
    validate_price_rows(
        std::slice::from_ref(&row),
        &BTreeSet::from([fee_asset.clone()]),
        retrieved,
    )
    .unwrap();
    let frozen = valuation(rounded, vec![row, price(origin_asset.clone(), rounded)]);
    assert_eq!(
        frozen_valuation_observation(
            &frozen,
            &BTreeSet::from([fee_asset.clone(), origin_asset.clone()])
        )
        .unwrap(),
        retrieved
    );
    assert_eq!(
        actual_cost_usd(&[cost_effect(fee_asset.clone(), "10")], &frozen).unwrap(),
        BigUint::from(100_000u32)
    );
    assert_eq!(
        convert_cost_to_origin(&fee_asset, &origin_asset, &quantity("10"), &frozen).unwrap(),
        quantity("10")
    );
}

#[test]
fn frozen_composite_anchor_does_not_accept_expired_malformed_or_inconsistent_legs() {
    let fee_asset = asset("coinbase", "USDC");
    let retrieved = at(47) + chrono::Duration::microseconds(265547);
    let mut wrong_effective = composite_price(fee_asset.clone(), at(0), &retrieved.to_rfc3339());
    wrong_effective.observed_at = at(1).to_rfc3339();
    let mut wrong_extra = composite_price(fee_asset.clone(), at(0), &retrieved.to_rfc3339());
    wrong_extra
        .source
        .push_str(&format!("; updated:{}", at(1).to_rfc3339()));
    for row in [
        composite_price(fee_asset.clone(), at(0), &at(300).to_rfc3339()),
        composite_price(fee_asset.clone(), at(48), &retrieved.to_rfc3339()),
        composite_price(fee_asset.clone(), at(0), "invalid"),
        wrong_effective,
        wrong_extra,
    ] {
        let frozen = valuation(at(47), vec![row]);
        assert_eq!(
            actual_cost_usd(&[cost_effect(fee_asset.clone(), "10")], &frozen)
                .unwrap_err()
                .code,
            "COST_VALUATION_STALE"
        );
    }
}

#[test]
fn boundary_calculation_timestamp_cannot_hide_stale_or_future_principal_prices() {
    let principal = asset("chain:1", "principal");
    let fee_asset = asset("chain:1", "fee");
    let current = at(200);
    let store = Store::in_memory().unwrap();
    let options = EngineOptions {
        max_movement_and_cost_usd_micros: "0".into(),
        max_run_allocation_usd_micros: "0".into(),
        allow_unpriced_exchange_assets: false,
    };
    for offset in [0, -120, 1] {
        let frozen = valuation(
            current,
            vec![
                price(
                    principal.clone(),
                    current + chrono::Duration::seconds(offset),
                ),
                price(fee_asset.clone(), current),
            ],
        );
        let prepared:gofin_domain::PreparedOperation=serde_json::from_value(json!({
            "operation":"synthetic.transfer","input":{},"binding":{},"summary":"Synthetic independently priced transfer","checks":[],"warnings":[],"observed_at":current.to_rfc3339(),"expires_at":(current+chrono::Duration::seconds(30)).to_rfc3339(),"valuation":frozen,"operation_version":1,
            "intent":{"debits":[{"resource":"synthetic-wallet","amount":{"asset":principal,"atoms":"1"}}],"maximum_costs":[{"location":{"kind":"cash","resource":"synthetic-wallet"},"amount":{"asset":fee_asset,"atoms":"1"},"treatment":"additional_debit"}],"expected":[],"locks":[]}
        })).unwrap();
        let result = ordinary_policy::validate_boundary_candidate_policy(
            &prepared,
            "synthetic",
            &options,
            &store,
            current,
        );
        if offset == 0 {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err().code, "COST_VALUATION_STALE");
        }
    }
}
