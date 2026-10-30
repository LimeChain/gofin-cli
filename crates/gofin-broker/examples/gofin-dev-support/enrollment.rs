//! Example-only enrollment; the installed protocol has no such request variant.
use crate::{DevEnvironment, RuntimeState, build_runtime, dev_credentials, profile};
use gofin_broker::{
    ConnectionConfig,
    enrollment::{public_connection_contract, wallet_generation},
};
use gofin_credentials::CredentialRef;
use gofin_discovery::{
    GenerateConnectionArgs, NamedConnectionArgs, enrollment_contract::EnrollmentOutcome,
};
use gofin_domain::{GoFinError, Result};
use gofin_protocol::BrokerResponse;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    action: Action,
    input: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Generate,
    Remove,
}

fn failure(message: &str) -> GoFinError {
    GoFinError::new(
        "DEVELOPMENT_ENROLLMENT_FAILED",
        "development",
        message,
        "Inspect this isolated environment and retry; production connections are unaffected.",
    )
}

fn name(connection: &ConnectionConfig) -> Result<String> {
    serde_json::to_value(public_connection_contract(connection))
        .map_err(|_| failure("The development connection could not be encoded."))?
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| failure("The development connection has no name."))
}

fn reference(connection: &ConnectionConfig) -> Result<CredentialRef> {
    match connection {
        ConnectionConfig::Evm {
            credential_ref: Some(reference),
            ..
        }
        | ConnectionConfig::Solana {
            credential_ref: Some(reference),
            ..
        } => CredentialRef::new(reference),
        _ => Err(failure(
            "Only generated wallet connections can be removed through this development path.",
        )),
    }
}

pub async fn handle(environment: &DevEnvironment, request: Request) -> BrokerResponse {
    match execute(environment, request).await {
        Ok(value) => BrokerResponse::success(value),
        Err(error) => BrokerResponse::failure(error),
    }
}

async fn execute(environment: &DevEnvironment, request: Request) -> Result<Value> {
    let args = &environment.args;
    let mut state = environment.state.write().await;
    if !state.healthy {
        return Err(failure(
            "Development configuration needs recovery; stop and restart the environment before continuing.",
        ));
    }
    if args.fixture.is_some() || state.config.yolo_fixture.is_some() {
        return Err(failure(
            "Wallet enrollment requires an environment without a selected rehearsal fixture.",
        ));
    }
    crate::reject_import_with_unresolved_records(&args.database)
        .map_err(|_| failure("Wallet enrollment is blocked while an operation is unresolved."))?;
    let credential_dir = args
        .database
        .parent()
        .ok_or_else(|| failure("Invalid development directory."))?
        .join("credentials");
    let mut candidate = profile::EnvironmentConfig::read(&args.config, &args.environment_id)
        .map_err(|_| failure("Development configuration could not be read."))?;
    let (action, connection, checks, staged) = match request.action {
        Action::Generate => {
            let input: GenerateConnectionArgs =
                serde_json::from_value(request.input).map_err(|_| {
                    failure("Wallet generation requires a supported network and a valid name.")
                })?;
            let input = wallet_generation::connection_args(input)?;
            // Include derived/imported connections in the namespace check.
            let existing = profile::broker_config(&candidate)
                .map_err(|_| failure("Development connection profile is invalid."))?;
            if existing
                .connections
                .iter()
                .map(name)
                .collect::<Result<Vec<_>>>()?
                .contains(&input.name)
            {
                return Err(GoFinError::new(
                    "CONNECTION_EXISTS",
                    "configuration",
                    "A connection with that name already exists.",
                    "Choose a different name.",
                ));
            }
            dev_credentials::create_store(&credential_dir)?;
            let material = wallet_generation::generate(input.provider)?;
            let staged = dev_credentials::write_material(&credential_dir, &material)?;
            let (connection, checks, _) =
                match wallet_generation::validate(&input, Some(&staged), Some(&material)) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = dev_credentials::remove_material(&credential_dir, &staged);
                        return Err(error);
                    }
                };
            candidate.generated_connections.push(connection.clone());
            ("generate", connection, checks, Some(staged))
        }
        Action::Remove => {
            let input: NamedConnectionArgs = serde_json::from_value(request.input)
                .map_err(|_| failure("Wallet removal requires a valid connection name."))?;
            gofin_discovery::validate_name(&input.name)?;
            let index = candidate
                .generated_connections
                .iter()
                .position(|connection| name(connection).ok().as_deref() == Some(&input.name))
                .ok_or_else(|| failure("No generated development wallet exists with that name."))?;
            let connection = candidate.generated_connections.remove(index);
            reference(&connection)?;
            ("remove", connection, vec![], None)
        }
    };
    // Build first, while holding exclusive access; a failed rebuild leaves the
    // old published configuration and runtime intact.
    let (runtime, metadata) = match build_runtime(args, &candidate).await {
        Ok(value) => value,
        Err(_) => {
            if let Some(staged) = &staged {
                let _ = dev_credentials::remove_material(&credential_dir, staged);
            }
            return Err(failure(
                "The updated development runtime could not be initialized.",
            ));
        }
    };
    let prepared = RuntimeState {
        healthy: true,
        runtime,
        metadata,
        config: candidate,
    };
    publish_candidate(
        &mut state,
        prepared,
        &args.config,
        &credential_dir,
        staged.as_ref(),
        |config, path| config.write_atomic_durable(path),
    )?;
    let cleanup_warning = if action == "remove" {
        dev_credentials::remove_material(&credential_dir, &reference(&connection)?)
            .err()
            .map(|_| {
                "The connection was removed, but its isolated credential file needs cleanup."
                    .to_owned()
            })
    } else {
        None
    };
    let warnings = if args.mode == "synthetic" {
        vec!["This synthetic environment verifies wallet storage and lifecycle; generated wallets do not enable live financial operations.".into()]
    } else {
        vec![]
    };
    serde_json::to_value(EnrollmentOutcome {
        action: action.into(),
        connection: Some(public_connection_contract(&connection)),
        checks,
        warnings,
        cleanup_warning,
        configuration_authorized: true,
        financial_movement: false,
        future_financial_authority: false,
        next_action: "Run connections list in this same development environment.".into(),
    })
    .map_err(|_| failure("The enrollment result could not be encoded."))
}

