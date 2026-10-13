//! Primary-source mutation DTO evidence only. No request is dispatched, signed,
//! authorized or settled by these schema/serialization tests.
use crate::{KrakenEndpoint, contracts, inputs};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct Inventory {
    endpoints: Vec<Fixture>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    client: String,
    method: String,
    source_spec: String,
    path: String,
    verb: String,
    source_sha256: String,
    options: Vec<String>,
    option_pointers: Vec<String>,
    query: Vec<String>,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    case: String,
    native: Value,
}
fn inventory() -> Inventory {
    serde_json::from_str(include_str!(
        "../../../docs/kraken/fixtures/mutation-native-options.json"
    ))
    .unwrap()
}
fn endpoint(f: &Fixture) -> KrakenEndpoint {
    *KrakenEndpoint::ALL
        .iter()
        .find(|e| e.method() == f.method && e.futures() == (f.client == "krakenfutures"))
        .unwrap()
}
fn present_pointers(value: &Value, prefix: &str, result: &mut BTreeSet<String>) {
    if !prefix.is_empty() {
        result.insert(prefix.to_owned());
    }
    match value {
        Value::Object(fields) => {
            for (name, child) in fields {
                present_pointers(child, &format!("{prefix}/{name}"), result);
            }
        }
        Value::Array(children) => {
            for child in children {
                present_pointers(child, &format!("{prefix}/*"), result);
            }
        }
        _ => {}
    }
}
fn source(name: &str) -> &'static str {
    match name {
        "spot-rest" => include_str!("../../../docs/kraken/spot-rest.json"),
        "futures-rest" => include_str!("../../../docs/kraken/futures-rest.json"),
        "futures-auth-rest" => include_str!("../../../docs/kraken/futures-auth-rest.json"),
        _ => panic!("unknown primary source"),
    }
}
#[test]
fn every_active_mutation_has_an_independent_primary_source_plan() {
    let mut covered = BTreeSet::new();
    for f in inventory().endpoints {
        let e = endpoint(&f);
        assert!(e.mutating());
        assert_eq!(e.path(), f.path);
        assert_eq!(e.verb(), f.verb);
        assert_eq!(
            hex::encode(Sha256::digest(source(&f.source_spec).as_bytes())),
            f.source_sha256
        );
        let raw: Value = serde_json::from_str(source(&f.source_spec)).unwrap();
        assert!(raw["paths"][&f.path].get(f.verb.to_lowercase()).is_some());
        assert_eq!(
            e.query_parameters()
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>(),
            f.query.iter().cloned().collect()
        );
        assert_eq!(
            f.options.iter().cloned().collect::<BTreeSet<_>>(),
            f.cases
                .iter()
                .flat_map(|c| c.native.as_object().unwrap().keys().cloned())
                .collect()
        );
        let mut pointers = BTreeSet::new();
        for case in &f.cases {
            present_pointers(&case.native, "", &mut pointers);
        }
        let missing: Vec<_> = f
            .option_pointers
            .iter()
            .filter(|pointer| !pointers.contains(*pointer))
            .collect();
        assert!(
            missing.is_empty(),
            "{} source nested options without fixtures: {:?}",
            f.method,
            missing
        );
        assert!(covered.insert(e.method().to_owned()));
    }
    assert_eq!(
        covered,
        KrakenEndpoint::ALL
            .iter()
            .filter(|e| e.mutating())
            .map(|e| e.method().to_owned())
            .collect()
    );
}
#[test]
fn raw_source_mutation_option_variants_decode_validate_and_roundtrip_exactly() {
    let mut failures = vec![];
    let mut checked = 0;
    for f in inventory().endpoints {
        let e = endpoint(&f);
        for c in &f.cases {
            checked += 1;
            if let Err(error) = contracts::validate_native(e, &c.native) {
                failures.push(format!(
                    "{}.{} native: {} {:?}",
                    f.method, c.case, error.code, error.details
                ));
                continue;
            }
            let roundtrip = inputs::decode_wire(e, c.native.clone())
                .unwrap_or_else(|err| panic!("{}.{} DTO {}", f.method, c.case, err.code));
            assert_eq!(
                roundtrip, c.native,
                "{}.{} exact decimals/arrays/null",
                f.method, c.case
            );
            let mut input = c.native.clone();
            input["account"] = json!("SYNTHETIC-NONLIVE-ACCOUNT");
            if !contracts::financial_controls(e).is_empty() {
                input["requestId"] = json!("SYNTHETIC-NONLIVE-REQUEST");
            }
            match contracts::decode(e, &input) {
                Ok((selection, wire)) => {
                    assert_eq!(
                        selection.account.as_deref(),
                        Some("SYNTHETIC-NONLIVE-ACCOUNT")
                    );
                    assert_eq!(wire, c.native);
                }
                Err(err) => failures.push(format!("{}.{} decode: {}", f.method, c.case, err.code)),
            }
        }
    }
    assert!(checked >= 296);
    assert!(
        failures.is_empty(),
        "Independent primary mutation option failures:\n{}",
        failures.join("\n")
    );
}
#[test]
fn mutation_inputs_reject_unknown_passthrough_and_missing_account_without_dispatch() {
    for f in inventory().endpoints {
        let e = endpoint(&f);
        let mut input = f.cases[0].native.clone();
        input["account"] = json!("SYNTHETIC-NONLIVE-ACCOUNT");
        if !contracts::financial_controls(e).is_empty() {
            input["requestId"] = json!("SYNTHETIC-NONLIVE-REQUEST");
        }
        for name in ["nonce", "otp", "path", "parameters", "SYNTHETIC_UNKNOWN"] {
            let mut bad = input.clone();
            bad[name] = json!("SYNTHETIC");
            assert!(contracts::decode(e, &bad).is_err(), "{}.{}", f.method, name);
        }
        if !contracts::financial_controls(e).is_empty() {
            let mut missing_identity = input.clone();
            missing_identity
                .as_object_mut()
                .unwrap()
                .remove("requestId");
            assert!(
                contracts::decode(e, &missing_identity).is_err(),
                "{} immutable request identity required",
                f.method
            );
        }
        input.as_object_mut().unwrap().remove("account");
        assert!(contracts::decode(e, &input).is_err());
    }
}
#[test]
fn legacy_address_generation_requires_explicit_true_and_discovery_cannot_generate() {
    let generate = KrakenEndpoint::KrakenLegacyCreateDepositAddress;
    for value in [json!(false), Value::Null, json!("true")] {
        assert!(
            contracts::validate_native(
                generate,
                &json!({"asset":"ETH","method":"Ethereum","new":value})
            )
            .is_err()
        );
    }
    assert!(
        contracts::validate_native(generate, &json!({"asset":"ETH","method":"Ethereum"})).is_err()
    );
    assert!(
        contracts::validate_native(
            KrakenEndpoint::KrakenPrivatePostDepositAddresses,
            &json!({"asset":"ETH","method":"Ethereum","new":true})
        )
        .is_err()
    );
    for e in KrakenEndpoint::ALL
        .iter()
        .filter(|e| e.method().starts_with("auth_"))
    {
        assert!(
            !e.mutating(),
            "Read-only auth inspection cannot imply key creation/deletion authority"
        );
    }
}
