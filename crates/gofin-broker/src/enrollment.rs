//! Protected human configuration path. Financial RPC cannot invoke this module.
use crate::{
    BrokerConfigV2, BrokerCredentialProvider, BrokerPolicy, CONFIG_PATH, ConnectionConfig,
    DeploymentConfig, NetworkConfig, ProtectedCredentialProvider,
    hyperliquid_owner_has_pending_operations, load_protected_config, load_stored_protected_config,
    lock_protected_configuration, open_protected_store, resource_has_pending_operations,
    save_protected_config, strip_derived_hyperliquid_links,
};
use gofin_authorization::{MacOsAuthorizer, validate_installation};
use gofin_binance::{BinanceEndpoint, BinanceRequest, BinanceTransport, CredentialHandle};
use gofin_credentials::{CredentialRef, SecretMaterial, TrustedCredentialStore};
use gofin_discovery::{
    AddConnectionArgs, BinanceRole, CredentialType, NamedConnectionArgs, Provider, validate_add,
    validate_name,
};
use gofin_domain::{AuthorizationChallenge, GoFinError, ResourceId, Result, binding_hash, now};
use gofin_human_terminal::{HumanTerminal, SecretInput};
use gofin_presentation::Ui;
#[path = "enrollment_presentation.rs"]
mod presentation;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::Arc,
};

fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "enrollment",
        message,
        "Inspect connections and unresolved operations. Run enrollment yourself in a fresh human Terminal; never paste credentials in chat.",
    )
}
fn binance_enrollment_error(code: &str, cause: gofin_binance::TransportError) -> GoFinError {
    use gofin_binance::TransportError;
    // Transport errors contain only fixed classifications and numeric codes,
    // never upstream messages, response bodies, signed URLs, or credentials.
    let mut message = match &cause {
        TransportError::PermissionDenied => {
            "The protected Binance credential could not be loaded or used for signing".into()
        }
        _ => cause.to_string(),
    };
    if let TransportError::HttpFailure {
        status,
        binance_code,
        ..
    } = cause
    {
        message.push_str(&format!(" [HTTP {status}"));
        if let Some(code) = binance_code {
            message.push_str(&format!("; Binance code {code}"));
        }
        message.push(']');
    }
    error(code, &message)
}
fn parse<T: for<'a> Deserialize<'a>>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| {
        error(
            "INVALID_ENROLLMENT_REQUEST",
            "Enrollment accepts only the documented safe metadata fields.",
        )
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialize {
    approver_uid: u32,
}

pub async fn handle(action: &str, request: Value) -> Result<Value> {
    handle_with_ui(action, request, &Ui::new(false, false)).await
}

pub async fn handle_with_ui(action: &str, request: Value, ui: &Ui) -> Result<Value> {
    validate_installation()?;
    if action == "initialize" {
        return initialize(parse(request)?, ui).await;
    }
    if action == "configure" {
        return configure_human_operation(parse(request)?, ui).await;
    }
    let _lock = lock_protected_configuration(true)?;
    let mut config = load_stored_protected_config()?;
    let effective_config = enrollment_config_for_action(action, config.clone())?;
    let mut terminal = HumanTerminal::open(config.approver_uid)?;
    let journal = open_protected_store()?;
    if action == "policy_set" {
        terminal.finish()?;
        let args: gofin_discovery::PolicySetArgs = parse(request)?;
        return crate::policy::update(
            &config,
            &journal,
            &args,
            |binding| async move {
                approve_and_audit(config.approver_uid, &binding, "Change financial policy", ui)
                    .await
            },
            save_protected_config,
        )
        .await;
    }

    for connection in &effective_config.connections {
        let pending_hyperliquid_signer = match connection {
            ConnectionConfig::Evm { address, .. } => {
                hyperliquid_owner_has_pending_operations(&journal, address)?
            }
            ConnectionConfig::Hyperliquid { owner, .. } => {
                hyperliquid_owner_has_pending_operations(&journal, owner)?
            }
            _ => false,
        };
        if pending_hyperliquid_signer
            || connection_resources(connection, &effective_config)
                .iter()
                .map(|resource| resource_has_pending_operations(&journal, resource))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .any(|pending| pending)
        {
            return Err(error(
                "CONNECTION_HAS_PENDING_OPERATION",
                "Reconcile unresolved financial operations before changing connections.",
            ));
        }
    }
    crate::kraken_aliases::retain(&mut config)?;
    let store = Arc::new(TrustedCredentialStore::open()?);
    recover_inactive_credentials(&store, &config)?;
    match action {
        "add" | "update" => {
            let mut args: AddConnectionArgs = parse(request)?;
            validate_add(&args)?;
            let mut configuration_warnings = Vec::new();
            if args.provider == Provider::Evm {
                configuration_warnings = expand_reviewed_evm_coverage(&mut config);
            }
            let existing = config
                .connections
                .iter()
                .position(|connection| name(connection) == args.name);
            if (action == "add" && existing.is_some()) || (action == "update" && existing.is_none())
            {
                return Err(error(
                    "CONNECTION_NAME_CONFLICT",
                    "The name is already enrolled or the selected update target is missing.",
                ));
            }
            let old = existing.map(|index| config.connections[index].clone());
            preserve_default_wallet(&mut args, old.as_ref())?;
            if args.provider == Provider::Kraken {
                crate::kraken_aliases::preserve(
                    &config,
                    args.account_id.as_deref().unwrap_or_default(),
                    &mut args.default_wallet_account_id,
                )?;
            }
            let material = collect_material(&mut terminal, &args)?;
            // Restore the human terminal before storing, validating or approving
            // the candidate. A failed restoration cannot report enrollment success.
            terminal.finish()?;
            let staged = material
                .as_ref()
                .map(|material| store.put(material))
                .transpose()?;
            let result=async {
                let (connection, checks, mut warnings) = validate_connection(
                    &args,
                    &effective_config,
                    store.clone(),
                    staged.as_ref(),
                    material.as_ref(),
                    ui,
                )
                .await?;
                if let ConnectionConfig::Evm {
                    address,
                    credential_ref: evm_reference,
                    signing_enabled: true,
                    ..
                } = &connection
                    && let Some(ConnectionConfig::Hyperliquid {
                        credential_ref: hl_reference,
                        signing_enabled: hl_signing,
                        ..
                    }) = effective_config.connections.iter().find(|candidate| matches!(
                        candidate,
                        ConnectionConfig::Hyperliquid {
                            owner,
                            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
                            ..
                        } if owner.eq_ignore_ascii_case(address)
                    ))
                    && (!hl_signing || hl_reference != evm_reference)
                {
                    warnings.push(if !hl_signing {
                        "A read-only Hyperliquid owner connection already uses this wallet. GoFin preserved that enrollment and did not enable signing on it.".into()
                    } else {
                        "A separately enrolled Hyperliquid signer already uses this wallet. GoFin preserved its credential mapping instead of rebinding it to the EVM credential.".into()
                    });
                }
                warnings.extend(configuration_warnings.clone());
                if let Some(index)=existing {
                    if immutable_identity(&config.connections[index])!=immutable_identity(&connection) {
                        return Err(error("CONNECTION_RESOURCE_CHANGED","Credential replacement must preserve provider, physical identity, network and account relationships. Enroll a separately named connection for a different identity."));
                    }
                    config.connections[index]=connection.clone();
                } else { config.connections.push(connection.clone()); }
                crate::kraken_aliases::retain(&mut config)?;
                config.validate()?;
                // Validate the runtime-only aliases before approval and save.
                // Otherwise a newly enrolled wallet could persist successfully
                // but make the next protected runtime load fail on a collision.
                effective_protected_config(config.clone())?;
                let binding=json!({"action":action,"connection":connection,"checks":checks,"warnings":warnings,
                    "configuration_hash":config_hash(&config)?,"financial_movement":false,"future_financial_authority":false});
                approve_and_audit(config.approver_uid,&binding,&format!("{action} connection {}",args.name),ui).await?;
                { let _progress=ui.progress("Saving connection"); save_protected_config(&config)?; }
                let retained_references = referenced(&config);
                let cleanup_warning=old.as_ref().and_then(credential).filter(|previous|Some(previous.as_str())!=staged.as_ref().map(CredentialRef::as_str))
                    .filter(|previous| !retained_references.contains(previous.as_str()))
                    .and_then(|reference|store.remove(&reference).err()).map(|_|"Connection is active; an obsolete protected credential needs cleanup at the next human enrollment.");
                Ok(outcome(action,Some(&connection),checks,warnings,cleanup_warning))
            }.await;
            // Rename+directory-sync can report failure after activation. Read back
            // before deleting a candidate; preserve its key if outcome is uncertain.
            if result.is_err()
                && let Some(reference) = &staged
            {
                match load_stored_protected_config() {
                    Ok(observed) if referenced(&observed).contains(reference.as_str()) => {
                        return Err(error(
                            "ENROLLMENT_OUTCOME_UNCERTAIN",
                            "Configuration may already be active. Inspect connections; its protected key was retained for recovery.",
                        ));
                    }
                    Ok(_) => {
                        store.remove(reference).map_err(|_|error("ENROLLMENT_CLEANUP_REQUIRED","No active connection was saved, but the staging credential could not be removed."))?;
                    }
                    Err(_) => {
                        return Err(error(
                            "ENROLLMENT_OUTCOME_UNCERTAIN",
                            "Configuration state could not be read back. The protected staging key was retained; inspect installation before retrying.",
                        ));
                    }
                }
            }
            result
        }
        "remove" => {
            terminal.finish()?;
            let args: NamedConnectionArgs = parse(request)?;
            validate_name(&args.name)?;
            let index = config
                .connections
                .iter()
                .position(|connection| name(connection) == args.name)
                .ok_or_else(|| {
                    error(
                        "CONNECTION_NOT_FOUND",
                        "The named connection is not enrolled.",
                    )
                })?;
            if config.connections.iter().any(|connection|matches!(connection,ConnectionConfig::Binance{parent:Some(parent),..} if parent==&args.name)) {
                return Err(error("CONNECTION_HAS_CHILDREN","Remove or re-enroll dependent child connections before removing their main connection."));
            }
            let removed = config.connections.remove(index);
            config.validate()?;
            // Removing an explicit owner mapping may expose the derived EVM
            // alias. Check that transition before requesting approval.
            effective_protected_config(config.clone())?;
            let binding = json!({"action":"remove","connection":removed,"configuration_hash":config_hash(&config)?,
                "warning":"Keep independent recovery credentials for any remaining external funds or positions.","financial_movement":false,"future_financial_authority":false});
            approve_and_audit(
                config.approver_uid,
                &binding,
                &format!("Remove connection {}", args.name),
                ui,
            )
            .await?;
            save_protected_config(&config)?;
            let retained_references = referenced(&config);
            let cleanup_warning=credential(&removed)
                .filter(|reference| !retained_references.contains(reference.as_str()))
                .and_then(|reference|store.remove(&reference).err())
                .map(|_|"Connection removal is active; an obsolete protected credential needs cleanup at the next human enrollment.");
            Ok(outcome(
                "remove",
                Some(&removed),
                vec![],
                vec![],
                cleanup_warning,
            ))
        }
        _ => {
            terminal.finish()?;
            Err(error(
                "UNKNOWN_ENROLLMENT_ACTION",
                "The protected enrollment action is not supported.",
            ))
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HumanConfigurationRequest {
    request_id: String,
    operation: String,
    input: Value,
    run_id: Option<String>,
}

struct HumanConfigurationAuthorizer {
    approver_uid: u32,
    ui: Ui,
}

#[async_trait::async_trait]
impl gofin_engine::ConfigurationAuthorizer for HumanConfigurationAuthorizer {
    async fn authorize_configuration(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<gofin_domain::ApprovalAudit> {
        let proof = HumanTerminal::open(self.approver_uid)?.review_configuration(
            challenge,
            &gofin_presentation::safe_text(&challenge.summary),
        )?;
        let _progress = self.ui.progress("Waiting for Touch ID");
        MacOsAuthorizer::protected(self.approver_uid)?
            .authorize_reviewed_configuration(challenge, proof)
            .await
    }
}

async fn configure_human_operation(request: HumanConfigurationRequest, ui: &Ui) -> Result<Value> {
    if request.run_id.is_some() {
        return Err(error(
            "CONFIGURATION_ROUTE_INVALID",
            "Configuration actions cannot be attached to a financial run or plan.",
        ));
    }
    if crate::configure::is_kraken_operation(&request.operation) {
        return crate::configure::handle_with_ui(
            gofin_protocol::ConfigureRequest {
                request_id: request.request_id,
                operation: request.operation,
                input: request.input,
            },
            ui,
        )
        .await;
    }
    let _configuration_lock = lock_protected_configuration(false)?;
    let config = load_protected_config()?;
    let credentials = Arc::new(ProtectedCredentialProvider::open()?);
    let applications = crate::build_protected_applications(&config, credentials.clone())?;
    let authorizer = Arc::new(MacOsAuthorizer::protected(config.approver_uid)?);
    let runtime = crate::compose_runtime(
        config.clone(),
        crate::RuntimeDependencies {
            store: open_protected_store()?,
            adapters: applications.adapters,
            authorizer,
            valuator: applications.valuator,
            bridge: applications.bridge,
        },
    )?;
    let record = runtime
        .engine
        .execute_configuration(
            gofin_domain::OperationRequest {
                request_id: request.request_id,
                operation: request.operation,
                input: request.input,
                run_id: None,
            },
            Arc::new(HumanConfigurationAuthorizer {
                approver_uid: config.approver_uid,
                ui: Ui::new(false, ui.is_verbose()),
            }),
        )
        .await?;
    serde_json::to_value(record).map_err(|_| {
        error(
            "CONFIGURATION_RESULT_INVALID",
            "The operation result could not be encoded.",
        )
    })
}

fn collect_setup_policy(mut read: impl FnMut(&str) -> Result<String>) -> Result<BrokerPolicy> {
    let movement = read(
        "Movement limit in USD, including maximum execution costs (0 = no limit; Enter = 100)",
    )?;
    let allocation = read(
        "Rolling 24-hour gross allocation in USD: movement + maximum costs + additional borrowing across all commands (0 = no limit; Enter = 100)",
    )?;
    let args = gofin_discovery::PolicySetArgs {
        max_movement_usd: if movement.is_empty() {
            "100".into()
        } else {
            movement
        },
        max_run_allocation_usd: if allocation.is_empty() {
            "100".into()
        } else {
            allocation
        },
        allow_unpriced_exchange_assets: None,
    };
    let (movement, allocation) = args.micros()?;
    let mut policy = BrokerPolicy::beta();
    policy.max_movement_usd_micros = movement;
    policy.max_run_allocation_usd_micros = allocation;
    Ok(policy)
}

/// Canonical first-install configuration, shared with the protected review.
fn setup_configuration(approver_uid: u32, policy: BrokerPolicy) -> BrokerConfigV2 {
    BrokerConfigV2 {
        version: 2,
        approver_uid,
        networks: NetworkConfig::reviewed_evm_networks(),
        deployments: DeploymentConfig::reviewed_amm_networks(),
        connections: vec![],
        policy,
        extensions: BTreeMap::new(),
    }
}

async fn initialize(args: Initialize, ui: &Ui) -> Result<Value> {
    let _lock = lock_protected_configuration(true)?;
    let mut terminal = HumanTerminal::open(args.approver_uid)?;
    if Path::new(CONFIG_PATH).try_exists().map_err(|_| {
        error(
            "CONFIG_UNAVAILABLE",
            "The installation configuration could not be inspected.",
        )
    })? {
        return Err(error(
            "INSTALLATION_ALREADY_CONFIGURED",
            "Setup cannot replace an existing journal or configuration. Use connection management or a reviewed upgrade.",
        ));
    }
    let policy = collect_setup_policy(|label| terminal.read_public_line(label))?;
    terminal.finish()?;
    let config = setup_configuration(args.approver_uid, policy);
    config.validate()?;
    let binding = json!({"action":"initialize","configuration":config,"installs":["compiled gofin CLI","protected root broker","native Touch ID helper","launchd socket activation","private configuration, credentials, and durable journal"],
        "financial_movement":false,"future_financial_authority":false});
    approve_and_audit(
        args.approver_uid,
        &binding,
        "Initialize protected GoFin configuration",
        ui,
    )
    .await?;
    save_protected_config(&config)?;
    let _journal = open_protected_store()?;
    let _credentials = TrustedCredentialStore::open()?;
    Ok(outcome("initialize", None, vec![], vec![], None))
}
fn collect_material(
    terminal: &mut HumanTerminal,
    args: &AddConnectionArgs,
) -> Result<Option<SecretMaterial>> {
    if args.provider == Provider::Hyperliquid
        && matches!(
            args.hyperliquid_scope,
            Some(
                gofin_discovery::HyperliquidScopeKind::Subaccount
                    | gofin_discovery::HyperliquidScopeKind::Vault
            )
        )
    {
        // A delegated alias reuses the master signer's existing protected key.
        return Ok(None);
    }
    let material = match args.provider {
        Provider::Kraken | Provider::KrakenFutures => {
            let api_key = terminal
                .read_secret("Kraken API key", SecretInput::Line)?
                .to_string();
            let secret = terminal
                .read_secret(
                    "Kraken base64 API secret (API-level 2FA keys are unsupported)",
                    SecretInput::Line,
                )?
                .to_string();
            if args.provider == Provider::Kraken {
                SecretMaterial::KrakenApi { api_key, secret }
            } else {
                SecretMaterial::KrakenFuturesApi { api_key, secret }
            }
        }
        Provider::CoinGecko => SecretMaterial::CoinGeckoApiKey {
            api_key: terminal
                .read_secret("CoinGecko API key", SecretInput::Line)?
                .to_string(),
            plan: args.api_plan.as_deref().unwrap_or("demo").into(),
        },
        Provider::Coinbase => {
            let api_key = terminal
                .read_secret("Coinbase CDP API key ID", SecretInput::Line)?
                .to_string();
            if args.credential_type == Some(CredentialType::Ecdsa) {
                SecretMaterial::CoinbaseCdpEcdsa {
                    api_key,
                    private_key_pem: terminal
                        .read_secret("Coinbase P-256 ECDSA private key PEM", SecretInput::Pem)?
                        .to_string(),
                }
            } else {
                SecretMaterial::CoinbaseCdpEd25519 {
                    api_key,
                    private_key_base64: terminal
                        .read_secret("Coinbase Ed25519 base64 private key", SecretInput::Line)?
                        .to_string(),
                }
            }
        }
        Provider::CoinbaseExchange | Provider::CoinbaseInternational => {
            let api_key = terminal
                .read_secret("Coinbase API key", SecretInput::Line)?
                .to_string();
            let secret = terminal
                .read_secret("Coinbase base64 HMAC secret", SecretInput::Line)?
                .to_string();
            let passphrase = terminal
                .read_secret("Coinbase API passphrase", SecretInput::Line)?
                .to_string();
            if args.provider == Provider::CoinbaseExchange {
                SecretMaterial::CoinbaseExchangeHmac {
                    api_key,
                    secret,
                    passphrase,
                }
            } else {
                SecretMaterial::CoinbaseInternationalHmac {
                    api_key,
                    secret,
                    passphrase,
                }
            }
        }
        Provider::Jupiter => {
            let api_key =
                terminal.read_secret("Jupiter API key (service access only)", SecretInput::Line)?;
            if !gofin_jupiter::valid_api_key(&api_key) {
                return Err(GoFinError::validation(
                    "The Jupiter API key has an invalid format.",
                ));
            }
            SecretMaterial::JupiterApiKey {
                api_key: api_key.to_string(),
            }
        }
        Provider::Relay => SecretMaterial::RelayApiKey {
            api_key: terminal
                .read_secret(
                    "Relay API key (quote service access only)",
                    SecretInput::Line,
                )?
                .to_string(),
        },
        Provider::Binance => {
            let api_key = terminal.read_secret("Binance API key", SecretInput::Line)?;
            if matches!(
                args.credential_type,
                Some(CredentialType::Rsa | CredentialType::Ed25519)
            ) {
                let ed25519 = args.credential_type == Some(CredentialType::Ed25519);
                let key = terminal.read_secret(
                    if ed25519 {
                        "Binance Ed25519 PKCS#8 PEM key"
                    } else {
                        "Binance RSA PEM key"
                    },
                    SecretInput::Pem,
                )?;
                if ed25519 {
                    SecretMaterial::BinanceEd25519 {
                        api_key: api_key.to_string(),
                        private_key: key.to_string(),
                    }
                } else {
                    SecretMaterial::BinanceRsa {
                        api_key: api_key.to_string(),
                        private_key: key.to_string(),
                    }
                }
            } else {
                let secret = terminal.read_secret("Binance HMAC secret", SecretInput::Line)?;
                SecretMaterial::BinanceHmac {
                    api_key: api_key.to_string(),
                    secret: secret.to_string(),
                }
            }
        }
        Provider::Evm if args.credential_type == Some(CredentialType::ReadOnly) => {
            return Ok(None);
        }
        Provider::Evm => SecretMaterial::EvmPrivateKey {
            private_key: terminal
                .read_secret("EVM private key", SecretInput::Line)?
                .to_string(),
        },
        Provider::Hyperliquid if args.credential_type == Some(CredentialType::ReadOnly) => {
            return Ok(None);
        }
        Provider::Hyperliquid => SecretMaterial::EvmPrivateKey {
            private_key: terminal
                .read_secret("Hyperliquid owner EVM private key", SecretInput::Line)?
                .to_string(),
        },
        Provider::Solana if args.credential_type == Some(CredentialType::ReadOnly) => {
            return Ok(None);
        }
        Provider::Solana => SecretMaterial::SolanaPrivateKey {
            private_key: terminal
                .read_secret(
                    "Solana keypair (base58 or JSON [u8; 64])",
                    SecretInput::Line,
                )?
                .to_string(),
        },
    };
    material.validate_present()?;
    Ok(Some(material))
}

async fn coinbase_read(
    client: &gofin_exchange::coinbase::CoinbaseTransport,
    endpoint: gofin_exchange::coinbase::CoinbaseEndpoint,
) -> Result<Value> {
    use gofin_exchange::coinbase::CoinbaseApi;
    let response = client
        .request(
            endpoint,
            gofin_exchange::coinbase::CoinbaseRequest::get(),
            None,
        )
        .await
        .map_err(|_| {
            error(
                "COINBASE_AUTHENTICATION_FAILED",
                "Coinbase rejected the authenticated read-only credential check.",
            )
        })?;
    if !(200..300).contains(&response.status) {
        return Err(error(
            "COINBASE_AUTHENTICATION_FAILED",
            "Coinbase rejected the authenticated read-only credential check.",
        ));
    }
    serde_json::from_slice(&response.body).map_err(|_| {
        error(
            "COINBASE_RESPONSE_INVALID",
            "Coinbase returned invalid account metadata.",
        )
    })
}

fn coinbase_ids(value: &Value, keys: &[&str]) -> Vec<String> {
    let rows = value
        .as_array()
        .or_else(|| value.get("portfolios").and_then(Value::as_array))
        .or_else(|| value.get("accounts").and_then(Value::as_array))
        .or_else(|| value.get("data").and_then(Value::as_array));
    rows.into_iter()
        .flatten()
        .filter_map(|row| {
            keys.iter()
                .find_map(|key| row.get(*key).and_then(Value::as_str))
        })
        .map(str::to_owned)
        .collect()
}

/// Refresh shipped RPC defaults and add missing reviewed chain and AMM coverage during the human's
/// protected EVM enrollment. Existing deployment choices remain authoritative.
fn expand_reviewed_evm_coverage(config: &mut BrokerConfigV2) -> Vec<String> {
    let reviewed_networks = NetworkConfig::reviewed_evm_networks();
    let reviewed_amms = DeploymentConfig::reviewed_amm_networks();
    expand_reviewed_evm_coverage_with(config, &reviewed_networks, &reviewed_amms)
}

fn expand_reviewed_evm_coverage_with(
    config: &mut BrokerConfigV2,
    reviewed_networks: &[NetworkConfig],
    reviewed_amms: &[DeploymentConfig],
) -> Vec<String> {
    refresh_reviewed_rpc_defaults(config, reviewed_networks);
    let mut warnings = Vec::new();
    for network in reviewed_networks {
        if !config
            .networks
            .iter()
            .any(|existing| existing.chain_id == network.chain_id)
        {
            config.networks.push(network.clone());
            warnings.push(format!(
                "Added reviewed EVM network {} ({}, chain {}).",
                gofin_networks::by_chain_id(network.chain_id)
                    .map(|descriptor| descriptor.display_name)
                    .unwrap_or("reviewed network"),
                network.id,
                network.chain_id
            ));
        }
    }

    for reviewed in reviewed_amms {
        if reviewed.uniswap.is_none()
            && reviewed.ramses.is_none()
            && reviewed.v3_deployments.is_empty()
        {
            continue;
        }
        let Some(index) = config
            .deployments
            .iter()
            .position(|existing| existing.network == reviewed.network)
        else {
            config.deployments.push(DeploymentConfig {
                aave_markets: reviewed.aave_markets.clone(),
                network: reviewed.network.clone(),
                weth: reviewed.weth.clone(),
                snapshot_tokens: Vec::new(),
                uniswap: reviewed.uniswap.clone(),
                ramses: reviewed.ramses.clone(),
                v3_deployments: reviewed.v3_deployments.clone(),
                aave: reviewed.aave.clone(),
            });
            if reviewed.uniswap.is_some() {
                warnings.push(format!(
                    "Added reviewed Uniswap v3 and v4 deployment coverage for {}.",
                    reviewed.network
                ));
            }
            if reviewed.ramses.is_some() {
                warnings.push(format!(
                    "Added reviewed Ramses v3 deployment coverage for {}.",
                    reviewed.network
                ));
            }
            for dex in reviewed.v3_deployments.keys() {
                warnings.push(format!(
                    "Added reviewed {} v3 deployment coverage for {}.",
                    dex.id(),
                    reviewed.network
                ));
            }
            continue;
        };

        let existing = &mut config.deployments[index];
        for market in &reviewed.aave_markets {
            if !existing
                .aave_markets
                .iter()
                .any(|m| m.pool.eq_ignore_ascii_case(&market.pool))
            {
                existing.aave_markets.push(market.clone());
            }
        }
        if reviewed.network != "ethereum:8453" && existing.aave.is_none() {
            existing.aave = reviewed.aave.clone();
        }
        if reviewed.network == "ethereum:8453" {
            if existing.weth.is_none() {
                existing.weth = reviewed.weth.clone();
                warnings.push("Added reviewed Base WETH deployment coverage.".into());
            }
            if existing.aave.is_none() {
                existing.aave = reviewed.aave.clone();
                warnings.push("Added reviewed Base Aave deployment coverage.".into());
            }
        }
        if let Some(reviewed_uniswap) = reviewed.uniswap.as_ref() {
            match existing.uniswap.as_mut() {
                None => {
                    existing.uniswap = Some(reviewed_uniswap.clone());
                    warnings.push(format!(
                        "Added reviewed Uniswap v3 and v4 deployment coverage for {}.",
                        reviewed.network
                    ));
                }
                Some(uniswap) if uniswap.v4.is_none() && reviewed_uniswap.v4.is_some() => {
                    uniswap.v4 = reviewed_uniswap.v4.clone();
                    warnings.push(format!(
                        "Added reviewed Uniswap v4 deployment coverage for {} while preserving the existing v3 deployment.",
                        reviewed.network
                    ));
                }
                Some(_) => {}
            }
        }
        for (dex, deployment) in &reviewed.v3_deployments {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                existing.v3_deployments.entry(*dex)
            {
                entry.insert(deployment.clone());
                warnings.push(format!(
                    "Added reviewed {} v3 deployment coverage for {}.",
                    dex.id(),
                    reviewed.network
                ));
            }
        }
        if existing.ramses.is_none() && reviewed.ramses.is_some() {
            existing.ramses = reviewed.ramses.clone();
            warnings.push(format!(
                "Added reviewed Ramses v3 deployment coverage for {}.",
                reviewed.network
            ));
        }
    }
    warnings
}

/// Derive the effective protected configuration from the stored config and the
/// reviewed coverage shipped by this broker. This never persists configuration
/// or touches credentials, and legacy chain-scoped Ethereum connections do not
/// opt in to the reusable-wallet coverage.
pub(crate) fn effective_protected_config(
    config: BrokerConfigV2,
) -> gofin_domain::Result<BrokerConfigV2> {
    let reviewed_networks = NetworkConfig::reviewed_evm_networks();
    let reviewed_amms = DeploymentConfig::reviewed_amm_networks();
    effective_protected_config_with(config, &reviewed_networks, &reviewed_amms)
}

fn enrollment_config_for_action(
    action: &str,
    stored: BrokerConfigV2,
) -> gofin_domain::Result<BrokerConfigV2> {
    match effective_protected_config(stored.clone()) {
        Ok(effective) => Ok(effective),
        Err(error) if action == "remove" && error.code == "HYPERLIQUID_AUTO_LINK_COLLISION" => {
            // A stale/conflicting alias must not prevent the human from
            // removing the conflicting enrolled row. The caller still checks
            // every raw connection's pending resources and validates the
            // resulting effective config before approval.
            stored.validate()?;
            let mut recovery = stored;
            expand_reviewed_evm_coverage(&mut recovery);
            recovery.validate()?;
            Ok(recovery)
        }
        Err(error) => Err(error),
    }
}

/// Replace only an exact previously shipped public RPC list. This changes
/// transport defaults, not network scope, fee policy or pending identities.
fn refresh_reviewed_rpc_defaults(config: &mut BrokerConfigV2, reviewed: &[NetworkConfig]) {
    for network in &mut config.networks {
        let Some(descriptor) = gofin_networks::by_chain_id(network.chain_id) else {
            continue;
        };
        if descriptor.previous_rpc_urls.is_empty()
            || !network
                .rpc_urls
                .iter()
                .map(String::as_str)
                .eq(descriptor.previous_rpc_urls.iter().copied())
        {
            continue;
        }
        if let Some(current) = reviewed
            .iter()
            .find(|current| current.chain_id == network.chain_id)
        {
            network.rpc_urls = current.rpc_urls.clone();
        }
    }
}

pub(crate) fn effective_protected_config_with(
    mut config: BrokerConfigV2,
    reviewed_networks: &[NetworkConfig],
    reviewed_amms: &[DeploymentConfig],
) -> gofin_domain::Result<BrokerConfigV2> {
    strip_derived_hyperliquid_links(&mut config);
    config.validate()?;
    refresh_reviewed_rpc_defaults(&mut config, reviewed_networks);
    let reusable_wallet_enrolled = config
        .connections
        .iter()
        .any(|connection| matches!(connection, ConnectionConfig::Evm { .. }));
    if reusable_wallet_enrolled {
        expand_reviewed_evm_coverage_with(&mut config, reviewed_networks, reviewed_amms);
        link_reusable_evm_wallets_to_hyperliquid(&mut config)?;
    }
    config.validate()?;
    Ok(config)
}

/// Derive one owner-scope Hyperliquid account view for each reusable EVM
/// wallet. The aliases live only in effective runtime configuration: protected
/// enrollment reads and writes the persisted config separately, so removing or
/// replacing a wallet cannot leave an obsolete derived signer behind.
fn link_reusable_evm_wallets_to_hyperliquid(config: &mut BrokerConfigV2) -> Result<()> {
    let wallets = config
        .connections
        .iter()
        .filter_map(|connection| match connection {
            ConnectionConfig::Evm {
                name,
                address,
                credential_ref,
                signing_enabled,
                ..
            } => Some((
                name.clone(),
                address.to_ascii_lowercase(),
                credential_ref.clone(),
                *signing_enabled,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    for (wallet_name, owner, credential_ref, signing_enabled) in wallets {
        let owner_connection = config.connections.iter_mut().find(|connection| {
            matches!(connection,
                ConnectionConfig::Hyperliquid {
                    owner: existing_owner,
                    execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
                    ..
                } if existing_owner.eq_ignore_ascii_case(&owner))
        });
        if owner_connection.is_some() {
            // An explicit owner mapping is the canonical account view. Keep
            // its human-enrolled capability exactly as configured: an EVM
            // signer must never silently elevate a read-only Hyperliquid row.
            continue;
        }

        let alias_name = hyperliquid_alias_name(&wallet_name, &owner);
        let alias_resource = ResourceId(format!("hyperliquid:mainnet:{owner}:account:primary"));
        if config.connections.iter().any(|connection| {
            name(connection) == alias_name || resource(connection) == &alias_resource
        }) {
            return Err(error(
                "HYPERLIQUID_AUTO_LINK_COLLISION",
                &format!(
                    "The automatic Hyperliquid link for EVM wallet {wallet_name} conflicts with an enrolled connection."
                ),
            ));
        }
        config.connections.push(ConnectionConfig::Hyperliquid {
            name: alias_name,
            resource: alias_resource,
            owner,
            credential_ref,
            signing_enabled,
            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            auto_derived: true,
        });
    }
    Ok(())
}

fn hyperliquid_alias_name(wallet_name: &str, owner: &str) -> String {
    let suffix = "-hyperliquid";
    let full_name = format!("{wallet_name}{suffix}");
    const MAX_ALIAS_LEN: usize = 64;
    if full_name.len() <= MAX_ALIAS_LEN {
        return full_name;
    }

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{wallet_name}:{owner}").as_bytes());
    let short_hash = hex::encode(&digest[..4]);
    let keep = MAX_ALIAS_LEN - suffix.len() - short_hash.len() - 1;
    let mut prefix = String::new();
    for character in wallet_name.chars() {
        if prefix.len() + character.len_utf8() > keep {
            break;
        }
        prefix.push(character);
    }
    format!("{prefix}-{short_hash}{suffix}")
}

async fn validate_connection(
    args: &AddConnectionArgs,
    config: &BrokerConfigV2,
    store: Arc<TrustedCredentialStore>,
    staged: Option<&CredentialRef>,
    material: Option<&SecretMaterial>,
    ui: &Ui,
) -> Result<(ConnectionConfig, Vec<Value>, Vec<String>)> {
    match args.provider {
        Provider::Relay => {
            let reference = staged.ok_or_else(|| {
                error(
                    "CREDENTIAL_REQUIRED",
                    "Relay API key enrollment needs a protected key.",
                )
            })?;
            Ok((
                ConnectionConfig::Relay {
                    name: args.name.clone(),
                    resource: ResourceId("relay:service".into()),
                    credential_ref: reference.as_str().into(),
                },
                vec![json!({"name":"protected service credential","status":"passed"})],
                vec!["The Relay API key is used for quote service access and grants no wallet authority. Calls may consume quota or incur billing.".into()],
            ))
        }
        Provider::Jupiter => {
            let reference = staged.ok_or_else(|| {
                error(
                    "CREDENTIAL_REQUIRED",
                    "Jupiter API key enrollment needs a protected key.",
                )
            })?;
            Ok((
                ConnectionConfig::Jupiter {
                    name: args.name.clone(),
                    resource: ResourceId("jupiter:service".into()),
                    credential_ref: reference.as_str().into(),
                },
                vec![json!({"name":"protected service credential","status":"passed"})],
                vec!["The Jupiter API key is used only for Jupiter service access and grants no wallet authority.".into()],
            ))
        }
        Provider::Solana => {
            let derived = match material {
                Some(SecretMaterial::SolanaPrivateKey { private_key }) => Some(
                    gofin_solana::SolanaSigner::parse(private_key)?
                        .address()
                        .to_owned(),
                ),
                _ => None,
            };
            let address = derived
                .clone()
                .or_else(|| args.address.clone())
                .ok_or_else(|| {
                    error(
                        "WALLET_ADDRESS_REQUIRED",
                        "The Solana wallet address could not be resolved.",
                    )
                })?;
            gofin_solana::parse_address(&address)?;
            if args
                .address
                .as_ref()
                .is_some_and(|expected| expected != &address)
            {
                return Err(error(
                    "SIGNER_ADDRESS_MISMATCH",
                    "The protected Solana key does not match the requested public wallet address.",
                ));
            }
            let rpc = gofin_solana::HttpRpc::mainnet_beta()?;
            let balance = {
                let _progress = ui.progress("Checking Solana mainnet-beta wallet");
                gofin_solana::SolanaRpc::request(
                    rpc.as_ref(),
                    "getBalance",
                    json!([address, {"commitment":"confirmed"}]),
                )
                .await?
            };
            let lamports = balance
                .get("value")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    error(
                        "SOLANA_RESPONSE_INVALID",
                        "The Solana wallet balance response was invalid.",
                    )
                })?;
            let network = gofin_solana::NETWORK.to_owned();
            let connection = ConnectionConfig::Solana {
                name: args.name.clone(),
                resource: ResourceId(format!("{network}:{address}")),
                address: address.clone(),
                credential_ref: staged.map(|reference| reference.as_str().into()),
                signing_enabled: derived.is_some(),
                network,
            };
            Ok((
                connection,
                vec![
                    json!({"name":"mainnet-beta RPC identity","status":"passed"}),
                    json!({"name":"wallet identity","status":"passed","signer_derived":derived.is_some()}),
                    json!({"name":"native balance read","status":"passed","lamports":lamports}),
                ],
                vec![],
            ))
        }
        Provider::Kraken | Provider::KrakenFutures => {
            let reference = staged.ok_or_else(|| {
                error(
                    "KRAKEN_CREDENTIAL_REQUIRED",
                    "A staged Kraken key is required.",
                )
            })?;
            use gofin_exchange::kraken::{KrakenClient, KrakenEndpoint, KrakenResponse};
            let futures = args.provider == Provider::KrakenFutures;
            let credentials = Arc::new(ProtectedCredentialProvider::from_store(store));
            let client = KrakenClient::new(BrokerCredentialProvider::kraken_nonces(
                credentials.as_ref(),
            )?)
            .map_err(|_| {
                error(
                    "KRAKEN_UNAVAILABLE",
                    "Kraken transport could not initialize.",
                )
            })?;
            let supplied = args.account_id.as_deref().ok_or_else(|| {
                error(
                    "KRAKEN_IDENTITY_REQUIRED",
                    "Kraken needs its nonsecret public account ID.",
                )
            })?;
            let endpoint = if futures {
                KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check
            } else {
                KrakenEndpoint::KrakenPrivatePostGetApiKeyInfo
            };
            let response = {
                let _progress = ui.progress("Checking Kraken credentials");
                client.request(endpoint,Some(gofin_exchange::ccxt::CredentialProvider::credentials(credentials.as_ref(),reference.as_str()).map_err(|_|error("KRAKEN_CREDENTIAL_UNAVAILABLE","Staged Kraken credentials are unavailable."))?),json!({}),None).await.map_err(|_|error("KRAKEN_AUTHENTICATION_FAILED","Kraken rejected the read-only credential check; confirm this product's permissions and disable API-level 2FA on this key."))?
            };
            let data = match response {
                KrakenResponse::Json(data) => data,
                _ => {
                    return Err(error(
                        "KRAKEN_INVALID_RESPONSE",
                        "Kraken did not return account metadata.",
                    ));
                }
            };
            let verified = verify_kraken_identity(&data, futures, supplied)?;
            reject_kraken_account_alias(config, &args.name, futures, supplied, &data)?;
            if let Some(default_id) = &args.default_wallet_account_id {
                let response = {
                    let _progress = ui.progress("Checking owned Kraken wallet account");
                    client.request(KrakenEndpoint::KrakenPrivatePostListWalletAccounts, Some(gofin_exchange::ccxt::CredentialProvider::credentials(credentials.as_ref(), reference.as_str()).map_err(|_| error("KRAKEN_CREDENTIAL_UNAVAILABLE", "Staged Kraken credentials are unavailable."))?), json!({}), None).await.map_err(|_| error("KRAKEN_WALLET_OWNERSHIP_UNVERIFIED", "Cannot verify the selected default wallet. Grant ListWalletAccounts permission or omit this optional relationship."))?
                };
                let KrakenResponse::Json(wallets) = response else {
                    return Err(error(
                        "KRAKEN_WALLET_OWNERSHIP_UNVERIFIED",
                        "Kraken did not return its authenticated wallet list.",
                    ));
                };
                gofin_kraken::funding::validate_default_wallet_relationship(&wallets, default_id)?;
            }
            let provider = if futures { "krakenfutures" } else { "kraken" };
            let resource = ResourceId(format!("{provider}:account:{supplied}"));
            let connection = if futures {
                ConnectionConfig::KrakenFutures {
                    name: args.name.clone(),
                    resource,
                    credential_ref: reference.as_str().into(),
                    physical_identity: supplied.into(),
                    physical_identity_verified: verified,
                }
            } else {
                ConnectionConfig::Kraken {
                    default_wallet_account_id: args.default_wallet_account_id.clone(),
                    name: args.name.clone(),
                    resource,
                    credential_ref: reference.as_str().into(),
                    physical_identity: supplied.into(),
                    physical_identity_verified: verified,
                }
            };
            Ok((
                connection,
                vec![
                    json!({"name":"Kraken authenticated read","status":"passed","product":provider}),
                    json!({"name":"account identity","status":if verified{"venue_verified"}else{"human_confirmed"}}),
                    json!({"name":"default wallet relationship","status":if args.default_wallet_account_id.is_some(){"human_verified"}else{"not_selected"},"ownership":if args.default_wallet_account_id.is_some(){"venue_verified"}else{"not_checked"}}),
                ],
                if verified && args.default_wallet_account_id.is_none() {
                    vec![]
                } else {
                    {
                        let mut warnings = vec![];
                        if !verified {
                            warnings.push("Kraken did not return a matching physical account identifier; the account mapping is human-confirmed.".into());
                        }
                        if args.default_wallet_account_id.is_some() {
                            warnings.push("Default wallet relationship is human_verified; Kraken verifies ownership but does not identify the default alias.".into());
                        }
                        warnings
                    }
                },
            ))
        }
        Provider::Coinbase | Provider::CoinbaseExchange | Provider::CoinbaseInternational => {
            use gofin_exchange::coinbase::{CoinbaseEndpoint as E, CoinbaseTransport};
            let reference = staged.ok_or_else(|| {
                error(
                    "CREDENTIAL_REQUIRED",
                    "Coinbase needs a staged protected API credential.",
                )
            })?;
            let credentials = ProtectedCredentialProvider::from_store(store);
            let auth = credentials.coinbase_auth(reference)?;
            let transport = CoinbaseTransport::new(auth).map_err(|_| {
                error(
                    "COINBASE_UNAVAILABLE",
                    "Coinbase transport could not initialize.",
                )
            })?;
            let mut checks =
                vec![json!({"name":"authenticated credential read","status":"passed"})];
            let mut reported_permissions = Value::Null;
            let ids = match args.provider {
                Provider::Coinbase => {
                    let permissions = coinbase_read(&transport, E::KeyPermissions).await?;
                    if permissions.get("can_view").and_then(Value::as_bool) != Some(true) {
                        return Err(error(
                            "COINBASE_VIEW_PERMISSION_REQUIRED",
                            "Coinbase did not affirm that the key can view its account scope.",
                        ));
                    }
                    reported_permissions = json!({"can_view":permissions.get("can_view"),"can_trade":permissions.get("can_trade"),"can_transfer":permissions.get("can_transfer"),"can_receive":permissions.get("can_receive")});
                    let portfolio=permissions.get("portfolio_uuid").and_then(Value::as_str).ok_or_else(||error("COINBASE_PORTFOLIO_UNVERIFIED","Coinbase key permissions did not identify a permissioned portfolio."))?.to_owned();
                    let portfolios = coinbase_read(&transport, E::Portfolios).await?;
                    let ids = coinbase_ids(&portfolios, &["uuid", "portfolio_uuid", "id"]);
                    if !ids.contains(&portfolio) {
                        return Err(error(
                            "COINBASE_PORTFOLIO_UNVERIFIED",
                            "The permissioned Coinbase portfolio was not present in the authenticated portfolio list.",
                        ));
                    }
                    checks.push(json!({"name":"reported API permissions","status":"observed","permissions":reported_permissions}));
                    vec![portfolio]
                }
                Provider::CoinbaseExchange => {
                    let accounts = coinbase_read(&transport, E::ExchangeAccounts).await?;
                    let mut ids = coinbase_ids(&accounts, &["profile_id"]);
                    ids.sort();
                    ids.dedup();
                    ids
                }
                Provider::CoinbaseInternational => {
                    let portfolios = coinbase_read(&transport, E::InternationalAccounts).await?;
                    let mut ids = coinbase_ids(&portfolios, &["portfolio_id", "id", "uuid"]);
                    ids.sort();
                    ids.dedup();
                    ids
                }
                _ => unreachable!(),
            };
            let selected = if args.provider == Provider::Coinbase {
                ids.first().cloned().ok_or_else(|| {
                    error(
                        "COINBASE_PORTFOLIO_UNVERIFIED",
                        "Coinbase returned no authenticated portfolio.",
                    )
                })?
            } else if let Some(selector) = &args.portfolio_id {
                if ids.contains(selector) {
                    selector.clone()
                } else {
                    return Err(error(
                        "COINBASE_PORTFOLIO_UNVERIFIED",
                        "The selected Coinbase portfolio was not returned by authenticated account discovery.",
                    ));
                }
            } else if ids.len() == 1 {
                ids[0].clone()
            } else {
                return Err(error(
                    "COINBASE_PORTFOLIO_SELECTION_REQUIRED",
                    "Coinbase exposed multiple portfolio identities. Re-run enrollment with --portfolio-id using an exact listed ID.",
                ));
            };
            if args.portfolio_id.as_ref().is_some_and(|id| id != &selected) {
                return Err(error(
                    "COINBASE_PORTFOLIO_MISMATCH",
                    "The selected portfolio does not match the key's permissioned portfolio.",
                ));
            }
            let provider_name = match args.provider {
                Provider::Coinbase => "coinbase",
                Provider::CoinbaseExchange => "coinbaseexchange",
                _ => "coinbaseinternational",
            };
            let resource = ResourceId(format!("{provider_name}:portfolio:{selected}"));
            let connection = match args.provider {
                Provider::Coinbase => ConnectionConfig::Coinbase {
                    name: args.name.clone(),
                    resource,
                    credential_ref: reference.as_str().into(),
                    physical_identity: selected.clone(),
                    physical_identity_verified: true,
                    portfolio_id: Some(selected.clone()),
                },
                Provider::CoinbaseExchange => ConnectionConfig::CoinbaseExchange {
                    name: args.name.clone(),
                    resource,
                    credential_ref: reference.as_str().into(),
                    physical_identity: selected.clone(),
                    physical_identity_verified: true,
                    portfolio_id: Some(selected.clone()),
                },
                _ => ConnectionConfig::CoinbaseInternational {
                    name: args.name.clone(),
                    resource,
                    credential_ref: reference.as_str().into(),
                    physical_identity: selected.clone(),
                    physical_identity_verified: true,
                    portfolio_id: Some(selected.clone()),
                },
            };
            checks.push(json!({"name":"portfolio identity","status":"venue_verified","portfolio_id":selected}));
            if args.provider == Provider::Coinbase {
                checks.push(json!({"name":"permissioned portfolio binding","status":"passed","permissions":reported_permissions}));
            }
            Ok((connection, checks, vec![]))
        }
        Provider::CoinGecko => {
            let reference = staged.ok_or_else(|| {
                error(
                    "CREDENTIAL_REQUIRED",
                    "CoinGecko needs a protected API key.",
                )
            })?;
            let plan = args.api_plan.as_deref().unwrap_or("demo");
            let source = store.with_material(reference, |material| match material {
                SecretMaterial::CoinGeckoApiKey {
                    api_key,
                    plan: stored_plan,
                } if stored_plan == plan => gofin_pricing::CoinGeckoPrices::with_api_key(
                    if plan == "demo" {
                        gofin_pricing::ApiPlan::Demo
                    } else {
                        gofin_pricing::ApiPlan::Pro
                    },
                    api_key,
                ),
                _ => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "The staged key is not the selected CoinGecko plan.",
                )),
            })?;
            {
                let _progress = ui.progress("Checking CoinGecko access");
                source.validate_access().await?;
            }
            {
                let _progress = ui.progress("Checking current USD pricing");
                gofin_pricing::UsdPriceSource::quote(
                    &source,
                    &gofin_domain::AssetId {
                        network: gofin_networks::by_chain_id(1)
                            .expect("Ethereum is reviewed")
                            .network_id
                            .into(),
                        reference: "native".into(),
                        decimals: 18,
                    },
                )
                .await?;
            }
            Ok((
                ConnectionConfig::CoinGecko {
                    name: args.name.clone(),
                    resource: ResourceId("market-data:coingecko".into()),
                    credential_ref: reference.as_str().into(),
                    api_plan: plan.into(),
                },
                vec![
                    json!({"name":"CoinGecko API access","status":"passed","api_plan":plan}),
                    json!({"name":"fresh USD price read","status":"passed"}),
                ],
                vec![],
            ))
        }
        Provider::Evm => {
            let network = config
                .networks
                .iter()
                .find(|network| {
                    network.id
                        == args.network.as_deref().unwrap_or_else(|| {
                            gofin_networks::by_chain_id(1)
                                .expect("Ethereum is reviewed")
                                .network_id
                        })
                })
                .ok_or_else(|| {
                    error(
                        "NETWORK_NOT_CONFIGURED",
                        "The requested reviewed network is not configured.",
                    )
                })?;
            let derived = match material {
                Some(SecretMaterial::EvmPrivateKey { private_key }) => Some(format!(
                    "{:#x}",
                    gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)?.address()
                )),
                _ => None,
            };
            let address = derived
                .clone()
                .or_else(|| args.address.clone())
                .ok_or_else(|| {
                    error(
                        "WALLET_ADDRESS_REQUIRED",
                        "The wallet address could not be resolved.",
                    )
                })?
                .to_ascii_lowercase();
            if args
                .address
                .as_ref()
                .is_some_and(|expected| !expected.eq_ignore_ascii_case(&address))
            {
                return Err(error(
                    "SIGNER_ADDRESS_MISMATCH",
                    "The protected signer does not match the requested public wallet address.",
                ));
            }
            let rpc = Arc::new(gofin_evm::HttpRpc::new(
                network.rpc_urls.clone(),
                network.chain_id,
            )?);
            let runtime = gofin_evm::EvmRuntime::new(rpc, network.chain_id, None);
            {
                let _progress = ui.progress("Checking EVM network");
                runtime.assert_chain().await?;
            }
            let balance = {
                let _progress = ui.progress("Checking wallet balance");
                runtime.balance(gofin_evm::address(&address)?, None).await?
            };
            let connection = ConnectionConfig::Evm {
                name: args.name.clone(),
                resource: ResourceId(format!("evm:{address}")),
                credential_ref: staged.map(|reference| reference.as_str().into()),
                address,
                signing_enabled: derived.is_some(),
            };
            Ok((
                connection,
                vec![
                    json!({"name":"RPC chain","status":"passed","chain_id":network.chain_id}),
                    json!({"name":"wallet identity","status":"passed","signer_derived":derived.is_some()}),
                    json!({"name":"native balance read","status":"passed","atoms":balance.to_string()}),
                ],
                vec![],
            ))
        }
        Provider::Hyperliquid => {
            let owner = args
                .address
                .as_deref()
                .filter(|value| gofin_hyperliquid::valid_address(value))
                .ok_or_else(|| {
                    error(
                        "HYPERLIQUID_OWNER_REQUIRED",
                        "A valid public Hyperliquid owner address is required.",
                    )
                })?
                .to_ascii_lowercase();
            let execution_scope = match (args.hyperliquid_scope, args.execution_address.as_deref())
            {
                (Some(gofin_discovery::HyperliquidScopeKind::Subaccount), Some(address)) => {
                    gofin_hyperliquid::HyperliquidExecutionScope::Subaccount {
                        address: address.to_ascii_lowercase(),
                    }
                }
                (Some(gofin_discovery::HyperliquidScopeKind::Vault), Some(address)) => {
                    gofin_hyperliquid::HyperliquidExecutionScope::Vault {
                        address: address.to_ascii_lowercase(),
                    }
                }
                _ => gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            };
            let delegated = !matches!(
                execution_scope,
                gofin_hyperliquid::HyperliquidExecutionScope::Owner
            );
            let (credential_ref, signing_enabled, signer_derived, reused_master) = if delegated {
                let master = config.connections.iter().find_map(|connection| match connection {
                    ConnectionConfig::Hyperliquid { owner: configured_owner, credential_ref: Some(reference), signing_enabled: true, .. }
                        if configured_owner.eq_ignore_ascii_case(&owner) => Some(reference.clone()),
                    _ => None,
                }).ok_or_else(|| error("HYPERLIQUID_MASTER_SIGNER_REQUIRED", "A delegated scope can only reuse an already enrolled signing master account."))?;
                let target = execution_scope.address().ok_or_else(|| {
                    error(
                        "HYPERLIQUID_SCOPE_INVALID",
                        "The delegated address is missing.",
                    )
                })?;
                let ccxt = gofin_exchange::ccxt::CcxtClient::new().map_err(|_| {
                    error(
                        "VENUE_UNAVAILABLE",
                        "The trusted Hyperliquid runtime could not be initialized.",
                    )
                })?;
                let verification = match &execution_scope {
                    gofin_hyperliquid::HyperliquidExecutionScope::Subaccount { .. } => {
                        let rows = ccxt.hyperliquid_info(gofin_exchange::ccxt::HyperliquidInfoRequest::SubAccounts { owner: owner.clone() }).await
                            .map_err(|_| error("HYPERLIQUID_SCOPE_UNVERIFIED", "Hyperliquid did not return the requested master's subaccount relationships."))?;
                        gofin_hyperliquid::verify_execution_scope(
                            &owner,
                            &execution_scope,
                            Some(&rows),
                            None,
                        )
                    }
                    gofin_hyperliquid::HyperliquidExecutionScope::Vault { .. } => {
                        let details = ccxt
                            .hyperliquid_info(
                                gofin_exchange::ccxt::HyperliquidInfoRequest::VaultDetails {
                                    vault_address: target.to_owned(),
                                    user: Some(owner.clone()),
                                },
                            )
                            .await
                            .map_err(|_| {
                                error(
                                    "HYPERLIQUID_SCOPE_UNVERIFIED",
                                    "Hyperliquid did not verify this vault leader.",
                                )
                            })?;
                        gofin_hyperliquid::verify_execution_scope(
                            &owner,
                            &execution_scope,
                            None,
                            Some(&details),
                        )
                    }
                    gofin_hyperliquid::HyperliquidExecutionScope::Owner => unreachable!(),
                };
                verification.map_err(|_| error("HYPERLIQUID_SCOPE_UNVERIFIED", "The selected subaccount or vault is not verified as belonging to the enrolled master signer."))?;
                (Some(master), true, false, true)
            } else {
                let derived = match material {
                    Some(SecretMaterial::EvmPrivateKey { private_key }) => Some(
                        format!(
                            "{:#x}",
                            gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)?
                                .address()
                        )
                        .to_ascii_lowercase(),
                    ),
                    None => None,
                    _ => {
                        return Err(error(
                            "CREDENTIAL_TYPE_MISMATCH",
                            "The selected protected credential is not an EVM signing key.",
                        ));
                    }
                };
                if derived.as_deref().is_some_and(|address| address != owner) {
                    return Err(error(
                        "SIGNER_ADDRESS_MISMATCH",
                        "The protected signer does not match the requested Hyperliquid owner.",
                    ));
                }
                (
                    staged.map(|reference| reference.as_str().into()),
                    derived.is_some(),
                    derived.is_some(),
                    false,
                )
            };
            let execution_address = execution_scope.address().unwrap_or(&owner);
            let connection = ConnectionConfig::Hyperliquid {
                name: args.name.clone(),
                resource: ResourceId(format!(
                    "hyperliquid:mainnet:{execution_address}:account:primary"
                )),
                owner: owner.clone(),
                credential_ref,
                signing_enabled,
                execution_scope: execution_scope.clone(),
                auto_derived: false,
            };
            let checks = vec![
                json!({"name":"master signer identity","status":"passed","signer_derived":signer_derived,"reused_protected_master":reused_master}),
                json!({"name":"delegated account relationship","status":if delegated{"passed"}else{"not_applicable"},"scope":execution_scope.kind(),"verified":delegated}),
            ];
            Ok((
                connection,
                checks,
                if delegated {
                    vec!["The master signer can trade for the verified delegated account; account authority and transfers remain owner-only.".into()]
                } else {
                    vec!["Hyperliquid owner enrollment is offline and does not verify account access or venue permissions.".into()]
                },
            ))
        }
        Provider::Binance => {
            let reference = staged.ok_or_else(|| {
                error(
                    "CREDENTIAL_REQUIRED",
                    "Binance enrollment needs a protected credential.",
                )
            })?;
            let credentials = Arc::new(ProtectedCredentialProvider::from_store(store));
            let client = gofin_exchange::ccxt::CcxtClient::new().map_err(|_| {
                error(
                    "VENUE_UNAVAILABLE",
                    "The trusted exchange runtime could not be initialized.",
                )
            })?;
            let transport = gofin_binance::BinanceCcxtTransport::new(Arc::new(client), credentials);
            let handle = CredentialHandle::new(reference.as_str()).map_err(|_| {
                error(
                    "CREDENTIAL_REFERENCE_INVALID",
                    "The staged credential handle is invalid.",
                )
            })?;
            let identity = {
                let _progress = ui.progress("Checking Binance account");
                transport
                    .request(&handle, BinanceRequest::new(BinanceEndpoint::Account))
                    .await
                    .map_err(|cause| {
                        binance_enrollment_error("BINANCE_AUTHENTICATION_FAILED", cause)
                    })?
            };
            let uid = account_uid(&identity)?;
            let permissions = {
                let _progress = ui.progress("Checking Binance API permissions");
                transport
                    .request(
                        &handle,
                        BinanceRequest::new(BinanceEndpoint::ApiRestrictions),
                    )
                    .await
                    .map_err(|cause| {
                        binance_enrollment_error("BINANCE_PERMISSIONS_UNAVAILABLE", cause)
                    })?
            };
            let permissions: gofin_binance::ApiRestrictions = serde_json::from_value(permissions)
                .map_err(|_| {
                error(
                    "BINANCE_PERMISSIONS_INVALID",
                    "Reported key restrictions could not be decoded.",
                )
            })?;
            if permissions.enable_reading == Some(false) {
                return Err(error(
                    "BINANCE_READING_DISABLED",
                    "The enrolled Binance key explicitly disables reading.",
                ));
            }
            let role = args.role.ok_or_else(|| {
                error(
                    "BINANCE_ROLE_REQUIRED",
                    "Select the Binance connection role.",
                )
            })?;
            let mut checks = vec![
                json!({"name":"credential authentication","status":"passed"}),
                json!({"name":"physical account identity","status":"passed","uid":uid,"source":"GET /api/v3/account"}),
                json!({"name":"reported API restrictions","status":"observed","permissions":permissions}),
            ];
            let mut warnings = Vec::new();
            if permissions.enable_reading.is_none() {
                warnings.push(
                    "Read authentication passed; the API did not explicitly report enableReading."
                        .into(),
                );
            }
            if role == BinanceRole::Sub && args.parent.is_some() {
                let parent = config
                    .connections
                    .iter()
                    .find(|connection| Some(name(connection)) == args.parent.as_deref())
                    .ok_or_else(|| {
                        error(
                            "BINANCE_PARENT_NOT_FOUND",
                            "Enroll the main authority before the child connection.",
                        )
                    })?;
                let ConnectionConfig::Binance {
                    role: parent_role,
                    credential_ref,
                    ..
                } = parent
                else {
                    return Err(error(
                        "BINANCE_PARENT_INVALID",
                        "The selected parent is not a Binance main connection.",
                    ));
                };
                if parent_role != "main" {
                    return Err(error(
                        "BINANCE_PARENT_INVALID",
                        "The selected parent is not a Binance main connection.",
                    ));
                }
                let email = args.email.as_deref().ok_or_else(|| {
                    error(
                        "BINANCE_EMAIL_REQUIRED",
                        "The child account requires its Binance email.",
                    )
                })?;
                let parent_handle = CredentialHandle::new(credential_ref).map_err(|_| {
                    error(
                        "CREDENTIAL_REFERENCE_INVALID",
                        "The parent's credential handle is invalid.",
                    )
                })?;
                let hierarchy = {
                    let _progress = ui.progress("Checking Binance subaccount mapping");
                    transport
                        .request(
                            &parent_handle,
                            BinanceRequest::new(BinanceEndpoint::SubAccountList)
                                .push("email", email)
                                .push("page", "1")
                                .push("limit", "200"),
                        )
                        .await
                        .map_err(|cause| {
                            binance_enrollment_error("BINANCE_PARENT_READ_FAILED", cause)
                        })?
                };
                let children: gofin_binance::SubAccountListResponse =
                    serde_json::from_value(hierarchy).map_err(|_| {
                        error(
                            "BINANCE_HIERARCHY_INVALID",
                            "The main key's child response was invalid.",
                        )
                    })?;
                let rows: Vec<_> = children
                    .sub_accounts
                    .iter()
                    .filter(|child| child.email == email)
                    .collect();
                if rows.len() != 1 || rows[0].is_freeze != Some(false) {
                    return Err(error(
                        "BINANCE_CHILD_UNVERIFIED",
                        "The main key did not report exactly one available child with the requested email.",
                    ));
                }
                checks.push(json!({"name":"parent child email","status":"passed","email":email}));
                checks.push(json!({"name":"child UID/email association","status":"human_confirmation_required","uid":uid,"email":email}));
            } else if role == BinanceRole::Sub {
                checks.push(json!({"name":"subaccount hierarchy","status":"not_checked","reason":"No parent connection was supplied; the authenticated account UID is verified independently."}));
            }
            let connection = ConnectionConfig::Binance {
                name: args.name.clone(),
                resource: ResourceId(format!("binance:uid:{uid}:SPOT")),
                credential_ref: reference.as_str().into(),
                role: if role == BinanceRole::Main {
                    "main"
                } else {
                    "sub"
                }
                .into(),
                parent: args.parent.clone(),
                email: if role == BinanceRole::Sub {
                    args.email.clone()
                } else {
                    None
                },
                wallet_type: "SPOT".into(),
                physical_identity: uid,
                physical_identity_verified: true,
            };
            Ok((connection, checks, warnings))
        }
    }
}
fn account_uid(identity: &Value) -> Result<String> {
    if identity["accountType"] != "SPOT" || !identity["balances"].is_array() {
        return Err(error(
            "BINANCE_IDENTITY_UNVERIFIED",
            "The key did not report a SPOT account identity and balances.",
        ));
    }
    let uid = match &identity["uid"] {
        Value::String(uid) => uid.clone(),
        Value::Number(number) if number.is_u64() => number.to_string(),
        _ => {
            return Err(error(
                "BINANCE_IDENTITY_UNVERIFIED",
                "The account UID is missing or inexact.",
            ));
        }
    };
    if uid.is_empty()
        || uid.len() > 32
        || uid.starts_with('0')
        || !uid.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(error(
            "BINANCE_IDENTITY_UNVERIFIED",
            "The account UID is not a canonical positive integer.",
        ));
    }
    Ok(uid)
}
pub(crate) async fn approve_and_audit(
    uid: u32,
    binding: &Value,
    summary: &str,
    ui: &Ui,
) -> Result<AuthorizationChallenge> {
    let challenge = AuthorizationChallenge {
        purpose: "configuration".into(),
        operation_id: format!("configuration:{}", uuid::Uuid::new_v4()),
        binding: binding.clone(),
        binding_hash: binding_hash(binding)?,
        summary: summary.into(),
        nonce: format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
        expires_at: (chrono::Utc::now() + chrono::Duration::seconds(90)).to_rfc3339(),
    };
    gofin_authorization::validate_challenge(&challenge, &now())?;
    let review = presentation::review(binding)?;
    let proof = HumanTerminal::open(uid)?.review_configuration(&challenge, &review)?;
    let approval = {
        let _progress = ui.progress("Waiting for Touch ID");
        MacOsAuthorizer::protected(uid)?
            .authorize_reviewed_configuration(&challenge, proof)
            .await
            .map_err(|failure| {
                if failure.code == "AUTHORIZATION_EXPIRED" {
                    gofin_human_terminal::configuration_approval_expired(
                        binding["action"].as_str().unwrap_or(""),
                    )
                } else {
                    failure
                }
            })?
    };
    let path = Path::new("/Library/GoFin/private/configuration-audit.jsonl");
    if path.try_exists().map_err(|_| {
        error(
            "CONFIG_AUDIT_FAILED",
            "Configuration approval audit could not be inspected.",
        )
    })? {
        gofin_authorization::validate_metadata(path, true)?;
    }
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| {
            error(
                "CONFIG_AUDIT_FAILED",
                "Configuration approval audit could not be opened.",
            )
        })?;
    let bytes=serde_json::to_vec(&json!({"version":1,"phase":"approved_attempt","approver_uid":uid,"operation_id":challenge.operation_id,"binding_hash":challenge.binding_hash,"binding":binding,"approval":approval,"observed_at":now()}))
        .map_err(|_|error("CONFIG_AUDIT_FAILED","Configuration audit could not be encoded."))?;
    file.write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            error(
                "CONFIG_AUDIT_FAILED",
                "Configuration approval audit could not be durably saved.",
            )
        })?;
    File::open(path.parent().expect("fixed parent"))
        .and_then(|directory| directory.sync_all())
        .map_err(|_| {
            error(
                "CONFIG_AUDIT_FAILED",
                "Configuration audit directory could not be synchronized.",
            )
        })?;
    gofin_authorization::validate_challenge(&challenge, &now()).map_err(|failure| {
        if failure.code == "AUTHORIZATION_EXPIRED" {
            gofin_human_terminal::configuration_approval_expired(
                binding["action"].as_str().unwrap_or(""),
            )
        } else {
            failure
        }
    })?;
    Ok(challenge)
}
fn config_hash(config: &BrokerConfigV2) -> Result<String> {
    binding_hash(
        &serde_json::to_value(config)
            .map_err(|_| error("CONFIG_INVALID", "Configuration could not be encoded."))?,
    )
}
fn name(connection: &ConnectionConfig) -> &str {
    match connection {
        ConnectionConfig::Coinbase { name, .. }
        | ConnectionConfig::CoinbaseExchange { name, .. }
        | ConnectionConfig::CoinbaseInternational { name, .. }
        | ConnectionConfig::Binance { name, .. }
        | ConnectionConfig::Ethereum { name, .. }
        | ConnectionConfig::Evm { name, .. }
        | ConnectionConfig::CoinGecko { name, .. }
        | ConnectionConfig::Hyperliquid { name, .. }
        | ConnectionConfig::Solana { name, .. }
        | ConnectionConfig::Jupiter { name, .. }
        | ConnectionConfig::Relay { name, .. }
        | ConnectionConfig::Kraken { name, .. }
        | ConnectionConfig::KrakenFutures { name, .. } => name,
    }
}
fn resource(connection: &ConnectionConfig) -> &ResourceId {
    match connection {
        ConnectionConfig::Coinbase { resource, .. }
        | ConnectionConfig::CoinbaseExchange { resource, .. }
        | ConnectionConfig::CoinbaseInternational { resource, .. }
        | ConnectionConfig::Binance { resource, .. }
        | ConnectionConfig::Ethereum { resource, .. }
        | ConnectionConfig::Evm { resource, .. }
        | ConnectionConfig::CoinGecko { resource, .. }
        | ConnectionConfig::Hyperliquid { resource, .. }
        | ConnectionConfig::Solana { resource, .. }
        | ConnectionConfig::Jupiter { resource, .. }
        | ConnectionConfig::Relay { resource, .. }
        | ConnectionConfig::Kraken { resource, .. }
        | ConnectionConfig::KrakenFutures { resource, .. } => resource,
    }
}
fn credential(connection: &ConnectionConfig) -> Option<CredentialRef> {
    match connection {
        ConnectionConfig::Coinbase { credential_ref, .. }
        | ConnectionConfig::CoinbaseExchange { credential_ref, .. }
        | ConnectionConfig::CoinbaseInternational { credential_ref, .. }
        | ConnectionConfig::Binance { credential_ref, .. }
        | ConnectionConfig::CoinGecko { credential_ref, .. }
        | ConnectionConfig::Jupiter { credential_ref, .. }
        | ConnectionConfig::Relay { credential_ref, .. }
        | ConnectionConfig::Kraken { credential_ref, .. }
        | ConnectionConfig::KrakenFutures { credential_ref, .. } => {
            CredentialRef::new(credential_ref).ok()
        }
        ConnectionConfig::Ethereum { credential_ref, .. }
        | ConnectionConfig::Evm { credential_ref, .. }
        | ConnectionConfig::Hyperliquid { credential_ref, .. }
        | ConnectionConfig::Solana { credential_ref, .. } => credential_ref
            .as_ref()
            .and_then(|reference| CredentialRef::new(reference).ok()),
    }
}
fn referenced(config: &BrokerConfigV2) -> BTreeSet<String> {
    config
        .connections
        .iter()
        .filter_map(credential)
        .map(|reference| reference.as_str().to_owned())
        .collect()
}
fn recover_inactive_credentials(
    store: &TrustedCredentialStore,
    config: &BrokerConfigV2,
) -> Result<()> {
    let active = referenced(config);
    for reference in store.references()? {
        if !active.contains(reference.as_str()) {
            store.remove(&reference).map_err(|_|error("ENROLLMENT_CLEANUP_REQUIRED","An inactive protected credential could not be cleaned up; no new connection was activated."))?;
        }
    }
    Ok(())
}
fn preserve_default_wallet(
    args: &mut AddConnectionArgs,
    old: Option<&ConnectionConfig>,
) -> Result<()> {
    if let Some(ConnectionConfig::Kraken {
        default_wallet_account_id: Some(previous),
        ..
    }) = old
    {
        if args
            .default_wallet_account_id
            .as_ref()
            .is_some_and(|id| id != previous)
        {
            return Err(error(
                "KRAKEN_DEFAULT_WALLET_CHANGED",
                "An existing default-wallet alias cannot change because historical custody records retain this identity. Preserve its ID.",
            ));
        }
        args.default_wallet_account_id = Some(previous.clone());
    }
    Ok(())
}
fn public_connection(connection: &ConnectionConfig) -> Value {
    serde_json::to_value(public_connection_contract(connection))
        .expect("public enrollment connection is serializable")
}

