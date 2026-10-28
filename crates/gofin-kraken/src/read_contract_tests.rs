//! Request expectations originate in pinned raw official sources, independently
//! of generated GoFin DTO schemas. No fixture uses live credentials or transport.
use crate::{
    KrakenAccount, KrakenEndpoint, KrakenProduct, KrakenProfile, KrakenResponse, KrakenTransport,
};
use async_trait::async_trait;
use gofin_domain::{GoFinError, ResourceId, Result};
use gofin_exchange::{ExchangeProfile, kraken::DispatchGuard};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Deserialize)]
struct Inventory {
    endpoints: Vec<EndpointFixture>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndpointFixture {
    client: String,
    method: String,
    options: Vec<String>,
    cases: Vec<ReadCase>,
    #[serde(default)]
    artifact: bool,
}
#[derive(Deserialize)]
struct ReadCase {
    native: Value,
}
fn inventory() -> Inventory {
    serde_json::from_str(include_str!(
        "../../../docs/kraken/fixtures/read-native-options.json"
    ))
    .unwrap()
}
fn endpoint(fixture: &EndpointFixture) -> KrakenEndpoint {
    *KrakenEndpoint::ALL
        .iter()
        .find(|e| {
            e.method() == fixture.method && e.futures() == (fixture.client == "krakenfutures")
        })
        .expect("independently pinned read endpoint remains available")
}
fn input(fixture: &EndpointFixture, case: &ReadCase) -> Value {
    let mut input = case.native.clone();
    if endpoint(fixture).private() {
        input["account"] = json!(format!("SYNTHETIC-{}", fixture.client));
    }
    input
}
#[derive(Debug)]
struct Captured {
    parameters: Value,
}
#[derive(Default)]
struct Capture {
    requests: Mutex<Vec<Captured>>,
    artifact: bool,
    error: Option<&'static str>,
    response: Option<Value>,
}
#[async_trait]
impl KrakenTransport for Capture {
    async fn request(
        &self,
        endpoint: KrakenEndpoint,
        _account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        assert!(
            !endpoint.mutating(),
            "read fixture cannot dispatch a mutation"
        );
        assert!(guard.is_none());
        self.requests.lock().unwrap().push(Captured { parameters });
        if let Some(code) = self.error {
            return Err(GoFinError::new(
                code,
                "synthetic-read-only",
                "Synthetic venue business failure after successful HTTP response.",
                "Add the exact required read permission to the selected disposable fixture key.",
            ));
        }
        if self.artifact {
            return Ok(KrakenResponse::Artifact {
                content_type: "text/csv".into(),
                bytes: b"SYNTHETIC-CSV-ONLY\n".to_vec(),
            });
        }
        if let Some(response) = &self.response {
            return Ok(KrakenResponse::Json(response.clone()));
        }
        Ok(KrakenResponse::Json(
            json!({"result":{"syntheticFixture":true,"zero":"0","tinyDecimal":"0.000000000000000001","rows":[{"nativeStatus":"pending","quantity":"12345678901234567890.000000000000000001"}]}}),
        ))
    }
}

fn profile(client: &str, transport: Arc<Capture>) -> KrakenProfile {
    let product = if client == "kraken" {
        KrakenProduct::Spot
    } else {
        KrakenProduct::Derivatives
    };
    KrakenProfile::new(
        product,
        vec![KrakenAccount {
            name: format!("SYNTHETIC-{client}"),
            resource: ResourceId(format!("SYNTHETIC-resource-{client}")),
            credential_ref: format!("SYNTHETIC-non-live-credential-{client}"),
            physical_identity: format!("SYNTHETIC-identity-{client}"),
            wallet_account_id: None,
            default_wallet_account_id: None,
            identity_verified: false,
            product,
        }],
        transport,
    )
    .unwrap()
}

#[tokio::test]
async fn typed_reads_reject_missing_accounts_wrong_products_and_unknown_write_fields() {
    for fixture in inventory().endpoints {
        let endpoint = endpoint(&fixture);
        let transport = Arc::new(Capture::default());
        let adapter = profile(&fixture.client, transport.clone());
        let first = &fixture.cases[0];
        let mut bad = input(&fixture, first);
        bad["SYNTHETIC_UNDECLARED_FIELD"] = json!(true);
        assert!(
            adapter.read(endpoint.operation(), &bad).await.is_err(),
            "{} unknown field",
            fixture.method
        );
        if endpoint.private() {
            assert!(
                adapter
                    .read(endpoint.operation(), &first.native)
                    .await
                    .is_err(),
                "{} account required",
                fixture.method
            );
        } else {
            let mut bad = first.native.clone();
            bad["account"] = json!(format!("SYNTHETIC-{}", fixture.client));
            assert!(adapter.read(endpoint.operation(), &bad).await.is_err());
        }
        for forbidden in ["nonce", "otp", "path", "method", "parameters", "new"] {
            // `method` is a documented native funding route selector on
            // several legacy reads; it is never an arbitrary HTTP method.
            if fixture.options.iter().any(|option| option == forbidden) {
                continue;
            }
            let mut bad = input(&fixture, first);
            bad[forbidden] = json!("SYNTHETIC-FORBIDDEN");
            assert!(
                adapter.read(endpoint.operation(), &bad).await.is_err(),
                "{}.{} authority/passthrough excluded",
                fixture.method,
                forbidden
            );
        }
        let wrong = profile(
            if fixture.client == "kraken" {
                "krakenfutures"
            } else {
                "kraken"
            },
            transport.clone(),
        );
        assert!(
            wrong
                .read(endpoint.operation(), &input(&fixture, first))
                .await
                .is_err()
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn authenticated_read_permissions_business_failures_and_artifacts_are_explicit() {
    for code in [
        "KRAKEN_PERMISSION_DENIED",
        "KRAKEN_AUTHENTICATION_FAILED",
        "KRAKEN_RATE_LIMITED",
        "KRAKEN_RESPONSE_TIMEOUT",
    ] {
        let transport = Arc::new(Capture {
            error: Some(code),
            ..Default::default()
        });
        let adapter = profile("kraken", transport.clone());
        assert_eq!(
            adapter
                .read(
                    KrakenEndpoint::KrakenPrivatePostBalance.operation(),
                    &json!({"account":"SYNTHETIC-kraken"})
                )
                .await
                .unwrap_err()
                .code,
            code
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
    for fixture in inventory().endpoints.into_iter().filter(|e| e.artifact) {
        let transport = Arc::new(Capture {
            artifact: true,
            ..Default::default()
        });
        let adapter = profile(&fixture.client, transport.clone());
        for case in &fixture.cases {
            assert_eq!(
                adapter
                    .read(endpoint(&fixture).operation(), &input(&fixture, case))
                    .await
                    .unwrap_err()
                    .code,
                "KRAKEN_ARTIFACT_OUTPUT_REQUIRED"
            );
            assert_eq!(
                transport
                    .requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .parameters,
                case.native
            );
        }
    }
}
