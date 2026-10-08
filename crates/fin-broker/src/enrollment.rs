//! Protected human configuration path. Financial RPC cannot invoke this module.
use crate::{
    BrokerConfigV2, BrokerPolicy, CONFIG_PATH, ConnectionConfig, DeploymentConfig, NetworkConfig,
    ProtectedCredentialProvider, load_protected_config, lock_protected_configuration,
    open_protected_store, resource_has_pending_operations, save_protected_config,
};
use fin_authorization::{MacOsAuthorizer, validate_installation};
use fin_binance::{BinanceEndpoint, BinanceRequest, BinanceTransport, CredentialHandle};
use fin_credentials::{CredentialRef, SecretMaterial, TrustedCredentialStore};
use fin_discovery::{
    AddConnectionArgs, BinanceRole, CredentialType, NamedConnectionArgs, Provider, validate_add,
    validate_name,
};
use fin_domain::{AuthorizationChallenge, FinError, ResourceId, Result, binding_hash, now};
use fin_human_terminal::{HumanTerminal, SecretInput};
use fin_presentation::Ui;
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

fn error(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "enrollment",
        message,
        "Inspect connections and unresolved operations. Run enrollment yourself in a fresh human Terminal; never paste credentials in chat.",
    )
}
fn binance_enrollment_error(code: &str, cause: fin_binance::TransportError) -> FinError {
    use fin_binance::TransportError;
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
    let _lock = lock_protected_configuration(true)?;
    let mut config = load_protected_config()?;
    let mut terminal = HumanTerminal::open(config.approver_uid)?;
    let journal = open_protected_store()?;
    if action == "policy_set" {
        terminal.finish()?;
        let args: fin_discovery::PolicySetArgs = parse(request)?;
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

    for connection in &config.connections {
        if connection_resources(connection, &config)
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
    let store = Arc::new(TrustedCredentialStore::open()?);
    recover_inactive_credentials(&store, &config)?;
    match action {
        "add" | "update" => {
            let args: AddConnectionArgs = parse(request)?;
            validate_add(&args)?;
            if args.provider == Provider::Evm {
                for network in NetworkConfig::reviewed_evm_networks() {
                    if !config
                        .networks
                        .iter()
                        .any(|existing| existing.chain_id == network.chain_id)
                    {
                        config.networks.push(network);
                    }
                }
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
            let material = collect_material(&mut terminal, &args)?;
            // Restore the human terminal before storing, validating or approving
            // the candidate. A failed restoration cannot report enrollment success.
            terminal.finish()?;
            let staged = material
                .as_ref()
                .map(|material| store.put(material))
                .transpose()?;
            let result=async {
                let (connection,checks,warnings)=validate_connection(&args,&config,store.clone(),staged.as_ref(),material.as_ref(),ui).await?;
                if let Some(index)=existing {
                    if immutable_identity(&config.connections[index])!=immutable_identity(&connection) {
                        return Err(error("CONNECTION_RESOURCE_CHANGED","Credential replacement must preserve provider, physical identity, network and account relationships. Enroll a separately named connection for a different identity."));
                    }
                    config.connections[index]=connection.clone();
                } else { config.connections.push(connection.clone()); }
                config.validate()?;
                let binding=json!({"action":action,"connection":connection,"checks":checks,"warnings":warnings,
                    "configuration_hash":config_hash(&config)?,"financial_movement":false,"future_financial_authority":false});
                approve_and_audit(config.approver_uid,&binding,&format!("{action} connection {}",args.name),ui).await?;
                { let _progress=ui.progress("Saving connection"); save_protected_config(&config)?; }
                let cleanup_warning=old.as_ref().and_then(credential).filter(|previous|Some(previous.as_str())!=staged.as_ref().map(CredentialRef::as_str))
                    .and_then(|reference|store.remove(&reference).err()).map(|_|"Connection is active; an obsolete protected credential needs cleanup at the next human enrollment.");
                Ok(outcome(action,Some(&connection),checks,warnings,cleanup_warning))
            }.await;
            // Rename+directory-sync can report failure after activation. Read back
            // before deleting a candidate; preserve its key if outcome is uncertain.
            if result.is_err()
                && let Some(reference) = &staged
            {
                match load_protected_config() {
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
            let cleanup_warning=credential(&removed).and_then(|reference|store.remove(&reference).err()).map(|_|"Connection removal is active; an obsolete protected credential needs cleanup at the next human enrollment.");
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

fn collect_setup_policy(mut read: impl FnMut(&str) -> Result<String>) -> Result<BrokerPolicy> {
    let movement = read(
        "Movement limit in USD, including maximum execution costs (0 = no limit; Enter = 100)",
    )?;
    let allocation = read(
        "Rolling 24-hour gross allocation in USD: movement + maximum costs + additional borrowing across all commands (0 = no limit; Enter = 100)",
    )?;
    let args = fin_discovery::PolicySetArgs {
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
    let config = BrokerConfigV2 {
        version: 2,
        approver_uid: args.approver_uid,
        networks: NetworkConfig::reviewed_evm_networks(),
        deployments: vec![DeploymentConfig::ethereum_mainnet()],
        connections: vec![],
        policy,
        extensions: BTreeMap::new(),
    };
    config.validate()?;
    let binding = json!({"action":"initialize","configuration":config,"installs":["compiled fin CLI","protected root broker","native Touch ID helper","launchd socket activation","private configuration, credentials, and durable journal"],
        "financial_movement":false,"future_financial_authority":false});
    approve_and_audit(
        args.approver_uid,
        &binding,
        "Initialize protected Fin configuration",
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
    let material = match args.provider {
        Provider::CoinGecko => SecretMaterial::CoinGeckoApiKey {
            api_key: terminal
                .read_secret("CoinGecko API key", SecretInput::Line)?
                .to_string(),
            plan: args.api_plan.as_deref().unwrap_or("demo").into(),
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
    };
    material.validate_present()?;
    Ok(Some(material))
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
                } if stored_plan == plan => fin_pricing::CoinGeckoPrices::with_api_key(
                    if plan == "demo" {
                        fin_pricing::ApiPlan::Demo
                    } else {
                        fin_pricing::ApiPlan::Pro
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
                fin_pricing::UsdPriceSource::quote(
                    &source,
                    &fin_domain::AssetId {
                        network: "ethereum:1".into(),
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
                .find(|network| network.id == args.network.as_deref().unwrap_or("ethereum:1"))
                .ok_or_else(|| {
                    error(
                        "NETWORK_NOT_CONFIGURED",
                        "The requested reviewed network is not configured.",
                    )
                })?;
            let derived = match material {
                Some(SecretMaterial::EvmPrivateKey { private_key }) => Some(format!(
                    "{:#x}",
                    fin_evm::LocalEnvelopeSigner::from_private_key(private_key)?.address()
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
            let rpc = Arc::new(fin_evm::HttpRpc::new(
                network.rpc_urls.clone(),
                network.chain_id,
            )?);
            let runtime = fin_evm::EvmRuntime::new(rpc, network.chain_id, None);
            {
                let _progress = ui.progress("Checking EVM network");
                runtime.assert_chain().await?;
            }
            let balance = {
                let _progress = ui.progress("Checking wallet balance");
                runtime.balance(fin_evm::address(&address)?, None).await?
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
        Provider::Binance => {
            let reference = staged.ok_or_else(|| {
                error(
                    "CREDENTIAL_REQUIRED",
                    "Binance enrollment needs a protected credential.",
                )
            })?;
            let credentials = Arc::new(ProtectedCredentialProvider::from_store(store));
            let client = fin_exchange::ccxt::CcxtClient::new().map_err(|_| {
                error(
                    "VENUE_UNAVAILABLE",
                    "The trusted exchange runtime could not be initialized.",
                )
            })?;
            let transport = fin_binance::BinanceCcxtTransport::new(Arc::new(client), credentials);
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
            let permissions: fin_binance::ApiRestrictions = serde_json::from_value(permissions)
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
            if role == BinanceRole::Sub {
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
                let children: fin_binance::SubAccountListResponse =
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
async fn approve_and_audit(
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
    fin_authorization::validate_challenge(&challenge, &now())?;
    let review = presentation::review(binding)?;
    let proof = HumanTerminal::open(uid)?.review_configuration(&challenge, &review)?;
    let approval = {
        let _progress = ui.progress("Waiting for Touch ID");
        MacOsAuthorizer::protected(uid)?
            .authorize_reviewed_configuration(&challenge, proof)
            .await
            .map_err(|failure| {
                if failure.code == "AUTHORIZATION_EXPIRED" {
                    fin_human_terminal::configuration_approval_expired(
                        binding["action"].as_str().unwrap_or(""),
                    )
                } else {
                    failure
                }
            })?
    };
    let path = Path::new("/Library/Fin/private/configuration-audit.jsonl");
    if path.try_exists().map_err(|_| {
        error(
            "CONFIG_AUDIT_FAILED",
            "Configuration approval audit could not be inspected.",
        )
    })? {
        fin_authorization::validate_metadata(path, true)?;
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
    fin_authorization::validate_challenge(&challenge, &now()).map_err(|failure| {
        if failure.code == "AUTHORIZATION_EXPIRED" {
            fin_human_terminal::configuration_approval_expired(
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
        ConnectionConfig::Binance { name, .. }
        | ConnectionConfig::Ethereum { name, .. }
        | ConnectionConfig::Evm { name, .. }
        | ConnectionConfig::CoinGecko { name, .. } => name,
    }
}
fn resource(connection: &ConnectionConfig) -> &ResourceId {
    match connection {
        ConnectionConfig::Binance { resource, .. }
        | ConnectionConfig::Ethereum { resource, .. }
        | ConnectionConfig::Evm { resource, .. }
        | ConnectionConfig::CoinGecko { resource, .. } => resource,
    }
}
fn credential(connection: &ConnectionConfig) -> Option<CredentialRef> {
    match connection {
        ConnectionConfig::Binance { credential_ref, .. }
        | ConnectionConfig::CoinGecko { credential_ref, .. } => {
            CredentialRef::new(credential_ref).ok()
        }
        ConnectionConfig::Ethereum { credential_ref, .. }
        | ConnectionConfig::Evm { credential_ref, .. } => credential_ref
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
fn public_connection(connection: &ConnectionConfig) -> Value {
    match connection {
        ConnectionConfig::CoinGecko {
            name,
            resource,
            api_plan,
            ..
        } => json!({"provider":"coingecko","name":name,"resource":resource,"api_plan":api_plan}),
        ConnectionConfig::Binance {
            name,
            resource,
            role,
            parent,
            email,
            wallet_type,
            physical_identity,
            ..
        } => {
            json!({"name":name,"provider":"binance","resource":resource,"role":role,"parent":parent,"email":email,"wallet_type":wallet_type,"physical_identity":physical_identity})
        }
        ConnectionConfig::Evm {
            name,
            resource,
            address,
            signing_enabled,
            ..
        } => {
            json!({"name":name,"provider":"evm","resource":resource,"address":address,"signing_enabled":signing_enabled})
        }
        ConnectionConfig::Ethereum {
            name,
            resource,
            network,
            address,
            signing_enabled,
            ..
        } => {
            json!({"name":name,"provider":"ethereum","resource":resource,"network":network,"address":address,"signing_enabled":signing_enabled})
        }
    }
}
fn immutable_identity(connection: &ConnectionConfig) -> Value {
    match connection {
        ConnectionConfig::Evm { name, address, .. }
        | ConnectionConfig::Ethereum { name, address, .. } => {
            json!({"provider":"evm","name":name,"address":address.to_ascii_lowercase()})
        }
        ConnectionConfig::Binance { .. } => public_connection(connection),
        ConnectionConfig::CoinGecko { name, resource, .. } => {
            json!({"provider":"coingecko","name":name,"resource":resource})
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
    use fin_discovery::enrollment_contract::{EnrollmentOutcome, PublicConnection};
    let connection = connection.map(|connection| {
        serde_json::from_value::<PublicConnection>(public_connection(connection))
            .expect("reviewed safe connection output matches the shared contract")
    });
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
            "Run fin connections add yourself in a fresh human Terminal."
        } else {
            "Run fin connections list --json and fin doctor --json."
        }
        .into(),
    })
    .expect("safe enrollment result serialization")
}
#[cfg(test)]
mod tests {
    use super::*;
    fn pricing_connection(plan: &str, reference: &str) -> ConnectionConfig {
        ConnectionConfig::CoinGecko {
            name: "synthetic-pricing".into(),
            resource: ResourceId("market-data:coingecko".into()),
            credential_ref: reference.into(),
            api_plan: plan.into(),
        }
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
            serde_json::from_value::<fin_discovery::enrollment_contract::EnrollmentOutcome>(output)
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
        use fin_binance::{HttpFailureKind, TransportError};
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
        use fin_binance::TransportError;
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
            serde_json::from_value::<fin_discovery::enrollment_contract::EnrollmentOutcome>(output)
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
        for chain in [1, 42161, 4663] {
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
}
