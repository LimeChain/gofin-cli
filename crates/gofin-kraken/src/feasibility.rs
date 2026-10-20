//! Read-only, balance-independent admission for future-funded plan steps.
use crate::{KrakenEndpoint as E, KrakenProfile, contracts, profile::error, trading};
use gofin_domain::{Quantity, Result};
use serde_json::Value;

impl KrakenProfile {
    pub(crate) fn kraken_plan_exclusive_resources(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<Option<Vec<gofin_domain::ResourceId>>> {
        let endpoint = contracts::endpoint(operation)?;
        if endpoint != E::KrakenPrivatePostAddOrder
            || contracts::contract(endpoint).client != self.product.client()
        {
            return Ok(None);
        }
        let (selection, parameters) = contracts::decode(endpoint, input)?;
        // Match ordinary spot-order preparation. Amendments, linked protection,
        // settlement and leveraged shapes retain their dispatch relationship checks.
        if parameters.get("close").is_some()
            || parameters.get("close[ordertype]").is_some()
            || parameters["validate"] == true
            || parameters["ordertype"] == "settle-position"
            || parameters
                .get("leverage")
                .is_some_and(|v| !matches!(v.as_str(), Some("none" | "1")))
        {
            return Ok(None);
        }
        let account =
            self.account(selection.account.as_deref().ok_or_else(|| {
                error("INVALID_INPUT", "Kraken writes require an exact account.")
            })?)?;
        Ok(Some(vec![account.resource.clone()]))
    }

    pub(crate) async fn validate_kraken_plan_feasibility(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<Vec<String>> {
        let endpoint = contracts::endpoint(operation)?;
        let contract = contracts::contract(endpoint);
        if contract.client != self.product.client() || contract.administrative || !contract.mutating
        {
            return Err(error(
                "WRONG_OPERATION_KIND",
                "Select a financial write for this exact Kraken product.",
            ));
        }
        contracts::validate_gofin_controls(endpoint, input, None)?;
        let (selection, mut parameters) = contracts::decode(endpoint, input)?;
        let account =
            self.account(selection.account.as_deref().ok_or_else(|| {
                error("INVALID_INPUT", "Kraken writes require an exact account.")
            })?)?;
        match endpoint {
            E::KrakenPrivatePostAddOrder
                if parameters.get("close").is_none()
                    && parameters["validate"] != true
                    && parameters["ordertype"] != "settle-position"
                    && parameters
                        .get("leverage")
                        .is_none_or(|v| matches!(v.as_str(), Some("none" | "1"))) =>
            {
                let controls = trading::controls(input)?;
                self.spot_permissions(
                    account,
                    &[
                        "query-funds",
                        "query-open-trades",
                        "query-closed-trades",
                        "query-ledger",
                        "modify-trades",
                    ],
                )
                .await?;
                let order = self
                    .prepare_spot_order(account, &mut parameters, &controls, 0, false)
                    .await?;
                for asset in [&order.base.reference, &order.quote.reference] {
                    if !controls.maximum_fees.contains_key(asset) {
                        return Err(error(
                            "KRAKEN_FEE_BOUNDS_REQUIRED",
                            &format!(
                                "Declare maximumFees for {asset}; Kraken fee-currency preferences do not guarantee which traded asset pays the fee."
                            ),
                        ));
                    }
                }
                if controls
                    .maximum_fees
                    .keys()
                    .any(|asset| asset != &order.base.reference && asset != &order.quote.reference)
                {
                    return Err(error(
                        "INVALID_INPUT",
                        "maximumFees names an asset outside the exact traded pair.",
                    ));
                }
                self.verify_spot_fee_admission(account, std::slice::from_ref(&order), &controls)
                    .await?;
                if order.buy
                    && !order.quoted_volume
                    && let Some(minimum) = &controls.minimum_receive
                {
                    let fee =
                        Quantity::from_decimal(&controls.maximum_fees[&order.base.reference], 36)?;
                    if fee > order.volume
                        || Quantity::from_decimal(minimum, 36)? > order.volume.checked_sub(&fee)?
                    {
                        return Err(error(
                            "KRAKEN_NET_RECEIPT_BOUND_INFEASIBLE",
                            "The base-volume buy cannot guarantee minimumReceive after its declared base-asset fee cap. Review the volume, fee cap and net receipt together.",
                        ));
                    }
                }
                let mut warnings = vec!["Current Kraken market rules, order precision/minimum, principal bounds, permissions and fee caps checked. Balances, live order acceptance, execution-time prices/fees and fill receipts remain deferred. Fee-currency flags are preferences; both traded assets require caps.".into()];
                if controls.minimum_receive.is_some() && (!order.buy || order.quoted_volume) {
                    warnings.push("Pre-approval minimumReceive feasibility is deferred for sells and quote-volume buys. Their net output depends on execution prices and fees; this review does not establish that the receipt target is achievable. Settlement still checks the declared bound.".into());
                }
                Ok(warnings)
            }
            E::KrakenPrivatePostWithdraw | E::KrakenFundingPostFundingV1Withdrawals => {
                self.validate_withdrawal_feasibility(endpoint, input)
                    .await?;
                Ok(vec!["Current Kraken withdrawal route, exact quantity/fees, saved destination, permissions and requested receipt observer checked. Available balances, account withdrawal limits and execution-time route/fee changes remain deferred; no funds reserved or sent.".into()])
            }
            _ => Ok(vec![format!(
                "Kraken pre-approval feasibility is not implemented for this {operation} request shape. Venue-specific fees, route/destination and account constraints remain deferred until execution; schema acceptance is not a feasibility guarantee."
            )]),
        }
    }
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    use crate::{KrakenAccount, KrakenProduct, KrakenResponse, KrakenTransport};
    use async_trait::async_trait;
    use gofin_domain::{Adapter, ResourceId};
    use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
    use serde_json::json;
    use std::sync::Arc;

    struct NoNetwork;
    #[async_trait]
    impl KrakenTransport for NoNetwork {
        async fn request(
            &self,
            _: E,
            _: Option<&KrakenAccount>,
            _: Value,
            _: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            panic!("known plan lock observation must be local and read-only")
        }
    }
    fn adapter() -> ExchangeAdapter<KrakenProfile> {
        ExchangeAdapter::new(
            KrakenProfile::new(
                KrakenProduct::Spot,
                vec![KrakenAccount {
                    name: "spot".into(),
                    resource: ResourceId("kraken:protected-synthetic:spot".into()),
                    credential_ref: "synthetic-unused".into(),
                    physical_identity: "synthetic-uid".into(),
                    wallet_account_id: None,
                    default_wallet_account_id: None,
                    identity_verified: true,
                    product: KrakenProduct::Spot,
                }],
                Arc::new(NoNetwork),
            )
            .unwrap(),
        )
    }
    fn order() -> Value {
        json!({
            "requestId":"synthetic-order","account":"spot","pair":"SOLETH",
            "type":"sell","ordertype":"limit","volume":"0.4","price":"0.05",
            "maximumFees":{"SOL":"0.01","XETH":"0.001"},"timeinforce":"IOC"
        })
    }
    #[test]
    fn ordinary_new_spot_order_reports_only_protected_account_lock_without_network() {
        let adapter = adapter();
        let expected = Some(vec![ResourceId("kraken:protected-synthetic:spot".into())]);
        for side in ["buy", "sell"] {
            let mut input = order();
            input["type"] = json!(side);
            assert_eq!(
                adapter
                    .plan_exclusive_resources("kraken.add-order", &input)
                    .unwrap(),
                expected
            );
        }
        let mut unknown = order();
        unknown["account"] = json!("not-enrolled");
        assert!(
            adapter
                .plan_exclusive_resources("kraken.add-order", &unknown)
                .is_err()
        );
    }
    #[test]
    fn recovery_amendment_and_nonordinary_shapes_keep_dispatch_relationship_checks() {
        let adapter = adapter();
        for operation in ["kraken.edit-order", "kraken.cancel-order"] {
            assert!(
                adapter
                    .plan_exclusive_resources(operation, &json!({}))
                    .unwrap()
                    .is_none()
            );
        }
        for (key, value) in [
            ("validate", json!(true)),
            ("ordertype", json!("settle-position")),
            ("leverage", json!("2")),
            ("close[ordertype]", json!("limit")),
        ] {
            let mut input = order();
            input[key] = value;
            if key == "close[ordertype]" {
                input["close[price]"] = json!("0.06");
            }
            input.as_object_mut().unwrap().remove("maximumFees");
            assert!(
                adapter
                    .plan_exclusive_resources("kraken.add-order", &input)
                    .unwrap_or_else(|error| panic!("{key}: {error:?}"))
                    .is_none()
            );
        }
    }
}