fn publish_candidate(
    state: &mut RuntimeState,
    prepared: RuntimeState,
    path: &std::path::Path,
    credential_dir: &std::path::Path,
    staged: Option<&CredentialRef>,
    save: impl FnOnce(&profile::EnvironmentConfig, &std::path::Path) -> std::result::Result<(), String>,
) -> Result<()> {
    if save(&prepared.config, path).is_ok() {
        *state = prepared;
        return Ok(());
    }
    let saved = if path.is_file() {
        profile::EnvironmentConfig::read(path, &prepared.config.environment_id).ok()
    } else {
        None
    };
    let matches = |expected: &profile::EnvironmentConfig| {
        saved.as_ref().is_some_and(|saved| {
            serde_json::to_value(saved).ok() == serde_json::to_value(expected).ok()
        })
    };
    if matches(&prepared.config) {
        // Rename was observed: use the same public configuration immediately,
        // but preserve keys because its crash durability remains uncertain.
        *state = prepared;
    } else if matches(&state.config) {
        if let Some(staged) = staged {
            let _ = dev_credentials::remove_material(credential_dir, staged);
        }
    } else {
        // No trustworthy published state: stop accepting financial requests
        // until startup re-reads and validates the selected environment.
        state.healthy = false;
    }
    Err(failure(
        "Development configuration save was not confirmed. List connections before retrying; stop and restart this environment if unavailable.",
    ))
}

