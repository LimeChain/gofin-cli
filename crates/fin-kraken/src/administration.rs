//! Separate administrative authority and bounded export retrieval. Neither uses financial plans.
use crate::{KrakenEndpoint as E, KrakenProfile, KrakenResponse, contracts};
use fin_domain::{FinError, Result};
use fin_exchange::kraken::DispatchGuard;
use serde_json::{Value, json};

fn wrong() -> FinError {
    FinError::new(
        "WRONG_OPERATION_KIND",
        "configuration",
        "This operation is not a supported administrative change.",
        "Use fin configure for administrative changes and fin execute for financial actions.",
    )
}
fn validate_acknowledgment(endpoint: E, input: &Value, response: &Value) -> Result<()> {
    let errors = response.get("error").or_else(|| response.get("errors"));
    if errors.is_some_and(|e| match e {
        Value::Array(a) => !a.is_empty(),
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }) {
        return Err(FinError::new(
            "KRAKEN_CONFIGURATION_REJECTED",
            "configuration",
            "Kraken rejected the configuration attempt.",
            "Inspect the original attempt; do not automatically replay it.",
        ));
    }
    let valid = match endpoint {
        E::KrakenPrivatePostCreateSubaccount => response["result"] == true,
        E::KrakenPrivatePostAddExport => response["result"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        E::KrakenPrivatePostRemoveExport => input["type"]
            .as_str()
            .is_some_and(|kind| response["result"][kind] == true),
        _ if endpoint.futures() => response["result"] == "success",
        _ => response.is_object() && response["result"] != "error" && response["success"] != false,
    };
    if !valid {
        return Err(FinError::new(
            "KRAKEN_CONFIGURATION_ACKNOWLEDGMENT_INVALID",
            "configuration",
            "Kraken did not acknowledge this exact configuration change as successful.",
            "Reconcile the original attempt without replay.",
        ));
    }
    Ok(())
}
impl KrakenProfile {
    /// Read-only venue permission admission precedes fresh configuration review.
    pub async fn preflight_configuration(&self, operation: &str, input: &Value) -> Result<()> {
        self.configuration_binding(operation, input)?;
        let endpoint = contracts::endpoint(operation)?;
        let (selection, _) = contracts::decode(endpoint, input)?;
        let account = self.account(selection.account.as_deref().ok_or_else(wrong)?)?;
        if endpoint.futures() {
            self.derivative_permissions(account).await?;
            return Ok(());
        }
        let required = match endpoint {
            E::KrakenPrivatePostAddExport | E::KrakenPrivatePostRemoveExport => "export-data",
            E::KrakenPrivatePostCreateSubaccount => "withdraw-funds",
            E::KrakenFundingPostFundingV1Addresses
            | E::KrakenFundingPutFundingV1AddressesId
            | E::KrakenFundingDeleteFundingV1AddressesId => "add-withdraw-address",
            _ => return Err(wrong()),
        };
        self.spot_permissions(account, &[required]).await?;
        Ok(())
    }
    pub fn configuration_binding(&self, operation: &str, input: &Value) -> Result<Value> {
        let endpoint = contracts::endpoint(operation)?;
        let contract = contracts::contract(endpoint);
        if contract.client != self.product.client() || !is_configuration(endpoint) {
            return Err(wrong());
        }
        let (selection, parameters) = contracts::decode(endpoint, input)?;
        let account = self.account(selection.account.as_deref().ok_or_else(wrong)?)?;
        Ok(
            json!({"version":1,"action":"configure","financial_movement":false,"future_financial_authority":false,"operation":operation,"product":self.product.client(),"account":account.name,"resource":account.resource,"physical_identity":account.physical_identity,"identity_verified":account.identity_verified,"parameters":parameters,"warning":if endpoint==E::KrakenPrivatePostCreateSubaccount { "Subaccount creation may lack request-correlated recovery after response loss. Venue settings may affect existing orders, risk limits or future withdrawals. This approval grants no Fin financial-plan authority." } else { "Venue settings may affect existing orders, risk limits or future withdrawals. This approval grants no Fin financial-plan authority." }}),
        )
    }
    pub async fn dispatch_configuration(
        &self,
        operation: &str,
        input: &Value,
        guard: DispatchGuard,
    ) -> Result<Value> {
        self.configuration_binding(operation, input)?;
        let endpoint = contracts::endpoint(operation)?;
        let (selection, parameters) = contracts::decode(endpoint, input)?;
        let account = self.account(selection.account.as_deref().ok_or_else(wrong)?)?;
        match self
            .transport
            .request(endpoint, Some(account), parameters, Some(guard))
            .await?
        {
            KrakenResponse::Json(mut value) => {
                validate_acknowledgment(endpoint, input, &value)?;
                super::profile::redact_credentials(&mut value);
                Ok(value)
            }
            KrakenResponse::Artifact { .. } => Err(wrong()),
            KrakenResponse::Rejected { .. } => Err(FinError::validation(
                "Kraken rejected this configuration request without mutation.",
            )),
        }
    }
    /// Only reads after response loss; absence never proves a mutation failed.
    pub async fn observe_configuration(&self, operation: &str, input: &Value) -> Result<Value> {
        self.configuration_binding(operation, input)?;
        let endpoint = contracts::endpoint(operation)?;
        let (selection, parameters) = contracts::decode(endpoint, input)?;
        let account = self.account(selection.account.as_deref().ok_or_else(wrong)?)?;
        let (observer, mut fields) = match endpoint {
            E::KrakenfuturesPrivatePutLeveragepreferences => {
                (E::KrakenfuturesPrivateGetLeveragepreferences, json!({}))
            }
            E::KrakenfuturesPrivatePutPnlpreferences => {
                (E::KrakenfuturesPrivateGetPnlpreferences, json!({}))
            }
            E::KrakenfuturesPrivatePutSelfTradeStrategy => {
                (E::KrakenfuturesPrivateGetSelfTradeStrategy, json!({}))
            }
            E::KrakenfuturesPrivatePutSubaccountUidTradingEnabled => (
                E::KrakenfuturesPrivateGetSubaccountUidTradingEnabled,
                json!({"subaccountUid":parameters["subaccountUid"]}),
            ),
            E::KrakenfuturesPrivatePutRfqAssignmentMaxLeverage
            | E::KrakenfuturesPrivateDeleteRfqAssignmentMaxLeverage => (
                E::KrakenfuturesPrivateGetRfqAssignmentMaxLeverage,
                json!({}),
            ),
            E::KrakenfuturesPrivatePostAssignmentprogramDelete => (
                E::KrakenfuturesPrivateGetAssignmentprogramCurrent,
                json!({}),
            ),
            E::KrakenFundingPostFundingV1Addresses
            | E::KrakenFundingPutFundingV1AddressesId
            | E::KrakenFundingDeleteFundingV1AddressesId => {
                (E::KrakenFundingGetFundingV1Addresses, json!({"limit":500}))
            }
            E::KrakenPrivatePostAddExport => (
                E::KrakenPrivatePostExportStatus,
                json!({"report":parameters["report"]}),
            ),
            _ => {
                return Ok(
                    json!({"level":"unresolved","reason":"This endpoint has no exact read-only request correlation. Do not resubmit."}),
                );
            }
        };
        if observer == E::KrakenFundingGetFundingV1Addresses
            && let Some(account_id) = parameters.get("account_id")
        {
            fields["account_id"] = account_id.clone();
        }
        let mut observed = self.json(observer, Some(account), fields).await?;
        super::profile::redact_credentials(&mut observed);
        // A snapshot is useful evidence but cannot prove that this particular uncertain attempt ran.
        Ok(
            json!({"level":"unresolved","observation":observed,"reason":"Current venue state was read without replay. Request-specific correlation is still required."}),
        )
    }
    pub async fn artifact(&self, operation: &str, input: &Value) -> Result<(String, Vec<u8>)> {
        let endpoint = contracts::endpoint(operation)?;
        if contracts::contract(endpoint).client != self.product.client()
            || !matches!(
                endpoint,
                E::KrakenPrivatePostRetrieveExport | E::KrakenfuturesHistoryGetAccountlogcsv
            )
        {
            return Err(wrong());
        }
        let (selection, parameters) = contracts::decode(endpoint, input)?;
        let account = self.account(selection.account.as_deref().ok_or_else(wrong)?)?;
        match self
            .transport
            .request(endpoint, Some(account), parameters, None)
            .await?
        {
            KrakenResponse::Artifact {
                content_type,
                bytes,
            } if bytes.len() <= 32 * 1024 * 1024 => Ok((content_type, bytes)),
            _ => Err(FinError::new(
                "KRAKEN_ARTIFACT_INVALID",
                "read",
                "Kraken did not return a bounded export artifact.",
                "Check report readiness and retry the read.",
            )),
        }
    }
}
pub fn is_configuration(endpoint: E) -> bool {
    contracts::contract(endpoint).administrative
        || matches!(
            endpoint,
            E::KrakenPrivatePostAddExport | E::KrakenPrivatePostRemoveExport
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KrakenAccount, KrakenProduct, KrakenTransport};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct Fixture {
        calls: Mutex<Vec<(E, Value, bool)>>,
        bad_response: bool,
    }
    #[async_trait]
    impl KrakenTransport for Fixture {
        async fn request(
            &self,
            endpoint: E,
            _: Option<&KrakenAccount>,
            parameters: Value,
            guard: Option<DispatchGuard>,
        ) -> Result<KrakenResponse> {
            let permitted = guard.as_ref().is_none_or(|g| g());
            self.calls
                .lock()
                .unwrap()
                .push((endpoint, parameters, guard.is_some()));
            if !permitted {
                return Err(FinError::validation("synthetic approval expired"));
            }
            if self.bad_response {
                return Ok(KrakenResponse::Json(json!({"result":false,"error":[]})));
            }
            if matches!(
                endpoint,
                E::KrakenPrivatePostRetrieveExport | E::KrakenfuturesHistoryGetAccountlogcsv
            ) {
                Ok(KrakenResponse::Artifact {
                    content_type: "text/csv".into(),
                    bytes: b"synthetic,csv\n".to_vec(),
                })
            } else {
                let mut response = match endpoint {
                    E::KrakenPrivatePostCreateSubaccount => json!({"error":[],"result":true}),
                    E::KrakenPrivatePostAddExport => {
                        json!({"error":[],"result":{"id":"SYNTHETIC-export"}})
                    }
                    E::KrakenPrivatePostRemoveExport => {
                        json!({"error":[],"result":{"cancel":true,"delete":true}})
                    }
                    _ if endpoint.futures() => json!({"result":"success"}),
                    _ => json!({"data":{"id":"SYNTHETIC-address"}}),
                };
                response["api_key"] = "SYNTHETIC-secret".into();
                Ok(KrakenResponse::Json(response))
            }
        }
    }
    fn profile(product: KrakenProduct, fixture: Arc<Fixture>) -> KrakenProfile {
        KrakenProfile::new(
            product,
            vec![KrakenAccount {
                name: "synthetic".into(),
                resource: fin_domain::ResourceId(format!("{}:account:synthetic", product.client())),
                credential_ref: "synthetic-ref".into(),
                physical_identity: "synthetic-physical".into(),
                identity_verified: false,
                wallet_account_id: None,
                default_wallet_account_id: None,
                product,
            }],
            fixture,
        )
        .unwrap()
    }
    #[tokio::test]
    async fn administrative_families_are_exact_separate_and_guarded() {
        let fixture = Arc::new(Fixture::default());
        let spot = profile(KrakenProduct::Spot, fixture.clone());
        let futures = profile(KrakenProduct::Derivatives, fixture.clone());
        let scenarios = [
            (
                E::KrakenPrivatePostCreateSubaccount,
                json!({"email":"synthetic@example.invalid","username":"synthetic"}),
            ),
            (
                E::KrakenPrivatePostAddExport,
                json!({"report":"trades","description":"synthetic"}),
            ),
            (
                E::KrakenPrivatePostRemoveExport,
                json!({"id":"synthetic-report","type":"cancel"}),
            ),
            (
                E::KrakenfuturesPrivatePostAssignmentprogramDelete,
                json!({"id":"1"}),
            ),
            (
                E::KrakenfuturesPrivatePutLeveragepreferences,
                json!({"symbol":"PF_ETHUSD","maxLeverage":"2"}),
            ),
            (
                E::KrakenfuturesPrivatePutPnlpreferences,
                json!({"symbol":"PF_ETHUSD","pnlPreference":"USD"}),
            ),
            (
                E::KrakenfuturesPrivatePutSelfTradeStrategy,
                json!({"strategy":"REJECT_TAKER"}),
            ),
            (
                E::KrakenfuturesPrivatePutSubaccountUidTradingEnabled,
                json!({"subaccountUid":"00000000-0000-0000-0000-000000000001","tradingEnabled":false}),
            ),
            (
                E::KrakenfuturesPrivatePutRfqAssignmentMaxLeverage,
                json!({"maxLeverage":"2"}),
            ),
            (
                E::KrakenfuturesPrivateDeleteRfqAssignmentMaxLeverage,
                json!({}),
            ),
            (
                E::KrakenFundingPostFundingV1Addresses,
                json!({"scope":{"network_id":"00000000-0000-0000-0000-000000000001"},"address_details":{"crypto":{"address":"synthetic-address","memo":"synthetic-memo"}},"name":"synthetic"}),
            ),
            (
                E::KrakenFundingPutFundingV1AddressesId,
                json!({"id":"ABaaaaa-bbbbb-cccccc","description":"synthetic"}),
            ),
            (
                E::KrakenFundingDeleteFundingV1AddressesId,
                json!({"id":"ABaaaaa-bbbbb-cccccc"}),
            ),
        ];
        for (endpoint, mut input) in scenarios {
            let selected = if endpoint.futures() { &futures } else { &spot };
            input["account"] = "synthetic".into();
            let binding = selected
                .configuration_binding(endpoint.operation(), &input)
                .unwrap();
            assert_eq!(binding["future_financial_authority"], false);
            assert_eq!(binding["action"], "configure");
            assert!(
                fin_exchange::ExchangeProfile::prepare(selected, endpoint.operation(), &input)
                    .await
                    .is_err()
            );
            let response = selected
                .dispatch_configuration(endpoint.operation(), &input, Arc::new(|| true))
                .await
                .unwrap();
            assert!(response.get("api_key").is_none());
            input["unknown"] = "unsafe".into();
            assert!(
                selected
                    .configuration_binding(endpoint.operation(), &input)
                    .is_err()
            );
        }
        assert_eq!(fixture.calls.lock().unwrap().len(), 13);
        assert!(fixture.calls.lock().unwrap().iter().all(|c| c.2));
        assert!(
            spot.configuration_binding("kraken.add-order", &json!({"account":"synthetic"}))
                .is_err()
        );
        assert!(
            spot.configuration_binding(
                "krakenfutures.set-self-trade-strategy",
                &json!({"account":"synthetic","strategy":"REJECT_TAKER"})
            )
            .is_err()
        );
        assert!(
            futures
                .dispatch_configuration(
                    "krakenfutures.set-self-trade-strategy",
                    &json!({"account":"synthetic","strategy":"REJECT_TAKER"}),
                    Arc::new(|| false)
                )
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn dispatch_business_failure_is_not_successful_configuration() {
        let profile = profile(
            KrakenProduct::Spot,
            Arc::new(Fixture {
                bad_response: true,
                ..Default::default()
            }),
        );
        let error=profile.dispatch_configuration("kraken.create-subaccount",&json!({"account":"synthetic","username":"synthetic","email":"synthetic@example.invalid"}),Arc::new(||true)).await.unwrap_err();
        assert_eq!(error.code, "KRAKEN_CONFIGURATION_ACKNOWLEDGMENT_INVALID");
    }
    #[test]
    fn business_failure_and_false_success_are_never_acknowledged() {
        assert!(
            validate_acknowledgment(
                E::KrakenPrivatePostCreateSubaccount,
                &json!({}),
                &json!({"error":[],"result":false})
            )
            .is_err()
        );
        assert!(
            validate_acknowledgment(
                E::KrakenPrivatePostAddExport,
                &json!({}),
                &json!({"error":[],"result":{}})
            )
            .is_err()
        );
        assert!(
            validate_acknowledgment(
                E::KrakenPrivatePostRemoveExport,
                &json!({"type":"delete"}),
                &json!({"error":[],"result":{"delete":false,"cancel":true}})
            )
            .is_err()
        );
        assert!(
            validate_acknowledgment(
                E::KrakenfuturesPrivatePutSelfTradeStrategy,
                &json!({}),
                &json!({"result":"error"})
            )
            .is_err()
        );
        assert!(
            validate_acknowledgment(
                E::KrakenFundingPostFundingV1Addresses,
                &json!({}),
                &json!({"errors":["synthetic failure"]})
            )
            .is_err()
        );
        assert!(
            validate_acknowledgment(
                E::KrakenFundingPostFundingV1Addresses,
                &json!({}),
                &json!({"success":false})
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn artifact_reads_have_no_dispatch_approval_or_arbitrary_endpoints() {
        let fixture = Arc::new(Fixture::default());
        let spot = profile(KrakenProduct::Spot, fixture.clone());
        assert_eq!(
            spot.artifact(
                "kraken.retrieve-export",
                &json!({"account":"synthetic","id":"synthetic-report"})
            )
            .await
            .unwrap()
            .1,
            b"synthetic,csv\n"
        );
        assert!(!fixture.calls.lock().unwrap()[0].2);
        assert!(
            spot.artifact("kraken.balance", &json!({"account":"synthetic"}))
                .await
                .is_err()
        );
    }
}
