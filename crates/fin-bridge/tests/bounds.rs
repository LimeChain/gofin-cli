use fin_bridge::{
    AmountInterval, BridgeBounds, BridgeFee, FeeCap, FeeKind, PricedBridgeFee,
    validate_fee_usd_caps, validate_usd_bounds,
};
use fin_domain::{Amount, AssetId, Quantity};

#[test]
fn usd_input_price_and_total_caps_use_independent_exact_asset_pricing() {
    let asset = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let amount = Amount {
        asset,
        atoms: Quantity::from_decimal("0.5", 18).unwrap(),
    };
    let bounds = BridgeBounds {
        input_usd_micros: Some(AmountInterval {
            minimum: Quantity::from_u64(49_000_000),
            maximum: Quantity::from_u64(51_000_000),
        }),
        source_usd_per_unit_micros: Some(AmountInterval {
            minimum: Quantity::from_u64(99_000_000),
            maximum: Quantity::from_u64(101_000_000),
        }),
        maximum_total_usd_micros: Some(50_100_000),
        ..Default::default()
    };
    assert!(validate_usd_bounds(&bounds, &amount, 100_000_000, &[100_000]).is_ok());
    assert!(validate_usd_bounds(&bounds, &amount, 101_000_000, &[]).is_err());
    assert!(validate_usd_bounds(&bounds, &amount, 100_000_000, &[200_000]).is_err());
}

#[test]
fn fee_usd_caps_aggregate_exact_asset_and_kind() {
    let asset = AssetId {
        network: "ethereum:1".into(),
        reference: "native".into(),
        decimals: 18,
    };
    let bounds = BridgeBounds {
        fee_caps: vec![FeeCap {
            asset: asset.clone(),
            kind: FeeKind::Gas,
            maximum: Quantity::from_u64(100),
            maximum_usd_micros: Some(150),
        }],
        ..Default::default()
    };
    let priced = |usd_micros| PricedBridgeFee {
        fee: BridgeFee {
            amount: Amount {
                asset: asset.clone(),
                atoms: Quantity::from_u64(1),
            },
            kind: FeeKind::Gas,
            payer: "sender".into(),
        },
        usd_micros,
    };
    assert!(validate_fee_usd_caps(&bounds, &[priced(50), priced(100)]).is_ok());
    assert!(validate_fee_usd_caps(&bounds, &[priced(151)]).is_err());
}
