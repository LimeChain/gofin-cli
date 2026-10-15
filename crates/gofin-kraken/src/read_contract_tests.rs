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
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[derive(Deserialize)]
struct Inventory {
    endpoints: Vec<EndpointFixture>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EndpointFixture {
    client: String,
    method: String,
    source_spec: String,
    path: String,
    verb: String,
    source_sha256: String,
    options: Vec<String>,
    query: Vec<String>,
    cases: Vec<ReadCase>,
    #[serde(default)]
    response_examples: Vec<ResponseExample>,
    #[serde(default)]
    artifact: bool,
}
#[derive(Deserialize)]
struct ReadCase {
    case: String,
    native: Value,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseExample {
    label: String,
    pointer: String,
    business_failure: bool,
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
fn source(name: &str) -> &'static str {
    match name {
        "spot-rest" => include_str!("../../../docs/kraken/spot-rest.json"),
        "futures-rest" => include_str!("../../../docs/kraken/futures-rest.json"),
        "futures-history-rest" => include_str!("../../../docs/kraken/futures-history-rest.json"),
        "futures-charts-rest" => include_str!("../../../docs/kraken/futures-charts-rest.json"),
        "futures-auth-rest" => include_str!("../../../docs/kraken/futures-auth-rest.json"),
        _ => panic!("unknown pinned primary source"),
    }
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
    endpoint: KrakenEndpoint,
    account: Option<KrakenAccount>,
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
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        assert!(
            !endpoint.mutating(),
            "read fixture cannot dispatch a mutation"
        );
        assert!(guard.is_none());
        self.requests.lock().unwrap().push(Captured {
            endpoint,
            account: account.cloned(),
            parameters,
        });
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

/// Independently specified credential filtering for the public source examples.
/// No example is a live private account fixture or a usable credential.
fn safe_public_example(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            // These are credential-bearing fields in the pinned public
            // examples, specified independently of the runtime redactor.
            fields.retain(|key, _| {
                !matches!(
                    key.as_str(),
                    "api_key" | "apiKey" | "cvc" | "secret" | "api_secret"
                )
            });
            for child in fields.values_mut() {
                safe_public_example(child);
            }
        }
        Value::Array(children) => {
            for child in children {
                safe_public_example(child);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn published_official_read_response_examples_preserve_native_data_and_business_errors() {
    let mut checked = 0;
    for fixture in inventory().endpoints {
        let raw_source: Value = serde_json::from_str(source(&fixture.source_spec)).unwrap();
        for example in &fixture.response_examples {
            checked += 1;
            let native = raw_source
                .pointer(&example.pointer)
                .expect("published primary response pointer remains valid")
                .clone();
            let transport = Arc::new(Capture {
                response: Some(native.clone()),
                error: if example.business_failure {
                    Some("KRAKEN_NATIVE_BUSINESS_ERROR")
                } else {
                    None
                },
                ..Default::default()
            });
            let adapter = profile(&fixture.client, transport.clone());
            let result = adapter
                .read(
                    endpoint(&fixture).operation(),
                    &input(&fixture, &fixture.cases[0]),
                )
                .await;
            if example.business_failure {
                assert_eq!(
                    result.unwrap_err().code,
                    "KRAKEN_NATIVE_BUSINESS_ERROR",
                    "{}.{} HTTP-success native rejection mapped by transport",
                    fixture.method,
                    example.label
                );
            } else {
                let result = result.unwrap();
                let mut expected = native;
                safe_public_example(&mut expected);
                assert_eq!(
                    result["data"], expected,
                    "{}.{} native response example decimal/array/object values",
                    fixture.method, example.label
                );
            }
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
        }
    }
    assert_eq!(
        checked, 72,
        "Source-provided example evidence count must be reviewed explicitly"
    );
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

#[test]
fn all_typed_read_paths_and_native_query_placement_match_pinned_primary_sources() {
    let inventory = inventory();
    let mut covered = BTreeSet::new();
    for fixture in &inventory.endpoints {
        let endpoint = endpoint(fixture);
        assert!(!endpoint.mutating());
        assert_eq!(endpoint.path(), fixture.path, "{}", fixture.method);
        assert_eq!(endpoint.verb(), fixture.verb, "{}", fixture.method);
        assert_eq!(
            hex::encode(Sha256::digest(source(&fixture.source_spec).as_bytes())),
            fixture.source_sha256,
            "Source changed; refresh independent read evidence for {}",
            fixture.method
        );
        let raw: Value = serde_json::from_str(source(&fixture.source_spec)).unwrap();
        if fixture.method == "private_get_recentorders" {
            assert!(
                raw["paths"].get(&fixture.path).is_none(),
                "Retired-schema gap must be reevaluated when official support changes"
            );
            assert!(
                include_str!("../../../docs/kraken/krakenfutures-ccxt.json")
                    .contains("private_get_recentorders")
            );
        } else {
            assert!(
                raw["paths"][&fixture.path]
                    .get(fixture.verb.to_lowercase())
                    .is_some(),
                "{} primary source endpoint missing",
                fixture.method
            );
        }
        let actual: BTreeSet<_> = endpoint
            .query_parameters()
            .iter()
            .map(|v| v.to_string())
            .collect();
        assert_eq!(
            actual,
            fixture.query.iter().cloned().collect(),
            "{} native query/body placement",
            fixture.method
        );
        assert!(covered.insert((fixture.client.clone(), fixture.method.clone())));
        let seen: BTreeSet<_> = fixture
            .cases
            .iter()
            .flat_map(|case| case.native.as_object().unwrap().keys().cloned())
            .collect();
        assert_eq!(
            seen,
            fixture.options.iter().cloned().collect(),
            "{} native option has no fixture",
            fixture.method
        );
    }
    let active: BTreeSet<_> = KrakenEndpoint::ALL
        .iter()
        .filter(|e| !e.mutating())
        .map(|e| {
            (
                if e.futures() {
                    "krakenfutures"
                } else {
                    "kraken"
                }
                .to_owned(),
                e.method().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        covered, active,
        "Every typed read requires an explicit primary-source mapping or recorded current-schema gap"
    );
}

#[tokio::test]
async fn every_raw_source_read_option_variant_reaches_protected_transport_exactly() {
    let mut failures = vec![];
    let mut count = 0;
    for fixture in inventory().endpoints {
        if fixture.artifact {
            continue;
        }
        let transport = Arc::new(Capture::default());
        let adapter = profile(&fixture.client, transport.clone());
        let endpoint = endpoint(&fixture);
        for case in &fixture.cases {
            count += 1;
            let outcome = adapter
                .read(endpoint.operation(), &input(&fixture, case))
                .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    failures.push(format!(
                        "{}.{}: {} {}",
                        fixture.method, case.case, error.code, error.message
                    ));
                    continue;
                }
            };
            let requests = transport.requests.lock().unwrap();
            let captured = requests.last().unwrap();
            assert_eq!(captured.endpoint, endpoint);
            assert_eq!(
                captured.parameters, case.native,
                "{}.{} exact nested/decimal/array/null wire value",
                fixture.method, case.case
            );
            assert_eq!(
                captured
                    .account
                    .as_ref()
                    .map(|account| account.name.as_str()),
                if endpoint.private() {
                    Some(if fixture.client == "kraken" {
                        "SYNTHETIC-kraken"
                    } else {
                        "SYNTHETIC-krakenfutures"
                    })
                } else {
                    None
                }
            );
            assert_eq!(
                outcome["data"]["result"]["tinyDecimal"],
                "0.000000000000000001"
            );
            assert_eq!(outcome["provider"], fixture.client);
            assert!(outcome["coverage"].as_str().unwrap().contains("bounded"));
        }
    }
    assert!(count > 600);
    assert!(
        failures.is_empty(),
        "Independent raw-schema cases failed:\n{}",
        failures.join("\n")
    );
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