/// Synthetic inventory exposes persisted public identity while explicitly
/// disabling financial signing and publishing no wallet operation definitions.
pub fn synthetic_connections(
    connections: &[ConnectionConfig],
) -> Vec<gofin_domain::ConnectionMetadata> {
    connections.iter().filter_map(|connection| {
        let (name, resource, address, provider, network) = match connection {
            ConnectionConfig::Evm { name, resource, address, .. } => (name, resource, address, "evm", None),
            ConnectionConfig::Solana { name, resource, address, network, .. } => (name, resource, address, "solana", Some(network.clone())),
            _ => return None,
        };
        Some(gofin_domain::ConnectionMetadata {
            name: name.clone(), provider: provider.into(), resource: resource.clone(),
            role: Some("wallet".into()), parent: None, network, address: Some(address.clone()),
            credential_ref: None, signing_enabled: false,
            details: serde_json::json!({"synthetic":true,"signing_enabled":false,"wallet_lifecycle_only":true}),
        })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_broker::BrokerCredentialProvider;
    use serde_json::json;

    async fn environment(directory: &std::path::Path, mode: &str) -> DevEnvironment {
        let args = crate::Args {
            socket: directory.join("broker.sock"),
            database: directory.join("operations.sqlite"),
            config: directory.join("config.json"),
            identity: "test-identity".into(),
            environment_id: "test-environment".into(),
            environment_name: "test".into(),
            build_id: "test-build".into(),
            mode: mode.into(),
            fixture: None,
            import_credentials: None,
            discover_identities: false,
            provider: None,
        };
        crate::prepare_storage(&args).unwrap();
        let config = crate::initialize_config(&args.config, &args.environment_id, mode).unwrap();
        let (runtime, metadata) = build_runtime(&args, &config).await.unwrap();
        DevEnvironment {
            args,
            state: tokio::sync::RwLock::new(RuntimeState {
                healthy: true,
                runtime,
                metadata,
                config,
            }),
        }
    }

    fn request(action: &str, input: Value) -> Request {
        serde_json::from_value(json!({"action":action,"input":input})).unwrap()
    }

    #[tokio::test]
    async fn generated_wallet_lifecycle_persists_signer_identity_and_removes_same_keys() {
        for mode in ["synthetic", "live"] {
            let directory = tempfile::tempdir().unwrap();
            let environment = environment(directory.path(), mode).await;
            for network in ["evm", "solana"] {
                let name = format!("test-{network}");
                let result = execute(
                    &environment,
                    request("generate", json!({"network":network,"name":name})),
                )
                .await
                .unwrap();
                let address = result["connection"]["address"].as_str().unwrap().to_owned();
                assert!(!address.is_empty());
                assert!(!result.to_string().contains("private_key"));
                assert!(!result.to_string().contains("credential_ref"));
                let provider = dev_credentials::DevelopmentCredentialProvider::new(
                    directory.path().join("credentials"),
                )
                .unwrap();
                let saved =
                    profile::EnvironmentConfig::read(&environment.args.config, "test-environment")
                        .unwrap();
                let connection = saved
                    .generated_connections
                    .iter()
                    .find(|c| super::name(c).unwrap() == name)
                    .unwrap();
                let reference = reference(connection).unwrap();
                if network == "evm" {
                    assert_eq!(provider.envelope_signer(&reference).unwrap().0, address);
                } else {
                    assert_eq!(
                        provider.solana_signer(&reference).unwrap().address(),
                        address
                    );
                }
                let (restarted, _) = build_runtime(&environment.args, &saved).await.unwrap();
                let listed = restarted
                    .handle_client_request(gofin_protocol::BrokerRequest::Connections, false)
                    .await;
                assert!(listed.ok);
                assert!(listed.result.unwrap().to_string().contains(&address));
                let duplicate = execute(
                    &environment,
                    request("generate", json!({"network":network,"name":name})),
                )
                .await
                .unwrap_err();
                assert_eq!(duplicate.code, "CONNECTION_EXISTS");
                let removed = execute(&environment, request("remove", json!({"name":name})))
                    .await
                    .unwrap();
                assert_eq!(removed["connection"]["address"], address);
                assert!(
                    !directory
                        .path()
                        .join("credentials")
                        .join(format!("{}.json", reference.as_str()))
                        .exists()
                );
                let saved =
                    profile::EnvironmentConfig::read(&environment.args.config, "test-environment")
                        .unwrap();
                assert!(saved.generated_connections.is_empty());
                assert!(
                    execute(&environment, request("remove", json!({"name":name})))
                        .await
                        .is_err()
                );
            }
        }
    }

    #[tokio::test]
    async fn generation_rejects_bad_requests_before_staging() {
        let directory = tempfile::tempdir().unwrap();
        let environment = environment(directory.path(), "synthetic").await;
        for input in [
            json!({"network":"bitcoin","name":"bad"}),
            json!({"network":"evm","name":"Bad"}),
            json!({"network":"evm","name":"bad","seed":"forbidden"}),
        ] {
            assert!(
                execute(&environment, request("generate", input))
                    .await
                    .is_err()
            );
        }
        assert!(
            environment
                .state
                .read()
                .await
                .config
                .generated_connections
                .is_empty()
        );
    }
    #[tokio::test]
    async fn failed_publication_reconciles_runtime_and_key_cleanup_without_guessing() {
        for phase in ["before", "after", "unreadable"] {
            let directory = tempfile::tempdir().unwrap();
            let environment = environment(directory.path(), "synthetic").await;
            let mut candidate = profile::EnvironmentConfig::empty("test-environment");
            let args = wallet_generation::connection_args(
                serde_json::from_value(json!({"network":"evm","name":"retained"})).unwrap(),
            )
            .unwrap();
            let material = wallet_generation::generate(args.provider).unwrap();
            let store = directory.path().join("credentials");
            dev_credentials::create_store(&store).unwrap();
            let reference = dev_credentials::write_material(&store, &material).unwrap();
            let (connection, _, _) =
                wallet_generation::validate(&args, Some(&reference), Some(&material)).unwrap();
            candidate.generated_connections.push(connection);
            let (runtime, metadata) = build_runtime(&environment.args, &candidate).await.unwrap();
            let prepared = RuntimeState {
                healthy: true,
                runtime,
                metadata,
                config: candidate,
            };
            let mut state = environment.state.write().await;
            assert!(
                publish_candidate(
                    &mut state,
                    prepared,
                    &environment.args.config,
                    &store,
                    Some(&reference),
                    |candidate, path| {
                        if phase == "after" {
                            candidate.write_atomic(path).unwrap();
                        }
                        if phase == "unreadable" {
                            std::fs::write(path, "invalid config").unwrap();
                        }
                        Err("injected persistence failure".into())
                    }
                )
                .is_err()
            );
            assert_eq!(state.healthy, phase != "unreadable");
            assert_eq!(
                store.join(format!("{}.json", reference.as_str())).exists(),
                phase != "before"
            );
            if phase == "after" {
                let response = state
                    .runtime
                    .handle_client_request(gofin_protocol::BrokerRequest::Connections, false)
                    .await;
                assert!(response.result.unwrap().to_string().contains("retained"));
                assert_eq!(state.config.generated_connections.len(), 1);
            } else {
                assert!(state.config.generated_connections.is_empty());
            }
        }
    }

    #[test]
    fn example_enrollment_wire_is_strict_and_absent_from_production_protocol() {
        let valid = json!({"development_enrollment":{"action":"generate","input":{"network":"evm","name":"test"}}});
        assert!(serde_json::from_value::<crate::DevOperation>(valid.clone()).is_ok());
        assert!(serde_json::from_value::<gofin_protocol::BrokerRequest>(valid.clone()).is_err());
        let mut extra = valid;
        extra["extra"] = json!(true);
        assert!(serde_json::from_value::<crate::DevOperation>(extra).is_err());
    }

    #[tokio::test]
    async fn unresolved_submissions_block_generation_and_removal() {
        let directory = tempfile::tempdir().unwrap();
        let environment = environment(directory.path(), "synthetic").await;
        execute(
            &environment,
            request("generate", json!({"network":"evm","name":"existing"})),
        )
        .await
        .unwrap();
        let mut store = gofin_store::Store::open(&environment.args.database).unwrap();
        store.create_request("pending", "pending-request", "pending-fingerprint", "synthetic.uncertain-write", None,
            "unknown", &json!({"id":"pending","request_id":"pending-request","fingerprint":"pending-fingerprint","status":"unknown"}), &gofin_domain::now()).unwrap();
        assert!(
            execute(
                &environment,
                request("generate", json!({"network":"solana","name":"blocked"}))
            )
            .await
            .is_err()
        );
        assert!(
            execute(&environment, request("remove", json!({"name":"existing"})))
                .await
                .is_err()
        );
        assert_eq!(
            environment
                .state
                .read()
                .await
                .config
                .generated_connections
                .len(),
            1
        );
    }
}
