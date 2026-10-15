//! Unsupported GoFin controls must not trigger authenticated discovery or dispatch.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport, contracts,
};
use async_trait::async_trait;
use gofin_domain::{Adapter, ResourceId, Result};
use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct NoReads(AtomicUsize);
#[async_trait]
impl KrakenTransport for NoReads {
    async fn request(
        &self,
        _: E,
        _: Option<&KrakenAccount>,
        _: Value,
        _: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("Unsupported GoFin controls reached native discovery")
    }
}
fn adapter(product: KrakenProduct, venue: Arc<NoReads>) -> ExchangeAdapter<KrakenProfile> {
    let accounts = (0..2)
        .map(|i| KrakenAccount {
            name: if i == 0 {
                "selected".into()
            } else {
                "other".into()
            },
            resource: ResourceId(format!("SYNTHETIC:{}:{i}", product.client())),
            credential_ref: format!("SYNTHETIC-not-a-live-key-{i}"),
            physical_identity: format!("SYNTHETIC-not-a-live-account-{i}"),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: true,
            product,
        })
        .collect();
    ExchangeAdapter::new(KrakenProfile::new(product, accounts, venue).unwrap())
}
fn spot() -> Value {
    json!({"account":"selected","requestId":"SYNTHETIC-controls","pair":"XBTUSD","type":"buy","ordertype":"limit","volume":"0.001","price":"20000","maximumFees":{"ZUSD":"1"}})
}
fn future() -> Value {
    json!({"account":"selected","requestId":"SYNTHETIC-controls","symbol":"PF_XBTUSD","side":"buy","orderType":"lmt","size":"0.001","limitPrice":"20000","maximumFees":{"USD":"1"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"})
}
#[tokio::test]
async fn unsupported_product_and_operation_controls_reject_before_alias_reads_or_dispatch() {
    for (endpoint, mut input, fields) in [
        (
            E::KrakenfuturesPrivatePostSendorder,
            future(),
            vec!["maximumSpend", "minimumReceive"],
        ),
        (
            E::KrakenPrivatePostAddOrder,
            spot(),
            vec![
                "maximumLoss",
                "maximumProfit",
                "maximumFundingLoss",
                "maximumFundingProfit",
                "settlementLimits",
            ],
        ),
        (
            E::KrakenPrivatePostCancelOrder,
            json!({"account":"selected","requestId":"SYNTHETIC-controls","txid":"SYNTHETIC-native-order"}),
            vec![
                "maximumFees",
                "maximumSpend",
                "minimumReceive",
                "maximumLoss",
            ],
        ),
        (
            E::KrakenfuturesPrivatePostCancelallordersafter,
            json!({"account":"selected","requestId":"SYNTHETIC-controls","timeout":60}),
            vec![
                "maximumFees",
                "maximumLoss",
                "operationId",
                "originalSubmission",
            ],
        ),
    ] {
        for field in fields {
            input[field] = json!("1");
            let venue = Arc::new(NoReads(AtomicUsize::new(0)));
            let selected = adapter(
                if endpoint.futures() {
                    KrakenProduct::Derivatives
                } else {
                    KrakenProduct::Spot
                },
                venue.clone(),
            );
            let error = selected
                .prepare(endpoint.operation(), &input)
                .await
                .unwrap_err();
            assert_eq!(
                error.code, "KRAKEN_CONTROL_UNSUPPORTED",
                "{endpoint:?}/{field}"
            );
            assert!(
                error.details.unwrap()["fields"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(format!("/{field}")))
            );
            assert_eq!(venue.0.load(Ordering::SeqCst), 0);
            input.as_object_mut().unwrap().remove(field);
        }
    }
}
#[tokio::test]
async fn futures_nested_spot_limits_and_cancel_limits_never_reach_native_reads() {
    for (kind, field) in [
        ("edit", "maximumSpend"),
        ("edit", "minimumReceive"),
        ("cancel", "maximumFees"),
        ("cancel", "maximumFundingProfit"),
    ] {
        let input = json!({"account":"selected","requestId":"SYNTHETIC-controls","batchOrder":[{"order":kind,"order_tag":"selected-child","order_id":"SYNTHETIC-order","size":"0.02"}],"amendments":[{"orderTag":"selected-child","operationId":"SYNTHETIC-original","originalSubmission":{"kind":"krakenfutures-rest-v1","reference":"SYNTHETIC-submission","data":{}},"limits":{field:"1"}}]});
        let venue = Arc::new(NoReads(AtomicUsize::new(0)));
        let error = adapter(KrakenProduct::Derivatives, venue.clone())
            .prepare(E::KrakenfuturesPrivatePostBatchorder.operation(), &input)
            .await
            .unwrap_err();
        assert_eq!(error.code, "KRAKEN_CONTROL_UNSUPPORTED");
        assert!(
            error.details.unwrap()["fields"]
                .as_array()
                .unwrap()
                .contains(&json!(format!("/amendments/0/limits/{field}")))
        );
        assert_eq!(venue.0.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn control_schemas_keep_actual_product_bounds_and_remove_foreign_limits() {
    for endpoint in [
        E::KrakenfuturesPrivatePostSendorder,
        E::KrakenfuturesPrivatePostEditorder,
        E::KrakenfuturesPrivatePostBatchorder,
    ] {
        let fields = contracts::financial_controls(endpoint);
        assert!(!fields.contains_key("maximumSpend") && !fields.contains_key("minimumReceive"));
        assert!(fields.contains_key("maximumLoss") && fields.contains_key("maximumFundingLoss"));
    }
    let fields = contracts::financial_controls(E::KrakenfuturesPrivatePostBatchorder);
    let nested = &fields["amendments"]["items"]["properties"]["limits"]["properties"];
    assert!(nested.get("maximumSpend").is_none() && nested.get("minimumReceive").is_none());
    for endpoint in [
        E::KrakenPrivatePostAddOrder,
        E::KrakenPrivatePostAddOrderBatch,
        E::KrakenPrivatePostAmendOrder,
        E::KrakenPrivatePostEditOrder,
    ] {
        let fields = contracts::financial_controls(endpoint);
        assert!(fields.contains_key("maximumSpend") && fields.contains_key("minimumReceive"));
        assert!(
            !fields.contains_key("maximumFundingLoss")
                && !fields.contains_key("maximumFundingProfit")
                && !fields.contains_key("settlementLimits")
        );
    }
    for endpoint in [
        E::KrakenPrivatePostCancelOrder,
        E::KrakenPrivatePostCancelAllOrdersAfter,
        E::KrakenfuturesPrivatePostCancelorder,
    ] {
        let fields = contracts::financial_controls(endpoint);
        assert!(
            contracts::MONETARY_CONTROLS
                .iter()
                .all(|field| !fields.contains_key(*field))
        );
    }
    let mut margin = spot();
    margin["leverage"] = json!("2");
    margin["maximumLoss"] = json!("1");
    margin["maximumProfit"] = json!("1");
    assert!(contracts::decode(E::KrakenPrivatePostAddOrder, &margin).is_ok());
    margin["minimumReceive"] = json!("0.001");
    assert_eq!(
        contracts::decode(E::KrakenPrivatePostAddOrder, &margin)
            .unwrap_err()
            .code,
        "KRAKEN_CONTROL_UNSUPPORTED"
    );
}
#[test]
fn validation_only_and_per_wallet_limits_cannot_silently_ignore_other_fin_controls() {
    let mut input = spot();
    input["validate"] = json!(true);
    assert_eq!(
        contracts::decode(E::KrakenPrivatePostAddOrder, &input)
            .unwrap_err()
            .code,
        "KRAKEN_CONTROL_UNSUPPORTED"
    );
    input.as_object_mut().unwrap().remove("maximumFees");
    assert!(contracts::decode(E::KrakenPrivatePostAddOrder, &input).is_ok());
    let input = json!({"account":"selected","requestId":"SYNTHETIC-controls","batchOrder":[{"order":"send","order_tag":"one","symbol":"PF_XBTUSD","side":"buy","orderType":"lmt","size":"0.001","limitPrice":"20000"}],"settlementLimits":{"flex":{"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"}},"maximumLoss":"2"});
    assert_eq!(
        contracts::decode(E::KrakenfuturesPrivatePostBatchorder, &input)
            .unwrap_err()
            .code,
        "KRAKEN_CONTROL_UNSUPPORTED"
    );
}
