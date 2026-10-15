use super::*;
fn batch() -> Value {
    json!({"account":"spot","requestId":"SYNTHETIC-whole-principal","pair":"XBTUSD","orders":[{"ordertype":"limit","type":"buy","volume":"0.01","price":"20000"},{"ordertype":"limit","type":"buy","volume":"0.01","price":"20000"}],"maximumFees":{"XXBT":"0.0002","ZUSD":"2"},"maximumSpend":"400"})
}
#[tokio::test]
async fn cash_batch_principal_cap_is_aggregate_not_reused_for_each_order() {
    let (adapter, venue) = setup("open", "0", "0", "0", false, false, false);
    let mut input = batch();
    input["maximumSpend"] = json!("300");
    assert_eq!(
        adapter
            .prepare(
                KrakenEndpoint::KrakenPrivatePostAddOrderBatch.operation(),
                &input
            )
            .await
            .unwrap_err()
            .code,
        "KRAKEN_SPEND_BOUND_EXCEEDED"
    );
    input["maximumSpend"] = json!("400");
    let prepared = adapter
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrderBatch.operation(),
            &input,
        )
        .await
        .unwrap();
    assert_eq!(
        prepared
            .intent
            .debits
            .iter()
            .filter(|debit| debit.amount.asset.reference == "ZUSD")
            .try_fold(Quantity::zero(), |sum, debit| sum
                .checked_add(&debit.amount.atoms))
            .unwrap()
            .to_decimal(36),
        "400"
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
#[tokio::test]
async fn cash_batch_market_collars_share_only_remaining_exact_quote_principal() {
    let (adapter, _) = setup("open", "0", "0", "0", false, false, false);
    let mut input = batch();
    input["orders"][0]["volume"] = json!("0.005");
    input["orders"][1] = json!({"ordertype":"market","type":"buy","volume":"0.01"});
    let prepared = adapter
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrderBatch.operation(),
            &input,
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.binding["parameters"]["orders"][1]["price"],
        "30000"
    );
    assert_eq!(
        prepared.binding["parameters"]["orders"][1]["timeinforce"],
        "IOC"
    );
    input["orders"][0] = json!({"ordertype":"market","type":"buy","volume":"0.01"});
    let prepared = adapter
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrderBatch.operation(),
            &input,
        )
        .await
        .unwrap();
    assert!(
        prepared.binding["parameters"]["orders"]
            .as_array()
            .unwrap()
            .iter()
            .all(|order| order["price"] == "20000" && order["timeinforce"] == "IOC")
    );
    let orders: Vec<crate::trading::SpotOrder> =
        serde_json::from_value(prepared.binding["orders"].clone()).unwrap();
    assert_eq!(
        orders
            .iter()
            .try_fold(Quantity::zero(), |sum, order| sum
                .checked_add(&order.maximum_spend))
            .unwrap()
            .to_decimal(36),
        "400"
    );
}
#[tokio::test]
async fn cash_batch_scalar_cap_cannot_compare_base_and_quote_sources() {
    let (adapter, venue) = setup("open", "0", "0", "0", false, false, false);
    let mut input = batch();
    input["orders"][1]["type"] = json!("sell");
    assert_eq!(
        adapter
            .prepare(
                KrakenEndpoint::KrakenPrivatePostAddOrderBatch.operation(),
                &input
            )
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
