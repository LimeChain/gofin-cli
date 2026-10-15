use super::*;
#[tokio::test]
async fn short_maximum_spend_caps_full_borrowed_base_not_quote_collateral() {
    let (adapter, venue) = setup(false, true, false, false, false);
    let mut input = input(false, true);
    input["maximumSpend"] = json!("0.009");
    assert_eq!(
        adapter
            .prepare(E::KrakenPrivatePostAddOrder.operation(), &input)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_SPEND_BOUND_EXCEEDED"
    );
    input["maximumSpend"] = json!("0.01");
    let prepared = adapter
        .prepare(E::KrakenPrivatePostAddOrder.operation(), &input)
        .await
        .unwrap();
    assert!(prepared.intent.expected.iter().any(|constraint|matches!(constraint,gofin_domain::EffectConstraint::Debt{asset,maximum_change,..} if asset.reference=="XXBT" && maximum_change.to_decimal(36)=="0.01")));
    assert!(prepared.warnings.iter().any(
        |warning| warning.contains("borrowed XXBT principal") && warning.contains("collateral")
    ));
    assert!(
        venue
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(endpoint, _)| !endpoint.mutating())
    );
}
#[tokio::test]
async fn closing_and_opening_pnl_irrelevant_controls_are_rejected_without_dispatch() {
    for closing in [false, true] {
        let (adapter, venue) = setup(closing, false, false, false, false);
        let mut input = input(closing, false);
        if closing {
            input["maximumSpend"] = json!("1000");
        } else {
            input["maximumLoss"] = json!("1");
        }
        assert_eq!(
            adapter
                .prepare(E::KrakenPrivatePostAddOrder.operation(), &input)
                .await
                .unwrap_err()
                .code,
            "KRAKEN_CONTROL_UNSUPPORTED"
        );
        assert!(
            venue
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(endpoint, _)| !endpoint.mutating())
        );
    }
}