fn immutable_identity(connection: &ConnectionConfig) -> Value {
    match connection {
        ConnectionConfig::Evm { name, address, .. }
        | ConnectionConfig::Ethereum { name, address, .. } => {
            json!({"provider":"evm","name":name,"address":address.to_ascii_lowercase()})
        }
        ConnectionConfig::Hyperliquid {
            name,
            owner,
            execution_scope,
            ..
        } => {
            json!({"provider":"hyperliquid","name":name,"owner":owner.to_ascii_lowercase(),"execution_scope":execution_scope})
        }
        ConnectionConfig::Binance { .. }
        | ConnectionConfig::Coinbase { .. }
        | ConnectionConfig::CoinbaseExchange { .. }
        | ConnectionConfig::CoinbaseInternational { .. } => public_connection(connection),
        ConnectionConfig::Kraken {
            name,
            resource,
            physical_identity,
            ..
        }
        | ConnectionConfig::KrakenFutures {
            name,
            resource,
            physical_identity,
            ..
        } => {
            json!({"provider":if matches!(connection,ConnectionConfig::Kraken{..}){"kraken"}else{"krakenfutures"},"name":name,"resource":resource,"physical_identity":physical_identity})
        }
        ConnectionConfig::CoinGecko { name, resource, .. } => {
            json!({"provider":"coingecko","name":name,"resource":resource})
        }
        ConnectionConfig::Solana {
            name,
            address,
            network,
            ..
        } => {
            json!({"provider":"solana","name":name,"address":address,"network":network})
        }
        ConnectionConfig::Jupiter { .. } | ConnectionConfig::Relay { .. } => {
            public_connection(connection)
        }
    }
}
fn connection_resources(connection: &ConnectionConfig, config: &BrokerConfigV2) -> Vec<ResourceId> {
    match connection {
        ConnectionConfig::Evm {
            address, resource, ..
        } => {
            let mut resources = vec![resource.clone()];
            resources.extend(config.networks.iter().map(|network| {
                ResourceId(format!(
                    "ethereum:{}:{}",
                    network.chain_id,
                    address.to_ascii_lowercase()
                ))
            }));
            resources.push(ResourceId(format!(
                "hyperliquid:mainnet:{}:account:primary",
                address.to_ascii_lowercase()
            )));
            resources
        }
        _ => vec![resource(connection).clone()],
    }
}
fn outcome(
    action: &str,
    connection: Option<&ConnectionConfig>,
    checks: Vec<Value>,
    warnings: Vec<String>,
    cleanup_warning: Option<&str>,
) -> Value {
    use gofin_discovery::enrollment_contract::EnrollmentOutcome;
    let connection = connection.map(public_connection_contract);
    serde_json::to_value(EnrollmentOutcome {
        action: action.into(),
        connection,
        checks,
        warnings,
        cleanup_warning: cleanup_warning.map(str::to_owned),
        configuration_authorized: true,
        financial_movement: false,
        future_financial_authority: false,
        next_action: if action == "initialize" {
            "Run gofin connections add yourself in a fresh human Terminal."
        } else {
            "Run gofin connections list --json and gofin doctor --json."
        }
        .into(),
    })
    .expect("safe enrollment result serialization")
}

