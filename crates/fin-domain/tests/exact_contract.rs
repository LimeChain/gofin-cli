use fin_domain::*;
use serde_json::json;

#[test]
fn unsigned_amounts_do_not_cross_a_float_boundary() {
    let amount = Quantity::from_decimal(
        "115792089237316195423570985008687907853269984665640564039457.584007913129639935",
        18,
    )
    .unwrap();
    assert_eq!(
        amount.to_string(),
        "115792089237316195423570985008687907853269984665640564039457584007913129639935"
    );
    assert_eq!(
        amount.to_decimal(18),
        "115792089237316195423570985008687907853269984665640564039457.584007913129639935"
    );
    assert_eq!(Quantity::new("0001").unwrap().to_string(), "1");
    assert_eq!(Quantity::new("000").unwrap().to_string(), "0");
    for invalid in ["", "-1", "1e3", "+1", "1.0", " 1", "1 ", "NaN", "١"] {
        assert!(Quantity::new(invalid).is_err(), "{invalid}");
    }
    assert!(Quantity::from_decimal("0.0000001", 6).is_err());
    assert!(Quantity::from_decimal("1.", 6).is_err());
    assert!(Quantity::from_decimal(".1", 6).is_err());
    assert!(serde_json::from_value::<Quantity>(json!(100)).is_err());
}

#[test]
fn request_precision_is_strict_and_reserve_rounding_is_explicit() {
    assert_eq!(
        Quantity::from_decimal("0.000001", 6).unwrap(),
        Quantity::from_u64(1)
    );
    let n = Quantity::from_u64(5);
    assert_eq!(
        n.mul_div_floor(&Quantity::from_u64(3), &Quantity::from_u64(2))
            .unwrap(),
        Quantity::from_u64(7)
    );
    assert_eq!(
        n.mul_div_ceil(&Quantity::from_u64(3), &Quantity::from_u64(2))
            .unwrap(),
        Quantity::from_u64(8)
    );
    assert!(n.mul_div_floor(&n, &Quantity::zero()).is_err());
    assert!(n.checked_sub(&Quantity::from_u64(6)).is_err());
    assert!(Quantity::from_u64(10) > Quantity::from_u64(9));
}

#[test]
fn signed_effects_keep_exact_sign_and_magnitude() {
    assert_eq!(SignedQuantity::new("-0001").unwrap().as_str(), "-1");
    assert_eq!(SignedQuantity::new("-0").unwrap().as_str(), "0");
    assert!(SignedQuantity::new("--1").is_err());
    assert!(SignedQuantity::new("+1").is_err());
    assert!(SignedQuantity::new("-1.0").is_err());
    assert!(SignedQuantity::negative(&Quantity::from_u64(9)).is_negative());
}

#[test]
fn canonical_request_identity_is_stable_and_scope_sensitive() {
    let mut a = OperationRequest {
        request_id: "logical-1".into(),
        operation: "wallet.transfer".into(),
        input: json!({"wallet":"source","amount":"0.1","to":"recipient"}),
        run_id: Some("scope1".into()),
    };
    let b = OperationRequest {
        request_id: "logical-2".into(),
        input: json!({"to":"recipient","amount":"0.1","wallet":"source"}),
        ..a.clone()
    };
    assert_eq!(
        request_fingerprint(&a).unwrap(),
        request_fingerprint(&b).unwrap()
    );
    let original = request_fingerprint(&a).unwrap();
    a.run_id = Some("scope2".into());
    assert_ne!(original, request_fingerprint(&a).unwrap());
    a.input["amount"] = json!(0.1);
    assert!(request_fingerprint(&a).is_err());
    assert!(binding_hash(&json!({"nested":[{"fee":1.01}]})).is_err());
}

#[test]
fn typed_financial_effects_reject_unknown_claims() {
    let unknown = json!({"kind":"balance_adjustment","resource":"wallet","atoms":"100"});
    assert!(serde_json::from_value::<FinancialEffect>(unknown).is_err());
    let sneaky = json!({"kind":"usable_credit","to":"wallet","amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"100"},"unapproved":true});
    assert!(serde_json::from_value::<FinancialEffect>(sneaky).is_err());
}