fn public_connection_contract(
    connection: &ConnectionConfig,
) -> gofin_discovery::enrollment_contract::PublicConnection {
    use gofin_discovery::enrollment_contract::PublicConnection as Public;
    match connection {
        ConnectionConfig::Coinbase {
            name,
            resource,
            physical_identity,
            physical_identity_verified,
            portfolio_id,
            ..
        } => Public::Coinbase {
            name: name.clone(),
            resource: resource.clone(),
            physical_identity: physical_identity.clone(),
            identity_verification: if *physical_identity_verified {
                "venue_verified"
            } else {
                "human_confirmed"
            }
            .into(),
            portfolio_id: portfolio_id.clone(),
        },
        ConnectionConfig::CoinbaseExchange {
            name,
            resource,
            physical_identity,
            physical_identity_verified,
            portfolio_id,
            ..
        } => Public::CoinbaseExchange {
            name: name.clone(),
            resource: resource.clone(),
            physical_identity: physical_identity.clone(),
            identity_verification: if *physical_identity_verified {
                "venue_verified"
            } else {
                "human_confirmed"
            }
            .into(),
            portfolio_id: portfolio_id.clone(),
        },
        ConnectionConfig::CoinbaseInternational {
            name,
            resource,
            physical_identity,
            physical_identity_verified,
            portfolio_id,
            ..
        } => Public::CoinbaseInternational {
            name: name.clone(),
            resource: resource.clone(),
            physical_identity: physical_identity.clone(),
            identity_verification: if *physical_identity_verified {
                "venue_verified"
            } else {
                "human_confirmed"
            }
            .into(),
            portfolio_id: portfolio_id.clone(),
        },
        ConnectionConfig::Kraken {
            name,
            resource,
            physical_identity,
            physical_identity_verified,
            default_wallet_account_id,
            ..
        } => Public::Kraken {
            name: name.clone(),
            resource: resource.clone(),
            physical_identity: physical_identity.clone(),
            identity_verification: if *physical_identity_verified {
                "venue_verified"
            } else {
                "human_confirmed"
            }
            .into(),
            default_wallet_account_id: default_wallet_account_id.clone(),
            default_wallet_relationship_verification: default_wallet_account_id
                .as_ref()
                .map(|_| "human_verified".into()),
        },
        ConnectionConfig::KrakenFutures {
            name,
            resource,
            physical_identity,
            physical_identity_verified,
            ..
        } => Public::KrakenFutures {
            name: name.clone(),
            resource: resource.clone(),
            physical_identity: physical_identity.clone(),
            identity_verification: if *physical_identity_verified {
                "venue_verified"
            } else {
                "human_confirmed"
            }
            .into(),
        },
        ConnectionConfig::CoinGecko {
            name,
            resource,
            api_plan,
            ..
        } => Public::CoinGecko {
            name: name.clone(),
            resource: resource.clone(),
            api_plan: api_plan.clone(),
        },
        ConnectionConfig::Binance {
            name,
            resource,
            role,
            parent,
            email,
            wallet_type,
            physical_identity,
            ..
        } => Public::Binance {
            name: name.clone(),
            resource: resource.clone(),
            role: role.clone(),
            parent: parent.clone(),
            email: email.clone(),
            wallet_type: wallet_type.clone(),
            physical_identity: physical_identity.clone(),
        },
        ConnectionConfig::Evm {
            name,
            resource,
            address,
            signing_enabled,
            ..
        } => Public::Evm {
            name: name.clone(),
            resource: resource.clone(),
            address: address.clone(),
            signing_enabled: *signing_enabled,
        },
        ConnectionConfig::Ethereum {
            name,
            resource,
            network,
            address,
            signing_enabled,
            ..
        } => Public::Ethereum {
            name: name.clone(),
            resource: resource.clone(),
            network: network.clone(),
            address: address.clone(),
            signing_enabled: *signing_enabled,
        },
        ConnectionConfig::Hyperliquid {
            name,
            resource,
            owner,
            execution_scope,
            signing_enabled,
            ..
        } => Public::Hyperliquid {
            name: name.clone(),
            resource: resource.clone(),
            owner: owner.clone(),
            execution_address: execution_scope.address().unwrap_or(owner).to_owned(),
            execution_scope: execution_scope.kind().into(),
            signing_enabled: *signing_enabled,
        },
        ConnectionConfig::Solana {
            name,
            resource,
            address,
            network,
            signing_enabled,
            ..
        } => Public::Solana {
            name: name.clone(),
            resource: resource.clone(),
            address: address.clone(),
            network: network.clone(),
            signing_enabled: *signing_enabled,
        },
        ConnectionConfig::Jupiter { name, resource, .. } => Public::Jupiter {
            name: name.clone(),
            resource: resource.clone(),
            credential_configured: true,
        },
        ConnectionConfig::Relay { name, resource, .. } => Public::Relay {
            name: name.clone(),
            resource: resource.clone(),
            credential_configured: true,
        },
    }
}
fn reject_kraken_account_alias(
    config: &BrokerConfigV2,
    name: &str,
    futures: bool,
    supplied: &str,
    data: &Value,
) -> Result<()> {
    let native = data.get("result").filter(|v| v.is_object()).unwrap_or(data);
    let mut identities = std::collections::BTreeSet::new();
    for id in std::iter::once(Some(supplied))
        .chain([
            native["accountUid"].as_str(),
            native["iiban"].as_str(),
            native["iban"].as_str(),
        ])
        .flatten()
    {
        identities.insert(gofin_kraken::contracts::canonical_account_identity(id));
    }
    for connection in &config.connections {
        let identity = match connection {
            ConnectionConfig::Kraken {
                name: existing,
                physical_identity,
                ..
            } if !futures && existing != name => physical_identity,
            ConnectionConfig::KrakenFutures {
                name: existing,
                physical_identity,
                ..
            } if futures && existing != name => physical_identity,
            _ => continue,
        };
        if identities.contains(&gofin_kraken::contracts::canonical_account_identity(
            identity,
        )) {
            return Err(error(
                "KRAKEN_DUPLICATE_CUSTODY_ALIAS",
                "This product account is already enrolled under another name or native account identifier. Use or update that connection to avoid duplicate custody accounting.",
            ));
        }
    }
    Ok(())
}

fn verify_kraken_identity(data: &Value, futures: bool, supplied: &str) -> Result<bool> {
    let normalized = |id: &str| {
        id.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    let supplied_normalized = normalized(supplied);
    let result = data.get("result").filter(|v| v.is_object()).unwrap_or(data);
    let reported = result
        .get("iiban")
        .or_else(|| result.get("iban"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if futures {
        let uid = result["accountUid"]
            .as_str()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .ok_or_else(|| {
                error(
                    "KRAKEN_INVALID_RESPONSE",
                    "The Derivatives credential check omitted its authoritative account UID.",
                )
            })?;
        if let Ok(selected_uid) = uuid::Uuid::parse_str(supplied) {
            if selected_uid != uid {
                return Err(error(
                    "KRAKEN_IDENTITY_MISMATCH",
                    "The Derivatives key belongs to another account UID.",
                ));
            }
            return Ok(true);
        }
    }
    if let Some(reported) = reported {
        if normalized(reported) != supplied_normalized {
            return Err(error(
                "KRAKEN_IDENTITY_MISMATCH",
                "The key's reported account identity differs from --account-id.",
            ));
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::AssetId;

    fn empty_config(
        networks: Vec<NetworkConfig>,
        deployments: Vec<DeploymentConfig>,
    ) -> BrokerConfigV2 {
        BrokerConfigV2 {
            version: 2,
            approver_uid: 501,
            networks,
            deployments,
            connections: vec![],
            policy: BrokerPolicy::beta(),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn reviewed_evm_expansion_adds_missing_network_and_deployment_coverage() {
        let mut config = empty_config(vec![NetworkConfig::ethereum_mainnet()], vec![]);
        let warnings = expand_reviewed_evm_coverage(&mut config);
        assert!(
            config
                .networks
                .iter()
                .any(|network| network.chain_id == 42161)
        );
        assert!(
            config
                .networks
                .iter()
                .any(|network| network.chain_id == 4663)
        );
        for network in [
            "ethereum:1",
            "ethereum:8453",
            "ethereum:42161",
            "ethereum:4663",
        ] {
            let deployment = config
                .deployments
                .iter()
                .find(|deployment| deployment.network == network)
                .unwrap();
            assert!(deployment.uniswap.as_ref().unwrap().v4.is_some());
            assert!(warnings.iter().any(|warning| warning.contains(network)));
        }
        let robinhood = config
            .deployments
            .iter()
            .find(|deployment| deployment.network == "ethereum:4663")
            .unwrap();
        let ramses = robinhood.ramses.as_ref().unwrap();
        assert_eq!(ramses.factory, "0xE0c4ceb92d08CA985bB70fe0a22fEb121A9854A8");
        assert_eq!(ramses.quoter, "0x4730e03EB4a58A5e20244062D5f9A99bCf5770a6");
        assert_eq!(ramses.router, "0xFCBBe2Af83F94e7E2a9C35a535B3A04719aFD2Ae");
        assert_eq!(
            ramses.pool_deployer,
            "0x4b37359BF291AbE8453692DB58d515a8b013Dca9"
        );
        assert!(warnings.iter().any(|warning| {
            warning.contains("Added reviewed Ramses v3 deployment coverage for ethereum:4663")
        }));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("Added reviewed EVM network"))
        );
        config.validate().unwrap();
    }

    #[test]
    fn reviewed_evm_expansion_preserves_existing_custom_v3_and_deployment_scope() {
        let reviewed = DeploymentConfig::reviewed_uniswap_networks()
            .into_iter()
            .find(|deployment| deployment.network == "ethereum:1")
            .unwrap();
        let mut custom_uniswap = reviewed.uniswap.unwrap();
        custom_uniswap.factory = "0x1111111111111111111111111111111111111111".into();
        custom_uniswap.quoter = "0x2222222222222222222222222222222222222222".into();
        custom_uniswap.router = "0x3333333333333333333333333333333333333333".into();
        custom_uniswap.v4 = None;
        let custom_aave = gofin_ethereum::AaveConfig {
            pool: "0x4444444444444444444444444444444444444444".into(),
            oracle: "0x5555555555555555555555555555555555555555".into(),
        };
        let deployment = DeploymentConfig {
            aave_markets: vec![],
            network: "ethereum:1".into(),
            weth: Some("0x6666666666666666666666666666666666666666".into()),
            snapshot_tokens: vec!["0x7777777777777777777777777777777777777777".into()],
            uniswap: Some(custom_uniswap.clone()),
            ramses: None,
            v3_deployments: BTreeMap::new(),
            aave: Some(custom_aave.clone()),
        };
        let mut config = empty_config(
            NetworkConfig::reviewed_evm_networks(),
            vec![deployment.clone()],
        );
        let warnings = expand_reviewed_evm_coverage(&mut config);
        let updated = config
            .deployments
            .iter()
            .find(|deployment| deployment.network == "ethereum:1")
            .unwrap();
        let uniswap = updated.uniswap.as_ref().unwrap();
        assert_eq!(uniswap.factory, custom_uniswap.factory);
        assert_eq!(uniswap.quoter, custom_uniswap.quoter);
        assert_eq!(uniswap.router, custom_uniswap.router);
        assert_eq!(uniswap.router_kind, custom_uniswap.router_kind);
        assert!(uniswap.v4.is_some());
        assert_eq!(updated.weth, deployment.weth);
        assert_eq!(updated.snapshot_tokens, deployment.snapshot_tokens);
        assert_eq!(updated.aave.as_ref().unwrap().pool, custom_aave.pool);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("preserving the existing v3"))
        );
    }

    #[test]
    fn base_coverage_preserves_custom_deployments_and_adds_missing_protocols() {
        let mut config = empty_config(
            NetworkConfig::reviewed_evm_networks(),
            DeploymentConfig::reviewed_amm_networks(),
        );
        let base = config
            .deployments
            .iter_mut()
            .find(|d| d.network == "ethereum:8453")
            .unwrap();
        base.uniswap.as_mut().unwrap().router = "0x1111111111111111111111111111111111111111".into();
        base.weth = Some("0x2222222222222222222222222222222222222222".into());
        base.aave.as_mut().unwrap().pool = "0x3333333333333333333333333333333333333333".into();
        let original = serde_json::to_value(&config.deployments).unwrap();
        assert!(expand_reviewed_evm_coverage(&mut config).is_empty());
        assert_eq!(serde_json::to_value(&config.deployments).unwrap(), original);

        let base = config
            .deployments
            .iter_mut()
            .find(|d| d.network == "ethereum:8453")
            .unwrap();
        base.weth = None;
        base.aave = None;
        assert_eq!(expand_reviewed_evm_coverage(&mut config).len(), 2);
        let base = config
            .deployments
            .iter()
            .find(|d| d.network == "ethereum:8453")
            .unwrap();
        assert_eq!(
            base.weth.as_deref(),
            Some("0x4200000000000000000000000000000000000006")
        );
        assert!(base.aave.is_some());
        assert_eq!(
            base.uniswap.as_ref().unwrap().router,
            "0x1111111111111111111111111111111111111111"
        );
        assert!(expand_reviewed_evm_coverage(&mut config).is_empty());
        config.validate().unwrap();
    }

    #[test]
    fn reviewed_evm_expansion_is_idempotent_and_keeps_custom_v4() {
        let mut reviewed = DeploymentConfig::reviewed_uniswap_networks();
        let mainnet = reviewed
            .iter_mut()
            .find(|deployment| deployment.network == "ethereum:1")
            .unwrap();
        let custom_v4 = mainnet.uniswap.as_mut().unwrap().v4.as_mut().unwrap();
        custom_v4.router = "0x8888888888888888888888888888888888888888".into();
        let mut config = empty_config(NetworkConfig::reviewed_evm_networks(), reviewed);
        let before = serde_json::to_value(&config.deployments).unwrap();
        assert!(expand_reviewed_evm_coverage(&mut config).is_empty());
        assert_eq!(serde_json::to_value(&config.deployments).unwrap(), before);
        let again = expand_reviewed_evm_coverage(&mut config);
        assert!(again.is_empty());
    }

    #[test]
    fn reviewed_amm_profiles_add_ramses_only_on_robinhood_and_preserve_custom_profile() {
        let profiles = DeploymentConfig::reviewed_amm_networks();
        assert!(
            profiles
                .iter()
                .find(|deployment| deployment.network == "ethereum:1")
                .unwrap()
                .ramses
                .is_none()
        );
        assert!(
            profiles
                .iter()
                .find(|deployment| deployment.network == "ethereum:42161")
                .unwrap()
                .ramses
                .is_none()
        );
        assert!(
            profiles
                .iter()
                .find(|deployment| deployment.network == "ethereum:4663")
                .unwrap()
                .ramses
                .is_some()
        );

        let custom = gofin_ethereum::RamsesConfig {
            factory: "0x1111111111111111111111111111111111111111".into(),
            quoter: "0x2222222222222222222222222222222222222222".into(),
            router: "0x3333333333333333333333333333333333333333".into(),
            pool_deployer: "0x4444444444444444444444444444444444444444".into(),
        };
        let mut profiles = profiles;
        profiles
            .iter_mut()
            .find(|deployment| deployment.network == "ethereum:4663")
            .unwrap()
            .ramses = Some(custom.clone());
        let mut config = empty_config(NetworkConfig::reviewed_evm_networks(), profiles);
        assert!(expand_reviewed_evm_coverage(&mut config).is_empty());
        assert_eq!(
            config
                .deployments
                .iter()
                .find(|deployment| deployment.network == "ethereum:4663")
                .unwrap()
                .ramses
                .as_ref()
                .unwrap()
                .router,
            custom.router
        );
    }

    #[test]
    fn legacy_deployment_without_ramses_deserializes_and_omits_none_when_saved() {
        let legacy = json!({
            "network":"ethereum:4663",
            "weth":null,
            "snapshot_tokens":[],
            "uniswap":null,
            "aave":null
        });
        let decoded: DeploymentConfig = serde_json::from_value(legacy).unwrap();
        assert!(decoded.ramses.is_none());
        assert!(
            serde_json::to_value(decoded)
                .unwrap()
                .get("ramses")
                .is_none()
        );
    }

    fn reusable_read_only_wallet() -> ConnectionConfig {
        ConnectionConfig::Evm {
            name: "wallet".into(),
            resource: ResourceId("evm:0x1111111111111111111111111111111111111111".into()),
            credential_ref: None,
            address: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: false,
        }
    }

    #[test]
    fn shipped_rpc_defaults_gain_backups_without_rewriting_stored_config_or_network_scope() {
        let mut stored = empty_config(NetworkConfig::reviewed_evm_networks(), vec![]);
        for network in &mut stored.networks {
            if network.chain_id != 1 {
                network.rpc_urls.truncate(1); // Previously shipped single-RPC rollup defaults.
                network.confirmations = 9;
                network.snapshot_tokens = vec!["0x1111111111111111111111111111111111111111".into()];
            }
        }
        stored.policy.max_movement_usd_micros = "50000000".into();
        let original = serde_json::to_value(&stored).unwrap();
        let effective = effective_protected_config(stored.clone()).unwrap();
        assert_eq!(effective.networks.len(), stored.networks.len());
        for (old, new) in stored.networks.iter().zip(&effective.networks) {
            let expected = match old.chain_id {
                1 => vec![
                    "https://ethereum-rpc.publicnode.com",
                    "https://eth.drpc.org",
                ],
                8453 => vec!["https://mainnet.base.org", "https://base.drpc.org"],
                42161 => vec![
                    "https://arb1.arbitrum.io/rpc",
                    "https://arb-one.api.pocket.network",
                    "https://arb-mainnet.g.alchemy.com/public",
                ],
                4663 => vec![
                    "https://rpc.mainnet.chain.robinhood.com",
                    "https://robinhood-rpc.publicnode.com",
                ],
                _ => old.rpc_urls.iter().map(String::as_str).collect(),
            };
            assert_eq!(new.rpc_urls, expected);
            assert_eq!(new.confirmations, old.confirmations);
            assert_eq!(new.snapshot_tokens, old.snapshot_tokens);
            assert_eq!(new.id, old.id);
        }
        assert_eq!(effective.policy, stored.policy);
        assert_eq!(effective.connections, stored.connections);
        assert_eq!(effective.approver_uid, stored.approver_uid);
        assert_eq!(serde_json::to_value(&stored).unwrap(), original);
        let again = effective_protected_config(effective.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(again).unwrap(),
            serde_json::to_value(effective).unwrap()
        );
    }

    #[test]
    fn custom_rpc_lists_and_provider_order_remain_authoritative() {
        for chain_id in [1, 8453, 42161, 4663, 10, 137, 43114, 5042, 999] {
            let reviewed = NetworkConfig::reviewed_evm_networks()
                .into_iter()
                .find(|network| network.chain_id == chain_id)
                .unwrap();
            for urls in [
                vec!["https://custom.example/rpc".to_owned()],
                vec![
                    reviewed.rpc_urls[0].clone(),
                    "https://custom.example/backup".into(),
                ],
                reviewed.rpc_urls.iter().rev().cloned().collect(),
            ] {
                let mut network = reviewed.clone();
                network.rpc_urls = urls;
                let stored = empty_config(vec![network], vec![]);
                let original = serde_json::to_value(&stored).unwrap();
                let effective = effective_protected_config(stored).unwrap();
                assert_eq!(serde_json::to_value(effective).unwrap(), original);
            }
        }
    }

    #[test]
    fn effective_protected_config_adds_reviewed_coverage_without_touching_custom_data() {
        let reviewed_networks = NetworkConfig::reviewed_evm_networks();
        let mut custom_mainnet = NetworkConfig::ethereum_mainnet();
        custom_mainnet.rpc_urls = vec!["https://custom.example/rpc".into()];
        let mut custom_rh = reviewed_networks
            .iter()
            .find(|network| network.chain_id == 4663)
            .unwrap()
            .clone();
        custom_rh.rpc_urls = vec!["https://rh-custom.example/rpc".into()];
        let reviewed_mainnet = DeploymentConfig::reviewed_amm_networks()
            .into_iter()
            .find(|deployment| deployment.network == "ethereum:1")
            .unwrap();
        let mut custom_uniswap = reviewed_mainnet.uniswap.unwrap();
        custom_uniswap.factory = "0x1111111111111111111111111111111111111111".into();
        custom_uniswap.router = "0x2222222222222222222222222222222222222222".into();
        custom_uniswap.quoter = "0x3333333333333333333333333333333333333333".into();
        custom_uniswap.v4 = None;
        let custom_ramses = gofin_ethereum::RamsesConfig {
            factory: "0x4444444444444444444444444444444444444444".into(),
            quoter: "0x5555555555555555555555555555555555555555".into(),
            router: "0x6666666666666666666666666666666666666666".into(),
            pool_deployer: "0x7777777777777777777777777777777777777777".into(),
        };
        let mut config = empty_config(
            vec![custom_mainnet.clone(), custom_rh.clone()],
            vec![
                DeploymentConfig {
                    aave_markets: vec![],
                    network: "ethereum:1".into(),
                    weth: Some("0x8888888888888888888888888888888888888888".into()),
                    snapshot_tokens: vec!["0x9999999999999999999999999999999999999999".into()],
                    uniswap: Some(custom_uniswap.clone()),
                    ramses: None,
                    v3_deployments: BTreeMap::new(),
                    aave: Some(gofin_ethereum::EthereumConfig::default().aave.unwrap()),
                },
                DeploymentConfig {
                    aave_markets: vec![],
                    network: "ethereum:4663".into(),
                    weth: None,
                    snapshot_tokens: vec![],
                    uniswap: None,
                    ramses: Some(custom_ramses.clone()),
                    v3_deployments: BTreeMap::new(),
                    aave: None,
                },
            ],
        );
        config.connections.push(reusable_read_only_wallet());
        config.connections.push(ConnectionConfig::Evm {
            name: "synthetic-signer".into(),
            resource: ResourceId("evm:0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            credential_ref: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            address: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            signing_enabled: true,
        });
        config.extensions.insert(
            "preserved".into(),
            json!({"journal_identity":"pending-123"}),
        );
        let original_connections = config.connections.clone();
        let original_policy = config.policy.clone();
        let original_extensions = config.extensions.clone();

        let reviewed_amms = DeploymentConfig::reviewed_amm_networks();
        let first =
            effective_protected_config_with(config, &reviewed_networks, &reviewed_amms).unwrap();
        first.validate().unwrap();
        assert_eq!(
            &first.connections[..original_connections.len()],
            &original_connections
        );
        let derived = first.connections[original_connections.len()..]
            .iter()
            .filter_map(|connection| match connection {
                ConnectionConfig::Hyperliquid {
                    name,
                    owner,
                    credential_ref,
                    signing_enabled,
                    auto_derived,
                    ..
                } => Some((name, owner, credential_ref, signing_enabled, auto_derived)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(derived.len(), 2);
        assert_eq!(derived[0].0, "wallet-hyperliquid");
        assert_eq!(derived[0].1, "0x1111111111111111111111111111111111111111");
        assert_eq!(derived[0].2, &None);
        assert!(!derived[0].3);
        assert!(*derived[0].4);
        assert_eq!(derived[1].0, "synthetic-signer-hyperliquid");
        assert_eq!(
            derived[1].2.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert!(*derived[1].3);
        assert!(*derived[1].4);
        assert_eq!(first.policy, original_policy);
        assert_eq!(first.extensions, original_extensions);
        assert_eq!(first.approver_uid, 501);
        assert_eq!(
            first.networks.iter().find(|n| n.chain_id == 1).unwrap(),
            &custom_mainnet
        );
        assert_eq!(
            first.networks.iter().find(|n| n.chain_id == 4663).unwrap(),
            &custom_rh
        );
        assert!(first.networks.iter().any(|n| n.chain_id == 42161));
        let base = first.networks.iter().find(|n| n.chain_id == 8453).unwrap();
        assert_eq!(
            base.rpc_urls,
            ["https://mainnet.base.org", "https://base.drpc.org"]
        );
        let base_deployment = first
            .deployments
            .iter()
            .find(|d| d.network == "ethereum:8453")
            .unwrap();
        assert!(base_deployment.uniswap.as_ref().unwrap().v4.is_some());
        assert!(base_deployment.aave.is_some());
        assert_eq!(
            base_deployment.weth.as_deref(),
            Some("0x4200000000000000000000000000000000000006")
        );
        let mainnet = first
            .deployments
            .iter()
            .find(|d| d.network == "ethereum:1")
            .unwrap();
        assert_eq!(
            mainnet.uniswap.as_ref().unwrap().factory,
            custom_uniswap.factory
        );
        assert!(mainnet.uniswap.as_ref().unwrap().v4.is_some());
        assert_eq!(
            mainnet.weth.as_deref(),
            Some("0x8888888888888888888888888888888888888888")
        );
        assert_eq!(
            mainnet.snapshot_tokens,
            vec!["0x9999999999999999999999999999999999999999"]
        );
        assert_eq!(
            serde_json::to_value(
                first
                    .deployments
                    .iter()
                    .find(|d| d.network == "ethereum:4663")
                    .unwrap()
                    .ramses
                    .as_ref()
            )
            .unwrap(),
            serde_json::to_value(Some(&custom_ramses)).unwrap()
        );

        let mut persisted = first.clone();
        strip_derived_hyperliquid_links(&mut persisted);
        assert_eq!(persisted.connections, original_connections);
        let persisted_json = serde_json::to_value(&persisted).unwrap();
        assert!(!persisted_json.to_string().contains("auto_derived"));

        let serialized = serde_json::to_value(&first).unwrap();
        let second =
            effective_protected_config_with(first, &reviewed_networks, &reviewed_amms).unwrap();
        assert_eq!(serde_json::to_value(second).unwrap(), serialized);
    }

    #[test]
    fn effective_coverage_is_gated_to_reusable_wallets_and_keeps_legacy_scope() {
        let mut legacy_only = empty_config(
            vec![NetworkConfig::ethereum_mainnet()],
            vec![DeploymentConfig::ethereum_mainnet()],
        );
        let legacy = ConnectionConfig::Ethereum {
            name: "legacy-wallet".into(),
            resource: ResourceId("ethereum:1:0x2222222222222222222222222222222222222222".into()),
            credential_ref: None,
            network: "ethereum:1".into(),
            address: "0x2222222222222222222222222222222222222222".into(),
            signing_enabled: false,
            assets: vec![AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            }],
        };
        legacy_only.connections.push(legacy.clone());
        let original = serde_json::to_value(&legacy_only).unwrap();
        let unchanged = effective_protected_config(legacy_only).unwrap();
        assert_eq!(serde_json::to_value(unchanged).unwrap(), original);

        let mut mixed = empty_config(
            vec![NetworkConfig::ethereum_mainnet()],
            vec![DeploymentConfig::ethereum_mainnet()],
        );
        mixed.connections.push(legacy.clone());
        mixed.connections.push(reusable_read_only_wallet());
        let expanded = effective_protected_config(mixed).unwrap();
        assert_eq!(expanded.connections[0], legacy);
        assert!(expanded.connections.iter().any(|connection| matches!(
            connection,
            ConnectionConfig::Hyperliquid { name, signing_enabled: false, auto_derived: true, .. }
                if name == "wallet-hyperliquid"
        )));
        assert!(
            expanded
                .networks
                .iter()
                .any(|network| network.chain_id == 4663)
        );
    }

    #[test]
    fn evm_hyperliquid_link_is_rebuilt_from_current_wallet_and_never_elevates_explicit_read_only() {
        let networks = NetworkConfig::reviewed_evm_networks();
        let deployments = DeploymentConfig::reviewed_amm_networks();
        let wallet = ConnectionConfig::Evm {
            name: "george-dev".into(),
            resource: ResourceId("evm:0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            credential_ref: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            address: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            signing_enabled: true,
        };
        let owner = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let explicit = ConnectionConfig::Hyperliquid {
            name: "manual-read-only".into(),
            resource: ResourceId(format!("hyperliquid:mainnet:{owner}:account:primary")),
            owner: owner.into(),
            credential_ref: None,
            signing_enabled: false,
            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            auto_derived: false,
        };
        let mut config = empty_config(networks.clone(), deployments.clone());
        config
            .connections
            .extend([wallet.clone(), explicit.clone()]);
        let linked = effective_protected_config_with(config, &networks, &deployments).unwrap();
        assert_eq!(linked.connections.len(), 2);
        assert_eq!(linked.connections[1], explicit);
        assert!(matches!(
            &linked.connections[1],
            ConnectionConfig::Hyperliquid {
                signing_enabled: false,
                credential_ref: None,
                auto_derived: false,
                ..
            }
        ));

        let shared_reference = "550e8400-e29b-41d4-a716-446655440000";
        let mut shared = empty_config(networks.clone(), deployments.clone());
        let signed_wallet = ConnectionConfig::Evm {
            name: "wallet".into(),
            resource: ResourceId(format!("evm:{owner}")),
            credential_ref: Some(shared_reference.into()),
            address: owner.into(),
            signing_enabled: true,
        };
        let signed_hl = ConnectionConfig::Hyperliquid {
            name: "manual-hl".into(),
            resource: ResourceId(format!("hyperliquid:mainnet:{owner}:account:primary")),
            owner: owner.into(),
            credential_ref: Some(shared_reference.into()),
            signing_enabled: true,
            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            auto_derived: false,
        };
        shared
            .connections
            .extend([signed_wallet.clone(), signed_hl]);
        shared
            .connections
            .retain(|connection| name(connection) != "wallet");
        assert!(referenced(&shared).contains(shared_reference));
        assert!(matches!(
            effective_protected_config_with(shared, &networks, &deployments)
                .unwrap()
                .connections
                .as_slice(),
            [_]
        ));

        let replaced_wallet = match wallet {
            ConnectionConfig::Evm {
                name,
                resource,
                address,
                ..
            } => ConnectionConfig::Evm {
                name,
                resource,
                address,
                credential_ref: Some("550e8400-e29b-41d4-a716-446655440001".into()),
                signing_enabled: true,
            },
            _ => unreachable!(),
        };
        let mut no_explicit = empty_config(networks.clone(), deployments.clone());
        no_explicit.connections.push(replaced_wallet);
        let linked = effective_protected_config_with(no_explicit, &networks, &deployments).unwrap();
        let generated = linked
            .connections
            .iter()
            .find_map(|connection| match connection {
                ConnectionConfig::Hyperliquid {
                    auto_derived: true,
                    credential_ref,
                    ..
                } => credential_ref.as_deref(),
                _ => None,
            });
        assert_eq!(generated, Some("550e8400-e29b-41d4-a716-446655440001"));

        let long_alias = hyperliquid_alias_name(&"wallet".repeat(20), owner);
        assert_eq!(
            long_alias,
            hyperliquid_alias_name(&"wallet".repeat(20), owner)
        );
        assert!(long_alias.len() <= 64);
        assert!(long_alias.ends_with("-hyperliquid"));
        assert!(hyperliquid_alias_name(&"é".repeat(80), owner).len() <= 64);

        let mut collision = empty_config(networks.clone(), deployments.clone());
        collision.connections.push(ConnectionConfig::Evm {
            name: "george-dev".into(),
            resource: ResourceId("evm:0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            credential_ref: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            address: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            signing_enabled: true,
        });
        collision.connections.push(ConnectionConfig::Hyperliquid {
            name: "george-dev-hyperliquid".into(),
            resource: ResourceId(
                "hyperliquid:mainnet:0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:account:primary"
                    .into(),
            ),
            owner: "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            credential_ref: None,
            signing_enabled: false,
            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            auto_derived: false,
        });
        assert_eq!(
            effective_protected_config_with(collision.clone(), &networks, &deployments)
                .unwrap_err()
                .code,
            "HYPERLIQUID_AUTO_LINK_COLLISION"
        );
        assert_eq!(
            enrollment_config_for_action("add", collision.clone())
                .unwrap_err()
                .code,
            "HYPERLIQUID_AUTO_LINK_COLLISION"
        );
        let mut removable = enrollment_config_for_action("remove", collision).unwrap();
        removable
            .connections
            .retain(|connection| name(connection) != "george-dev-hyperliquid");
        assert!(effective_protected_config_with(removable, &networks, &deployments).is_ok());
    }

    #[test]
    fn effective_overlay_rejects_reviewed_deployments_with_dangling_networks() {
        let mut config = empty_config(
            vec![NetworkConfig::ethereum_mainnet()],
            vec![DeploymentConfig::ethereum_mainnet()],
        );
        config.connections.push(reusable_read_only_wallet());
        let dangling = DeploymentConfig {
            aave_markets: vec![],
            network: "ethereum:999999".into(),
            weth: None,
            snapshot_tokens: vec![],
            uniswap: None,
            ramses: Some(gofin_ethereum::RamsesConfig {
                factory: "0x1111111111111111111111111111111111111111".into(),
                quoter: "0x2222222222222222222222222222222222222222".into(),
                router: "0x3333333333333333333333333333333333333333".into(),
                pool_deployer: "0x4444444444444444444444444444444444444444".into(),
            }),
            v3_deployments: BTreeMap::new(),
            aave: None,
        };
        let error = effective_protected_config_with(
            config,
            &NetworkConfig::reviewed_evm_networks(),
            &[dangling],
        )
        .unwrap_err();
        assert_eq!(error.code, "CONFIG_DEPLOYMENT_INVALID");
    }

    fn pricing_connection(plan: &str, reference: &str) -> ConnectionConfig {
        ConnectionConfig::CoinGecko {
            name: "synthetic-pricing".into(),
            resource: ResourceId("market-data:coingecko".into()),
            credential_ref: reference.into(),
            api_plan: plan.into(),
        }
    }

    #[test]
    fn public_connection_mapping_preserves_expected_fields_for_every_provider() {
        let secret = "11111111-1111-4111-8111-111111111111";
        let cases = [
            (
                json!({"provider":"kraken","name":"spot","resource":"kraken:account:x","credential_ref":secret,"physical_identity":"x","physical_identity_verified":true,"default_wallet_account_id":"wallet-1"}),
                json!({"provider":"kraken","name":"spot","resource":"kraken:account:x","physical_identity":"x","identity_verification":"venue_verified","default_wallet_account_id":"wallet-1","default_wallet_relationship_verification":"human_verified"}),
            ),
            (
                json!({"provider":"krakenfutures","name":"futures","resource":"kraken:futures:x","credential_ref":secret,"physical_identity":"x","physical_identity_verified":false}),
                json!({"provider":"krakenfutures","name":"futures","resource":"kraken:futures:x","physical_identity":"x","identity_verification":"human_confirmed"}),
            ),
            (
                json!({"provider":"coingecko","name":"prices","resource":"market-data:coingecko","credential_ref":secret,"api_plan":"demo"}),
                json!({"provider":"coingecko","name":"prices","resource":"market-data:coingecko","api_plan":"demo"}),
            ),
            (
                json!({"provider":"binance","name":"main","resource":"binance:uid:x:SPOT","credential_ref":secret,"role":"main","parent":null,"email":null,"wallet_type":"SPOT","physical_identity":"x","physical_identity_verified":true}),
                json!({"provider":"binance","name":"main","resource":"binance:uid:x:SPOT","role":"main","parent":null,"email":null,"wallet_type":"SPOT","physical_identity":"x"}),
            ),
            (
                json!({"provider":"evm","name":"wallet","resource":"evm:wallet","credential_ref":secret,"address":"0x1111111111111111111111111111111111111111","signing_enabled":true}),
                json!({"provider":"evm","name":"wallet","resource":"evm:wallet","address":"0x1111111111111111111111111111111111111111","signing_enabled":true}),
            ),
            (
                json!({"provider":"ethereum","name":"legacy","resource":"ethereum:1:wallet","credential_ref":secret,"network":"ethereum:1","address":"0x1111111111111111111111111111111111111111","signing_enabled":false,"assets":[]}),
                json!({"provider":"ethereum","name":"legacy","resource":"ethereum:1:wallet","network":"ethereum:1","address":"0x1111111111111111111111111111111111111111","signing_enabled":false}),
            ),
            (
                json!({"provider":"hyperliquid","name":"hl","resource":"hyperliquid:wallet","owner":"0x1111111111111111111111111111111111111111","credential_ref":secret,"signing_enabled":true}),
                json!({"provider":"hyperliquid","name":"hl","resource":"hyperliquid:wallet","owner":"0x1111111111111111111111111111111111111111","execution_address":"0x1111111111111111111111111111111111111111","execution_scope":"owner","signing_enabled":true}),
            ),
            (
                json!({"provider":"solana","name":"sol","resource":"solana:mainnet-beta:wallet","address":"SyntheticAddress","credential_ref":null,"signing_enabled":false,"network":"solana:mainnet-beta"}),
                json!({"provider":"solana","name":"sol","resource":"solana:mainnet-beta:wallet","address":"SyntheticAddress","network":"solana:mainnet-beta","signing_enabled":false}),
            ),
            (
                json!({"provider":"solana","name":"sol-signer","resource":"solana:mainnet-beta:signer","address":"SyntheticSignerAddress","credential_ref":secret,"signing_enabled":true,"network":"solana:mainnet-beta"}),
                json!({"provider":"solana","name":"sol-signer","resource":"solana:mainnet-beta:signer","address":"SyntheticSignerAddress","network":"solana:mainnet-beta","signing_enabled":true}),
            ),
            (
                json!({"provider":"jupiter","name":"jupiter","resource":"jupiter:service","credential_ref":secret}),
                json!({"provider":"jupiter","name":"jupiter","resource":"jupiter:service","credential_configured":true}),
            ),
        ];
        for (raw, expected) in cases {
            let connection: ConnectionConfig = serde_json::from_value(raw).unwrap();
            let actual = public_connection(&connection);
            assert_eq!(actual, expected);
            serde_json::from_value::<gofin_discovery::enrollment_contract::PublicConnection>(
                actual.clone(),
            )
            .unwrap();
            assert!(!actual.to_string().contains(secret));
            assert!(actual.get("credential_ref").is_none());
        }
    }

    #[test]
    fn solana_and_jupiter_outcomes_are_typed_and_never_expose_protected_references() {
        let secret_ref = "11111111-1111-4111-8111-111111111111";
        let connections = [
            ConnectionConfig::Solana {
                name: "wallet-readonly".into(),
                resource: ResourceId("solana:mainnet-beta:wallet".into()),
                address: "SyntheticAddress".into(),
                credential_ref: None,
                signing_enabled: false,
                network: "solana:mainnet-beta".into(),
            },
            ConnectionConfig::Solana {
                name: "wallet-signer".into(),
                resource: ResourceId("solana:mainnet-beta:signer".into()),
                address: "SyntheticSignerAddress".into(),
                credential_ref: Some(secret_ref.into()),
                signing_enabled: true,
                network: "solana:mainnet-beta".into(),
            },
            ConnectionConfig::Jupiter {
                name: "jupiter-service".into(),
                resource: ResourceId("jupiter:service".into()),
                credential_ref: secret_ref.into(),
            },
        ];
        for connection in &connections {
            let output = outcome("add", Some(connection), vec![], vec![], None);
            serde_json::from_value::<gofin_discovery::enrollment_contract::EnrollmentOutcome>(
                output.clone(),
            )
            .unwrap();
            let public = &output["connection"];
            assert_eq!(public, &public_connection(connection));
            assert!(public.get("credential_ref").is_none());
            assert!(!output.to_string().contains(secret_ref));
            assert!(!output.to_string().contains("private_key"));
        }
        let readonly = outcome("add", Some(&connections[0]), vec![], vec![], None);
        assert_eq!(readonly["connection"]["signing_enabled"], false);
        let signer = outcome("add", Some(&connections[1]), vec![], vec![], None);
        assert_eq!(signer["connection"]["signing_enabled"], true);
        let jupiter = outcome("add", Some(&connections[2]), vec![], vec![], None);
        assert_eq!(jupiter["connection"]["credential_configured"], true);
    }

    fn pricing_config(connection: ConnectionConfig) -> BrokerConfigV2 {
        BrokerConfigV2 {
            version: 2,
            approver_uid: 501,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![connection],
            policy: BrokerPolicy::beta(),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn setup_collects_defaults_exact_limits_or_zero_before_approval() {
        for (inputs, expected) in [
            (["", ""], ["100000000", "100000000"]),
            (["1000.000001", "5000"], ["1000000001", "5000000000"]),
            (["0", "0"], ["0", "0"]),
            (["0", "5000"], ["0", "5000000000"]),
        ] {
            let mut index = 0;
            let policy = collect_setup_policy(|label| {
                assert!(label.contains("0 = no limit; Enter = 100"));
                let value = inputs[index].into();
                index += 1;
                Ok(value)
            })
            .unwrap();
            assert_eq!(policy.max_movement_usd_micros, expected[0]);
            assert_eq!(policy.max_run_allocation_usd_micros, expected[1]);
            assert!(!policy.require_run_for_writes);
            assert_eq!(policy.preview_ttl_seconds, 90);
        }
        for invalid in ["-1", "1e3", "0.0000001", "18446744073709.551616"] {
            assert!(collect_setup_policy(|_| Ok(invalid.into())).is_err());
        }
        assert_eq!(
            collect_setup_policy(|_| Err(error("ENROLLMENT_CANCELLED", "Synthetic cancellation")))
                .unwrap_err()
                .code,
            "ENROLLMENT_CANCELLED"
        );
    }

    #[test]
    fn setup_review_accepts_collected_limits_and_current_reviewed_deployments() {
        for inputs in [["50", "50"], ["", ""], ["0", "0"], ["1000.000001", "5000"]] {
            let mut inputs = inputs.into_iter();
            let policy = collect_setup_policy(|_| Ok(inputs.next().unwrap().into())).unwrap();
            let movement =
                gofin_domain::policy::format_limit(&policy.max_movement_usd_micros).unwrap();
            let allocation =
                gofin_domain::policy::format_limit(&policy.max_run_allocation_usd_micros).unwrap();
            let config = setup_configuration(501, policy);
            let binding = json!({
                "action":"initialize", "configuration":config,
                "financial_movement":false, "future_financial_authority":false
            });
            let text = presentation::review(&binding).unwrap();
            assert!(text.contains(&format!("Movement limit: {movement}")));
            assert!(text.contains(&format!("24-hour allocation limit: {allocation}")));
            assert!(text.contains("Every financial write still requires fresh Touch ID."));
        }
    }

    #[test]
    fn coingecko_configuration_and_public_outcomes_use_one_safe_identity() {
        let reference = "11111111-1111-4111-8111-111111111111";
        let connection = pricing_connection("demo", reference);
        let config = pricing_config(connection.clone());
        config.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&connection).unwrap()["provider"],
            "coingecko"
        );
        assert_eq!(name(&connection), "synthetic-pricing");
        assert_eq!(resource(&connection).0, "market-data:coingecko");
        assert_eq!(credential(&connection).unwrap().as_str(), reference);
        assert!(referenced(&config).contains(reference));
        assert_eq!(
            connection_resources(&connection, &config),
            vec![ResourceId("market-data:coingecko".into())]
        );
        let public = public_connection(&connection);
        assert_eq!(
            public,
            json!({"provider":"coingecko","name":"synthetic-pricing","resource":"market-data:coingecko","api_plan":"demo"})
        );
        for action in ["add", "update", "remove"] {
            let output = outcome(
                action,
                Some(&connection),
                vec![json!({"name":"CoinGecko API access","status":"passed"})],
                vec![],
                None,
            );
            assert_eq!(output["connection"], public);
            assert_eq!(output["configuration_authorized"], true);
            assert_eq!(output["financial_movement"], false);
            assert_eq!(output["future_financial_authority"], false);
            assert!(!output.to_string().contains(reference));
            assert!(output["connection"].get("credential_ref").is_none());
            assert!(output["connection"].get("signing_enabled").is_none());
            serde_json::from_value::<gofin_discovery::enrollment_contract::EnrollmentOutcome>(
                output,
            )
            .unwrap();
        }
    }

    #[test]
    fn coingecko_rotation_preserves_market_data_identity_and_rejects_duplicate_aliases() {
        let original = pricing_connection("demo", "11111111-1111-4111-8111-111111111111");
        let replacement = pricing_connection("pro", "22222222-2222-4222-8222-222222222222");
        assert_eq!(
            immutable_identity(&original),
            immutable_identity(&replacement)
        );
        assert_ne!(credential(&original), credential(&replacement));
        assert_ne!(
            config_hash(&pricing_config(original.clone())).unwrap(),
            config_hash(&pricing_config(replacement.clone())).unwrap()
        );
        let mut duplicate = replacement;
        if let ConnectionConfig::CoinGecko { name, .. } = &mut duplicate {
            *name = "synthetic-other-pricing".into();
        }
        let mut config = pricing_config(original.clone());
        config.connections.push(duplicate);
        assert_eq!(
            config.validate().unwrap_err().code,
            "CONFIG_CONNECTION_DUPLICATE"
        );
        for (field, value) in [
            ("api_plan", "unsupported"),
            ("resource", "market-data:other"),
            ("credential_ref", "not-a-credential-reference"),
        ] {
            let mut wrong = serde_json::to_value(&original).unwrap();
            wrong[field] = json!(value);
            let wrong: ConnectionConfig = serde_json::from_value(wrong).unwrap();
            assert_eq!(
                pricing_config(wrong).validate().unwrap_err().code,
                "CONFIG_PRICING_CONNECTION_INVALID"
            );
        }
    }

    #[test]
    fn coingecko_safe_requests_reject_key_and_approval_values_without_echo() {
        let base = json!({"provider":"coingecko","name":"synthetic-pricing","credential_type":"api_key","api_plan":"demo"});
        validate_add(&parse::<AddConnectionArgs>(base.clone()).unwrap()).unwrap();
        for field in [
            "api_key",
            "secret",
            "approval",
            "credential_ref",
            "signature",
        ] {
            let mut request = base.clone();
            request[field] = json!("SYNTHETIC-NOT-A-REAL-KEY");
            let failure = parse::<AddConnectionArgs>(request).unwrap_err();
            assert!(!failure.to_string().contains("SYNTHETIC-NOT-A-REAL-KEY"));
        }
        let material = SecretMaterial::CoinGeckoApiKey {
            api_key: "SYNTHETIC-NOT-A-REAL-KEY".into(),
            plan: "demo".into(),
        };
        material.validate_present().unwrap();
        assert!(!format!("{material:?}").contains("SYNTHETIC-NOT-A-REAL-KEY"));
    }

    #[test]
    fn enrollment_binance_diagnostic_preserves_only_typed_http_codes() {
        use gofin_binance::{HttpFailureKind, TransportError};
        for (status, binance_code, suffix) in [
            (401, Some(-2015), " [HTTP 401; Binance code -2015]"),
            (400, Some(-1022), " [HTTP 400; Binance code -1022]"),
            (403, None, " [HTTP 403]"),
        ] {
            let failure = binance_enrollment_error(
                "BINANCE_AUTHENTICATION_FAILED",
                TransportError::HttpFailure {
                    status,
                    binance_code,
                    kind: HttpFailureKind::PermissionDenied,
                },
            );
            assert_eq!(failure.code, "BINANCE_AUTHENTICATION_FAILED");
            assert_eq!(failure.stage, "enrollment");
            assert_eq!(
                failure.message,
                format!("Binance request permission denied{suffix}")
            );
        }
    }

    #[test]
    fn enrollment_binance_diagnostic_distinguishes_network_and_local_signing() {
        use gofin_binance::TransportError;
        for (cause, expected) in [
            (TransportError::Timeout, "Binance request timed out"),
            (TransportError::Unavailable, "Binance request unavailable"),
            (
                TransportError::PermissionDenied,
                "The protected Binance credential could not be loaded or used for signing",
            ),
        ] {
            let failure = binance_enrollment_error("BINANCE_AUTHENTICATION_FAILED", cause);
            assert_eq!(failure.message, expected);
            assert!(!failure.message.contains("HTTP"));
        }
    }

    #[test]
    fn key_rotation_preserves_physical_identity_and_relationships() {
        let original = ConnectionConfig::Binance {
            name: "child".into(),
            resource: ResourceId("binance:uid:2:SPOT".into()),
            credential_ref: "SYNTHETIC-OLD-REFERENCE".into(),
            role: "sub".into(),
            parent: Some("main".into()),
            email: Some("synthetic@example.invalid".into()),
            wallet_type: "SPOT".into(),
            physical_identity: "2".into(),
            physical_identity_verified: true,
        };
        let mut replacement = original.clone();
        if let ConnectionConfig::Binance { credential_ref, .. } = &mut replacement {
            *credential_ref = "SYNTHETIC-NEW-REFERENCE".into();
        }
        assert_eq!(
            immutable_identity(&original),
            immutable_identity(&replacement)
        );
        if let ConnectionConfig::Binance { parent, .. } = &mut replacement {
            *parent = Some("other-main".into());
        }
        assert_ne!(
            immutable_identity(&original),
            immutable_identity(&replacement)
        );
        let output = outcome("update", Some(&original), vec![], vec![], None);
        assert!(!output.to_string().contains("SYNTHETIC-OLD-REFERENCE"));
        assert_eq!(output["financial_movement"], false);
        assert_eq!(output["future_financial_authority"], false);
        assert!(
            serde_json::from_value::<gofin_discovery::enrollment_contract::EnrollmentOutcome>(
                output
            )
            .is_ok()
        );
    }
    #[test]
    fn legacy_wallet_upgrade_retains_identity_and_checks_each_chain_for_pending_writes() {
        let address = "0x1111111111111111111111111111111111111111";
        let legacy = ConnectionConfig::Ethereum {
            name: "wallet".into(),
            resource: ResourceId(format!("ethereum:1:{address}")),
            network: "ethereum:1".into(),
            address: address.into(),
            credential_ref: None,
            signing_enabled: false,
            assets: vec![],
        };
        let shared = ConnectionConfig::Evm {
            name: "wallet".into(),
            resource: ResourceId(format!("evm:{address}")),
            address: address.into(),
            credential_ref: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            signing_enabled: true,
        };
        assert_eq!(immutable_identity(&legacy), immutable_identity(&shared));
        let config = BrokerConfigV2 {
            version: 2,
            approver_uid: 501,
            networks: NetworkConfig::reviewed_evm_networks(),
            deployments: vec![],
            connections: vec![shared.clone()],
            policy: BrokerPolicy::beta(),
            extensions: BTreeMap::new(),
        };
        config.validate().unwrap();
        let resources = connection_resources(&shared, &config);
        for chain in [1, 42161, 4663, 8453, 10, 137, 43114, 5042, 999] {
            assert!(resources.contains(&ResourceId(format!("ethereum:{chain}:{address}"))));
        }
        let output = outcome("update", Some(&shared), vec![], vec![], None);
        assert_eq!(output["connection"]["provider"], "evm");
        assert!(output["connection"].get("network").is_none());
        assert!(!output.to_string().contains("SYNTHETIC-KEY-REFERENCE"));
    }
    #[test]
    fn readonly_wallet_can_gain_same_address_signing_without_retargeting() {
        let original = ConnectionConfig::Ethereum {
            name: "wallet".into(),
            resource: ResourceId("ethereum:1:synthetic".into()),
            credential_ref: None,
            network: "ethereum:1".into(),
            address: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: false,
            assets: vec![],
        };
        let mut replacement = original.clone();
        if let ConnectionConfig::Ethereum {
            signing_enabled,
            credential_ref,
            ..
        } = &mut replacement
        {
            *signing_enabled = true;
            *credential_ref = Some("SYNTHETIC-NEW-REFERENCE".into());
        }
        assert_eq!(
            immutable_identity(&original),
            immutable_identity(&replacement)
        );
        if let ConnectionConfig::Ethereum { address, .. } = &mut replacement {
            *address = "0x2222222222222222222222222222222222222222".into();
        }
        assert_ne!(
            immutable_identity(&original),
            immutable_identity(&replacement)
        );
    }
    #[test]
    fn identity_requires_exact_positive_uid_and_spot_observation() {
        assert_eq!(
            account_uid(&json!({"uid":354937868,"accountType":"SPOT","balances":[]})).unwrap(),
            "354937868"
        );
        assert_eq!(
            account_uid(&json!({"uid":"184467440737095516150","accountType":"SPOT","balances":[]}))
                .unwrap(),
            "184467440737095516150"
        );
        for uid in [
            json!(null),
            json!(0),
            json!(-1),
            json!(1.1),
            json!("01"),
            json!("1e3"),
            json!("../key"),
        ] {
            assert!(account_uid(&json!({"uid":uid,"accountType":"SPOT","balances":[]})).is_err());
        }
        assert!(account_uid(&json!({"uid":1,"accountType":"MARGIN","balances":[]})).is_err());
    }
    #[test]
    fn safe_request_rejects_secret_and_authorization_fields_without_echo() {
        for field in [
            "secret",
            "api_key",
            "private_key",
            "approval",
            "credential_ref",
        ] {
            let result = parse::<AddConnectionArgs>(
                json!({"provider":"binance","name":"main","role":"main",field:"SYNTHETIC-SECRET"}),
            );
            assert!(!result.unwrap_err().to_string().contains("SYNTHETIC-SECRET"));
        }
        assert!(parse::<Initialize>(json!({"approver_uid":501,"extra":true})).is_err());
    }
    #[tokio::test]
    async fn uninstalled_agent_cannot_enter_enrollment() {
        let error = handle(
            "add",
            json!({"provider":"binance","name":"main","role":"main"}),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error.code.as_str(),
            "PROTECTED_BROKER_REQUIRED" | "INVALID_INSTALLATION" | "AUTHORIZATION_UNAVAILABLE"
        ));
    }
    #[test]
    fn reviewed_v3_fork_overlay_preserves_custom_profiles_and_credentials() {
        use gofin_ethereum::V3Dex;
        let mut deployment = DeploymentConfig::ethereum_mainnet();
        let mut custom = deployment.v3_deployments.remove(&V3Dex::Sushi).unwrap();
        custom.router = "0x1111111111111111111111111111111111111111".into();
        deployment.v3_deployments.clear();
        deployment
            .v3_deployments
            .insert(V3Dex::Sushi, custom.clone());
        let mut config = empty_config(vec![NetworkConfig::ethereum_mainnet()], vec![deployment]);
        config.connections.push(reusable_read_only_wallet());
        let original_connections = config.connections.clone();
        let effective = effective_protected_config(config.clone()).unwrap();
        // Main also derives Hyperliquid access from a reusable wallet. The
        // original wallet and credential mapping must remain byte-identical.
        let wallets: Vec<_> = effective
            .connections
            .iter()
            .filter(|connection| matches!(connection, ConnectionConfig::Evm { .. }))
            .cloned()
            .collect();
        assert_eq!(wallets, original_connections);
        let mainnet = effective
            .deployments
            .iter()
            .find(|d| d.network == "ethereum:1")
            .unwrap();
        assert_eq!(
            serde_json::to_value(&mainnet.v3_deployments[&V3Dex::Sushi]).unwrap(),
            serde_json::to_value(&custom).unwrap()
        );
        assert!(mainnet.v3_deployments.contains_key(&V3Dex::PancakeSwap));
        let robinhood = effective
            .deployments
            .iter()
            .find(|d| d.network == "ethereum:4663")
            .unwrap();
        assert!(!robinhood.v3_deployments.contains_key(&V3Dex::Sushi));
        assert!(robinhood.v3_deployments.contains_key(&V3Dex::PancakeSwap));
        let again = effective_protected_config(effective.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(again).unwrap(),
            serde_json::to_value(effective).unwrap()
        );
        assert!(
            !config.deployments[0]
                .v3_deployments
                .contains_key(&V3Dex::PancakeSwap)
        );
    }

    #[test]
    fn legacy_deployment_decoding_and_closed_fork_names_are_preserved() {
        let old = json!({"network":"ethereum:1","weth":null,"snapshot_tokens":[],"uniswap":null,"ramses":null,"aave":null});
        let deployment: DeploymentConfig = serde_json::from_value(old.clone()).unwrap();
        assert!(deployment.v3_deployments.is_empty());
        assert!(
            serde_json::to_value(&deployment)
                .unwrap()
                .get("v3_deployments")
                .is_none()
        );
        let mut unknown = old;
        unknown["v3_deployments"] = json!({"unreviewed":{}});
        assert!(serde_json::from_value::<DeploymentConfig>(unknown).is_err());
    }
    #[test]
    fn default_wallet_config_is_backward_compatible_public_and_cannot_retarget() {
        let raw = json!({"provider":"kraken","name":"spot","resource":"kraken:account:synthetic","credential_ref":"11111111-1111-4111-8111-111111111111","physical_identity":"synthetic","physical_identity_verified":false});
        let old: ConnectionConfig = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&old).unwrap(), raw);
        let mut args: AddConnectionArgs = serde_json::from_value(json!({"provider":"kraken","name":"spot","account_id":"synthetic","default_wallet_account_id":"WDEFAULT"})).unwrap();
        preserve_default_wallet(&mut args, Some(&old)).unwrap();
        let mut selected = raw.clone();
        selected["default_wallet_account_id"] = json!("WDEFAULT");
        let selected: ConnectionConfig = serde_json::from_value(selected).unwrap();
        args.default_wallet_account_id = None;
        preserve_default_wallet(&mut args, Some(&selected)).unwrap();
        assert_eq!(args.default_wallet_account_id.as_deref(), Some("WDEFAULT"));
        let public = public_connection(&selected);
        assert_eq!(public["default_wallet_account_id"], "WDEFAULT");
        assert_eq!(
            public["default_wallet_relationship_verification"],
            "human_verified"
        );
        assert!(public.get("credential_ref").is_none());
        serde_json::from_value::<gofin_discovery::enrollment_contract::PublicConnection>(public)
            .unwrap();
        args.default_wallet_account_id = Some("OTHER".into());
        assert!(preserve_default_wallet(&mut args, Some(&selected)).is_err());
        let mut futures = raw;
        futures["provider"] = json!("krakenfutures");
        futures["default_wallet_account_id"] = json!("WDEFAULT");
        assert!(serde_json::from_value::<ConnectionConfig>(futures).is_err());
    }

    #[test]
    fn kraken_product_identity_check_verifies_uuid_or_iiban_without_guessing_relationships() {
        let uid = "123e4567-e89b-42d3-a456-426614174000";
        let data =
            json!({"accountUid":uid,"iiban":"SYNTH ETIC","permissions":{"general":"READ_ONLY"}});
        assert!(verify_kraken_identity(&data, true, uid).unwrap());
        assert!(verify_kraken_identity(&data, true, "SYNTHETIC").unwrap());
        assert_eq!(
            verify_kraken_identity(&data, true, "00000000-0000-0000-0000-000000000000")
                .unwrap_err()
                .code,
            "KRAKEN_IDENTITY_MISMATCH"
        );
        assert_eq!(
            verify_kraken_identity(&data, true, "OTHER")
                .unwrap_err()
                .code,
            "KRAKEN_IDENTITY_MISMATCH"
        );
        assert!(
            !verify_kraken_identity(
                &json!({"accountUid":uid,"iiban":null}),
                true,
                "HUMAN-CONFIRMED"
            )
            .unwrap()
        );
        assert!(
            verify_kraken_identity(&json!({"result":{"iban":"SYNTH ETIC"}}), false, "SYNTHETIC")
                .unwrap()
        );
        assert_eq!(
            verify_kraken_identity(&json!({"result":{"accounts":{}}}), true, uid)
                .unwrap_err()
                .code,
            "KRAKEN_INVALID_RESPONSE"
        );
    }

    #[test]
    fn kraken_enrollment_refuses_native_uuid_iiban_and_spacing_aliases() {
        let uid = "123e4567-e89b-42d3-a456-426614174000";
        let mut config = pricing_config(pricing_connection(
            "demo",
            "11111111-1111-4111-8111-111111111111",
        ));
        config.connections.push(serde_json::from_value(json!({"provider":"krakenfutures","name":"existing","resource":"krakenfutures:original","credential_ref":"11111111-1111-4111-8111-111111111111","physical_identity":"SYNTH ETIC","physical_identity_verified":true})).unwrap());
        let data = json!({"accountUid":uid,"iiban":"SYNTHETIC"});
        assert_eq!(
            reject_kraken_account_alias(&config, "new", true, uid, &data)
                .unwrap_err()
                .code,
            "KRAKEN_DUPLICATE_CUSTODY_ALIAS"
        );
        assert!(reject_kraken_account_alias(&config, "existing", true, uid, &data).is_ok());
        assert!(
            reject_kraken_account_alias(
                &config,
                "spot",
                false,
                "SYNTHETIC",
                &json!({"result":{"iban":"SYNTHETIC"}})
            )
            .is_ok()
        );
    }
}
