//! Development broker example. Synthetic is the default; live mode uses only
//! credentials imported into its selected same-user development environment.
//! It is never part of the installed broker and never uses production storage.
#[path = "../../gofin-ethereum/tests/support/aave_mcp_fixture.rs"]
mod aave_mcp_fork_fixture;
#[path = "gofin-dev-support/aave_mcp_read.rs"]
mod aave_mcp_read_fixture;
#[path = "gofin-dev-support/binance_rehearsal.rs"]
mod binance_rehearsal;
#[path = "gofin-dev-support/credentials.rs"]
mod dev_credentials;
#[path = "gofin-dev-support/dotenv.rs"]
mod dotenv;
#[path = "gofin-dev-support/identity.rs"]
mod identity;
#[path = "gofin-dev-support/morpho_fork_rewards.rs"]
mod morpho_fork_rewards;
#[path = "gofin-dev-support/profile.rs"]
mod profile;
#[path = "gofin-dev-support/solana_rehearsal.rs"]
mod solana_rehearsal;
#[path = "gofin-dev-support/solana_validator.rs"]
mod solana_validator;
use async_trait::async_trait;
use gofin_broker::{
    AssetValuator, BrokerConfigV2, BrokerPolicy, DeploymentConfig, NetworkConfig,
    RuntimeDependencies, compose_runtime,
};
use gofin_domain::{
    Adapter, AssetPrice, AuthorizationChallenge, Authorizer, Definition, ExecutionActor,
    FinancialEffect, FinancialIntent, GoFinError, OperationKind, OperationRecord,
    PreparedOperation, Reconciliation, Status, Submission, SubmissionJournal, Valuation,
    VerifiedEffects, now, verified_effect_digest,
};
use gofin_protocol::{BrokerRequest, BrokerResponse, MAX_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES};
use gofin_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

/// Startup-only development fixture check; this path is absent from the installed
/// broker. It accepts only endpoints already vetted by profile::fork_rpc_urls.
async fn owned_fork_identity(endpoint: &str, expected_chain: u64) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "owned fork identity transport is unavailable")?;
    let response: Value = client
        .post(endpoint)
        .json(&serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"anvil_metadata", "params":[]}))
        .send()
        .await
        .map_err(|_| "owned fork identity is unavailable")?
        .error_for_status()
        .map_err(|_| "owned fork identity is unavailable")?
        .json()
        .await
        .map_err(|_| "owned fork identity is invalid")?;
    validate_owned_fork_identity(&response, expected_chain)
}

fn validate_owned_fork_identity(response: &Value, expected_chain: u64) -> Result<Value, String> {
    let metadata = &response["result"];
    let instance = metadata["instanceId"].as_str();
    let anchor = metadata["forkedNetwork"]["forkBlockHash"].as_str();
    if response.get("error").is_some_and(|error| !error.is_null())
        || metadata["chainId"].as_u64() != Some(expected_chain)
        || instance.is_none_or(str::is_empty)
        || anchor.is_none_or(|hash| {
            hash.len() != 66
                || !hash.starts_with("0x")
                || !hash[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || !metadata["forkedNetwork"]["forkBlockNumber"].is_u64()
    {
        return Err(
            "owned fork node must report its expected chain, instance and fork anchor".into(),
        );
    }
    Ok(serde_json::json!({
        "chain_id":expected_chain,
        "instance_id":instance,
        "fork_block_hash":anchor,
        "fork_block_number":metadata["forkedNetwork"]["forkBlockNumber"],
    }))
}

#[cfg(test)]
mod owned_fork_tests {
    use super::*;

    #[test]
    fn startup_identity_requires_expected_chain_and_real_fork_metadata() {
        let mut response = serde_json::json!({"result":{
            "chainId":1, "instanceId":"synthetic-owned-instance",
            "forkedNetwork":{"forkBlockHash":format!("0x{}", "ab".repeat(32)), "forkBlockNumber":123}
        }});
        assert!(validate_owned_fork_identity(&response, 1).is_ok());
        assert!(validate_owned_fork_identity(&response, 42161).is_err());
        response["result"]["forkedNetwork"]["forkBlockHash"] = serde_json::json!("garbage");
        assert!(validate_owned_fork_identity(&response, 1).is_err());
        response["result"]["forkedNetwork"]["forkBlockHash"] =
            serde_json::json!(format!("0x{}", "ab".repeat(32)));
        response["result"]["instanceId"] = serde_json::json!("");
        assert!(validate_owned_fork_identity(&response, 1).is_err());
        assert!(
            validate_owned_fork_identity(&serde_json::json!({"error":{"code":-1}}), 1).is_err()
        );
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    socket: PathBuf,
    database: PathBuf,
    config: PathBuf,
    identity: String,
    environment_id: String,
    /// Human-selected development profile name. This is used only to gate
    /// explicitly named synthetic fixtures; it never selects a production
    /// endpoint or credential.
    environment_name: String,
    build_id: String,
    mode: String,
    fixture: Option<String>,
    import_credentials: Option<PathBuf>,
    discover_identities: bool,
    provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloRequest {
    kind: String,
    identity: String,
    build_id: String,
    mode: String,
}

#[derive(Serialize)]
struct HelloResponse<'a> {
    kind: &'static str,
    identity: &'a str,
    build_id: &'a str,
    mode: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DevRequest {
    identity: String,
    build_id: String,
    mode: String,
    request: BrokerRequest,
    #[serde(default)]
    progress: bool,
    #[serde(default)]
    progress_details: bool,
    #[serde(default)]
    cancel_on_disconnect: bool,
}

#[derive(Serialize)]
struct DevResponse<'a> {
    identity: &'a str,
    build_id: &'a str,
    mode: &'a str,
    response: BrokerResponse,
}

type ConfigMarker = profile::EnvironmentConfig;

struct EnvironmentLifetimeLock {
    _file: fs::File,
}

impl EnvironmentLifetimeLock {
    fn acquire(environment_dir: &Path) -> Result<Self, String> {
        let path = environment_dir.join("broker.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| format!("could not open the environment lifetime lock: {error}"))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Err("environment lifetime lock is not a regular file".into());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        // SAFETY: flock only inspects the valid descriptor while this File
        // remains owned by the returned guard.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            return Err("another broker still owns this environment's persistent state".into());
        }
        Ok(Self { _file: file })
    }
}

struct DevelopmentAutoAuthorizer;

#[async_trait]
impl Authorizer for DevelopmentAutoAuthorizer {
    async fn authorize(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
        validate_development_challenge(challenge)?;
        Ok(gofin_domain::ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "development-auto-approval-no-os-authentication".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct DevelopmentAutoConfigurationAuthorizer;

#[async_trait]
impl gofin_engine::ConfigurationAuthorizer for DevelopmentAutoConfigurationAuthorizer {
    async fn authorize_configuration(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
        validate_development_challenge(challenge)?;
        Ok(gofin_domain::ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "development-auto-approval-no-os-authentication".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn validate_development_challenge(challenge: &AuthorizationChallenge) -> gofin_domain::Result<()> {
    let reject = || {
        GoFinError::new(
            "INVALID_DEVELOPMENT_CHALLENGE",
            "authorization",
            "The development approval challenge is malformed or expired.",
            "Refresh the operation preview before retrying.",
        )
    };
    if challenge.purpose.trim().is_empty()
        || challenge.operation_id.trim().is_empty()
        || challenge.summary.trim().is_empty()
        || challenge.nonce.len() != 64
        || !challenge.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || gofin_domain::binding_hash(&challenge.binding)
            .ok()
            .as_deref()
            != Some(&challenge.binding_hash)
    {
        return Err(reject());
    }
    let expires_at =
        chrono::DateTime::parse_from_rfc3339(&challenge.expires_at).map_err(|_| reject())?;
    if expires_at <= chrono::Utc::now() {
        return Err(reject());
    }
    Ok(())
}

struct SyntheticAdapter {
    receipt_dir: PathBuf,
    movement_usd_micros: String,
}

#[async_trait]
impl Adapter for SyntheticAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![
            Definition {
                name: "synthetic.echo".into(),
                description: "Return a deterministic development fixture without network access.".into(),
                kind: OperationKind::Read,
                execution_actor: ExecutionActor::AgentReadable,
                input_schema: serde_json::json!({"type":"object","additionalProperties":true}),
                output_schema: serde_json::json!({"type":"object"}),
                error_schema: serde_json::json!({"$ref":"GoFinError"}),
                examples: vec![serde_json::json!({"input":{"value":"hello"}})],
                coverage: "Synthetic local fixture only.".into(),
                completion: "Returns the supplied safe value.".into(),
                semantics: serde_json::json!({"network":false,"synthetic":true}),
            },
            Definition {
                name: "synthetic.uncertain-write".into(),
                description: "Persist one synthetic submission then return an uncertain outcome for recovery testing.".into(),
                kind: OperationKind::Write,
                execution_actor: ExecutionActor::AgentInvocable,
                input_schema: serde_json::json!({"type":"object","additionalProperties":false,"properties":{"value":{"type":"string"}}}),
                output_schema: serde_json::json!({"type":"object"}),
                error_schema: serde_json::json!({"$ref":"GoFinError"}),
                examples: vec![serde_json::json!({"input":{"value":"synthetic"}})],
                coverage: "Synthetic journal and recovery exercise; moves no funds.".into(),
                completion: "The submission remains unresolved until synthetic reconciliation.".into(),
                semantics: serde_json::json!({"network":false,"synthetic":true,"financialMovement":false}),
            },
        ]
    }

    async fn read(&self, operation: &str, input: &Value) -> gofin_domain::Result<Value> {
        if operation != "synthetic.echo" {
            return Err(GoFinError::validation("Unknown synthetic read."));
        }
        Ok(
            serde_json::json!({"synthetic":true,"value":input.get("value").cloned().unwrap_or(Value::Null)}),
        )
    }

    async fn prepare(
        &self,
        operation: &str,
        input: &Value,
    ) -> gofin_domain::Result<PreparedOperation> {
        if operation != "synthetic.uncertain-write" {
            return Err(GoFinError::validation("Unknown synthetic write."));
        }
        let at = chrono::Utc::now();
        Ok(PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding: serde_json::json!({"synthetic":true,"value":input.get("value").cloned().unwrap_or(Value::Null)}),
            summary:
                "Record one synthetic development submission; no funds or network are involved."
                    .into(),
            checks: vec![],
            warnings: vec![
                "Synthetic approval and submission are local development fixtures.".into(),
            ],
            observed_at: at.to_rfc3339(),
            expires_at: (at + chrono::Duration::seconds(90)).to_rfc3339(),
            valuation: Some(Valuation {
                movement_usd_micros: gofin_domain::Quantity::new(&self.movement_usd_micros)?,
                maximum_fee_usd_micros: gofin_domain::Quantity::zero(),
                additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
                observed_at: at.to_rfc3339(),
                source: "synthetic zero-movement fixture".into(),
                prices: vec![],
            }),
            intent: FinancialIntent {
                debits: vec![],
                maximum_costs: vec![],
                expected: vec![],
                locks: vec![],
            },
            operation_version: 1,
            action_deadline: None,
        })
    }

    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> gofin_domain::Result<PreparedOperation> {
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        _prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> gofin_domain::Result<Submission> {
        journal.assert_authorized()?;
        let submission = Submission {
            kind: "synthetic_local".into(),
            reference: uuid::Uuid::new_v4().to_string(),
            data: serde_json::json!({"network":false}),
        };
        journal.persist_submission(&submission)?;
        fs::create_dir_all(&self.receipt_dir).map_err(|_| receipt_error())?;
        fs::set_permissions(&self.receipt_dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| receipt_error())?;
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: Vec::<FinancialEffect>::new(),
            evidence: serde_json::json!({"synthetic":true,"network":false,"result":"accepted"}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        let mut receipt = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(
                self.receipt_dir
                    .join(format!("{}.json", submission.reference)),
            )
            .map_err(|_| receipt_error())?;
        let bytes = serde_json::to_vec(&verified).map_err(|_| receipt_error())?;
        receipt
            .write_all(&bytes)
            .and_then(|_| receipt.sync_all())
            .map_err(|_| receipt_error())?;
        Err(GoFinError::new(
            "SYNTHETIC_UNCERTAIN",
            "venue",
            "Synthetic dispatch is intentionally uncertain after its submission is persisted.",
            "Inspect the same request ID in this environment; do not create a replacement identity.",
        ))
    }

    async fn reconcile(&self, record: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
        let Some(submission) = &record.submission else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: serde_json::json!({"synthetic":true,"request_id":record.request_id,"receipt":"not-found"}),
                verified: None,
            });
        };
        let receipt = self
            .receipt_dir
            .join(format!("{}.json", submission.reference));
        let verified = fs::read(receipt)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<VerifiedEffects>(&bytes).ok());
        if let Some(verified) = verified.filter(|verified| {
            verified.submission == *submission
                && verified_effect_digest(verified).ok().as_deref()
                    == Some(verified.proof_digest.as_str())
        }) {
            return Ok(Reconciliation {
                status: Status::Completed,
                evidence: verified.evidence.clone(),
                verified: Some(verified),
            });
        }
        Ok(Reconciliation {
            status: Status::Unknown,
            evidence: serde_json::json!({"synthetic":true,"request_id":record.request_id,"receipt":"not-found-or-invalid"}),
            verified: None,
        })
    }
}

fn receipt_error() -> GoFinError {
    GoFinError::new(
        "SYNTHETIC_RECEIPT_UNCERTAIN",
        "reconciliation",
        "A synthetic submission was persisted but its local receipt could not be committed.",
        "Inspect the original operation identity before retrying.",
    )
}

struct SyntheticValuator;

#[async_trait]
impl AssetValuator for SyntheticValuator {
    async fn usd_micros(
        &self,
        _amount: &gofin_domain::Amount,
    ) -> gofin_domain::Result<(String, String, String)> {
        Err(GoFinError::new(
            "SYNTHETIC_PRICE_UNAVAILABLE",
            "valuation",
            "Synthetic development has no configured asset prices.",
            "Use a focused fixture when testing financial valuation.",
        ))
    }

    async fn asset_price(
        &self,
        _asset: &gofin_domain::AssetId,
    ) -> gofin_domain::Result<AssetPrice> {
        Err(GoFinError::new(
            "SYNTHETIC_PRICE_UNAVAILABLE",
            "valuation",
            "Synthetic development has no configured asset prices.",
            "Use a focused fixture when testing financial valuation.",
        ))
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args_os().skip(1);
    let mut values = std::collections::BTreeMap::new();
    let mut mode = None;
    let mut import_credentials = None;
    let mut discover_identities = false;
    let mut provider = None;
    let mut fixture = None;
    while let Some(flag) = args.next() {
        let flag = flag.to_string_lossy().into_owned();
        if flag == "--discover-identities" {
            if discover_identities {
                return Err("duplicate --discover-identities".into());
            }
            discover_identities = true;
            continue;
        }
        let Some(value) = args.next() else {
            return Err(format!("missing value for {flag}"));
        };
        match flag.as_str() {
            "--mode" => {
                if mode.replace(value.to_string_lossy().into_owned()).is_some() {
                    return Err("duplicate --mode".into());
                }
            }
            "--import-credentials" => {
                if import_credentials.replace(PathBuf::from(value)).is_some() {
                    return Err("duplicate --import-credentials".into());
                }
            }
            "--provider" => {
                let selected = value.to_string_lossy().into_owned();
                if provider.replace(selected).is_some() {
                    return Err("duplicate --provider".into());
                }
            }
            "--fixture" => {
                let selected = value.to_string_lossy().into_owned();
                if fixture.replace(selected).is_some() {
                    return Err("duplicate --fixture".into());
                }
            }
            "--socket" | "--database" | "--config" | "--identity" | "--environment-id"
            | "--environment-name" | "--build-id" => {
                if values.insert(flag.clone(), value).is_some() {
                    return Err(format!("duplicate {flag}"));
                }
            }
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    let mode = mode.unwrap_or_else(|| "synthetic".into());
    if !matches!(mode.as_str(), "synthetic" | "live") {
        return Err("--mode must be synthetic or live".into());
    }
    if fixture.as_deref().is_some_and(|selected| {
        !matches!(
            selected,
            "solana"
                | "solana-validator"
                | "binance-futures"
                | "binance-futures-subaccount"
                | "aave-mcp-read"
                | "aave-mcp-read-unavailable"
                | "aave-mcp-read-tool-error"
                | "aave-mcp-read-stale-catalog"
        )
    }) || (fixture.is_some() && mode != "synthetic")
    {
        return Err("Synthetic fixtures are available only in synthetic mode".into());
    }
    let get = |key: &str| {
        values
            .get(key)
            .map(PathBuf::from)
            .ok_or_else(|| format!("missing {key}"))
    };
    let identity = values
        .get("--identity")
        .map(|v| v.to_string_lossy().into_owned())
        .ok_or("missing --identity")?;
    let environment_id = values
        .get("--environment-id")
        .map(|v| v.to_string_lossy().into_owned())
        .ok_or("missing --environment-id")?;
    let environment_name = values
        .get("--environment-name")
        .map(|v| v.to_string_lossy().into_owned())
        .ok_or("missing --environment-name")?;
    let build_id = values
        .get("--build-id")
        .map(|v| v.to_string_lossy().into_owned())
        .ok_or("missing --build-id")?;
    if identity.is_empty() || std::env::var("GOFIN_DEV_IDENTITY").ok().as_deref() != Some(&identity)
    {
        return Err("--identity must match GOFIN_DEV_IDENTITY".into());
    }
    if environment_id.is_empty()
        || std::env::var("GOFIN_DEV_ENVIRONMENT_ID").ok().as_deref() != Some(&environment_id)
    {
        return Err("--environment-id must match GOFIN_DEV_ENVIRONMENT_ID".into());
    }
    let compiled_build =
        option_env!("GOFIN_DEV_BUILD_ID").ok_or("broker was not built with GOFIN_DEV_BUILD_ID")?;
    if build_id.is_empty()
        || build_id != compiled_build
        || std::env::var("GOFIN_DEV_BUILD_ID").ok().as_deref() != Some(&build_id)
    {
        return Err("--build-id does not match this compiled broker".into());
    }
    validate_import_options(
        &mode,
        import_credentials.as_ref(),
        discover_identities,
        provider.as_deref(),
    )?;
    Ok(Args {
        socket: get("--socket")?,
        database: get("--database")?,
        config: get("--config")?,
        identity,
        environment_id,
        environment_name,
        build_id,
        mode,
        fixture,
        import_credentials,
        discover_identities,
        provider,
    })
}

fn validate_import_options(
    mode: &str,
    import_credentials: Option<&PathBuf>,
    discover_identities: bool,
    provider: Option<&str>,
) -> Result<(), String> {
    if import_credentials.is_some() && mode != "live" {
        return Err("credential import requires --mode live".into());
    }
    if discover_identities && import_credentials.is_none() {
        return Err("--discover-identities requires --import-credentials".into());
    }
    if provider.is_some() && import_credentials.is_none() {
        return Err("--provider requires --import-credentials".into());
    }
    if provider.is_some_and(|provider| {
        !matches!(
            provider,
            "binance"
                | "evm"
                | "solana"
                | "coinbase"
                | "coinbaseexchange"
                | "coinbaseinternational"
        )
    }) {
        return Err(
            "--provider must be binance, evm, solana or an explicit Coinbase product".into(),
        );
    }
    if discover_identities && provider.is_some_and(|provider| provider != "binance") {
        return Err("--discover-identities is available only for Binance imports".into());
    }
    Ok(())
}

fn initialize_config(path: &Path, environment_id: &str) -> Result<ConfigMarker, String> {
    let config = ConfigMarker::read(path, environment_id)?;
    if !path.exists() {
        config.write_atomic(path)?;
    }
    Ok(config)
}

fn prepare_storage(args: &Args) -> Result<(), String> {
    if args.database == Path::new(gofin_broker::OPERATIONS_PATH)
        || args.config == Path::new(gofin_broker::CONFIG_PATH)
        || args.socket == Path::new(gofin_broker::SOCKET_PATH)
        || args.database.file_name().and_then(|name| name.to_str()) != Some("operations.sqlite")
        || args.config.file_name().and_then(|name| name.to_str()) != Some("config.json")
    {
        return Err(
            "development paths must name environment-owned config.json and operations.sqlite"
                .into(),
        );
    }
    let config_parent = args.config.parent().ok_or("config path has no parent")?;
    let database_parent = args
        .database
        .parent()
        .ok_or("database path has no parent")?;
    fs::create_dir_all(config_parent).map_err(|error| error.to_string())?;
    fs::create_dir_all(database_parent).map_err(|error| error.to_string())?;
    if fs::canonicalize(config_parent).map_err(|error| error.to_string())?
        != fs::canonicalize(database_parent).map_err(|error| error.to_string())?
    {
        return Err("config and database must live in the same selected environment".into());
    }
    for path in [&args.config, &args.database] {
        if let Ok(metadata) = fs::symlink_metadata(path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(format!(
                "{} must be a regular environment-owned file",
                path.display()
            ));
        }
    }
    if !args.database.exists() {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&args.database)
            .map_err(|e| e.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    } else {
        fs::set_permissions(&args.database, fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn reject_import_with_unresolved_records(database: &Path) -> Result<(), String> {
    if !database.exists() {
        return Ok(());
    }
    let store = Store::open(database)
        .map_err(|_| "development journal could not be inspected".to_owned())?;
    for record in store
        .list_records()
        .map_err(|_| "development journal could not be inspected".to_owned())?
    {
        let status = record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(
            status,
            "rejected" | "expired" | "cancelled" | "completed" | "failed"
        ) {
            return Err(
                "credential replacement is blocked while an operation is unresolved".into(),
            );
        }
    }
    Ok(())
}

fn config() -> BrokerConfigV2 {
    BrokerConfigV2 {
        version: 2,
        networks: NetworkConfig::reviewed_evm_networks(),
        deployments: vec![DeploymentConfig::ethereum_mainnet()],
        connections: Vec::new(),
        policy: BrokerPolicy::beta(),
        approver_uid: unsafe { libc::geteuid() },
        extensions: Default::default(),
    }
}

fn validate_bridge_composition(
    adapters: &[Arc<dyn Adapter>],
    bridge: &Option<Arc<gofin_broker::BridgeAdapter>>,
) -> Result<(), String> {
    let adapter_has_bridge_operations = adapters.iter().any(|adapter| {
        adapter
            .definitions()
            .iter()
            .any(|definition| definition.name.starts_with("bridge."))
    });
    if adapter_has_bridge_operations != bridge.is_some() {
        return Err(
            "bridge operations and shared bridge state must be wired together at startup".into(),
        );
    }
    Ok(())
}

async fn run(args: Args) -> Result<(), String> {
    let environment_dir = args
        .database
        .parent()
        .ok_or("database path has no parent")?;
    // Acquire before inspecting config or opening SQLite. The lifecycle helper
    // probes this same lock before reset, even if process metadata is missing.
    let _lifetime_lock = EnvironmentLifetimeLock::acquire(environment_dir)?;
    prepare_storage(&args)?;
    if let Some(env_file) = &args.import_credentials {
        let existing = profile::EnvironmentConfig::read(&args.config, &args.environment_id)?;
        if !existing.credentials_is_empty() {
            reject_import_with_unresolved_records(&args.database)?;
        }
        profile::import(
            &args.config,
            &args.environment_id,
            env_file,
            &environment_dir.join("credentials"),
            args.discover_identities,
            args.provider.as_deref(),
        )
        .await?;
        println!("Development credentials imported.");
        return Ok(());
    }
    let environment_config = initialize_config(&args.config, &args.environment_id)?;
    if environment_config.mode != args.mode {
        return Err("selected development mode does not match the saved environment".into());
    }
    if args.mode == "live" && environment_config.credentials_is_empty() {
        return Err("live development mode has no imported credentials".into());
    }
    let yolo_fixture_paths = if environment_config.yolo_fixture.is_some() {
        let directory = environment_dir.join("yolo-fixture");
        fs::create_dir_all(&directory)
            .map_err(|_| "development Yolo fixture directory is unavailable")?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .map_err(|_| "development Yolo fixture directory cannot be protected")?;
        let state = directory.join("state.json");
        let gate = directory.join("gate.lock");
        gofin_broker::yolo_runtime::set_enabled(
            &state,
            &gate,
            environment_config
                .yolo_fixture
                .as_ref()
                .is_some_and(|fixture| fixture.enabled),
        )
        .map_err(|_| "development Yolo fixture state is unavailable")?;
        Some((state, gate))
    } else {
        None
    };
    if args.fixture.is_some()
        && (args.mode != "synthetic" || !environment_config.credentials_is_empty())
    {
        return Err(
            "synthetic fixtures require an isolated synthetic profile without imported credentials"
                .into(),
        );
    }
    if let Some(parent) = args.socket.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let mut owned_fork_metadata = serde_json::Map::new();
    let mut artifact_profiles = vec![];
    let (runtime_config, adapters, valuator, bridge) = if args.mode == "live" {
        let runtime_config = profile::broker_config(&environment_config)?;
        let credentials = Arc::new(
            dev_credentials::DevelopmentCredentialProvider::new(
                environment_dir.join("credentials"),
            )
            .map_err(|_| "development credential store is unavailable")?,
        );
        artifact_profiles =
            gofin_broker::kraken::profiles(&runtime_config, credentials.clone(), None, None)
                .map_err(|_| "development export profiles could not be initialized")?;
        let source: Arc<dyn gofin_pricing::UsdPriceSource> = Arc::new(
            gofin_pricing::ExchangeSpotPrices::new(
                gofin_pricing::CoinGeckoPrices::new()
                    .map_err(|_| "public pricing is unavailable")?,
            )
            .map_err(|_| "public pricing is unavailable")?,
        );
        let applications = if let Some(endpoints) = profile::fork_rpc_urls(&environment_config)? {
            let mut transports = std::collections::BTreeMap::new();
            let mut morpho_transport = None;
            let mut morpho_prices = None;
            for (chain_id, endpoint) in endpoints {
                let rpc: Arc<dyn gofin_evm::EvmRpc> = Arc::new(
                    gofin_evm::HttpRpc::isolated_localhost(endpoint.clone(), chain_id)
                        .map_err(|_| "owned fork transport could not be initialized")?,
                );
                // Verify a fork node, not merely any process accepting loopback HTTP.
                // The public CLI harness separately pins ownership and anchor before
                // every financial dispatch; no production RPC fallback is configured.
                let metadata = owned_fork_identity(&endpoint, chain_id).await?;
                let chain = rpc
                    .request("eth_chainId", serde_json::json!([]))
                    .await
                    .map_err(|_| "owned fork chain identity could not be verified")?;
                if chain
                    .as_str()
                    .and_then(|hex| hex.strip_prefix("0x"))
                    .and_then(|hex| u64::from_str_radix(hex, 16).ok())
                    != Some(chain_id)
                {
                    return Err("owned fork RPC chain does not match the selected chain".into());
                }
                if environment_config.morpho_fork_rewards {
                    let transport = Arc::new(morpho_fork_rewards::MorphoForkRewards::new(
                        chain_id,
                        environment_config
                            .evm_addresses
                            .first()
                            .ok_or("Morpho fork fixture requires a disposable wallet")?
                            .clone(),
                        endpoint.clone(),
                        metadata["instance_id"]
                            .as_str()
                            .ok_or("Owned fork instance identity is missing")?
                            .to_owned(),
                        metadata["fork_block_hash"]
                            .as_str()
                            .ok_or("Owned fork block identity is missing")?
                            .to_owned(),
                    ));
                    morpho_transport = Some(transport.clone()
                        as Arc<dyn gofin_ethereum::morpho::api::MorphoApiTransport>);
                    morpho_prices = Some(transport as Arc<dyn gofin_pricing::UsdPriceSource>);
                }
                owned_fork_metadata.insert(chain_id.to_string(), metadata);
                transports.insert(chain_id, rpc);
            }
            if let Some(morpho_transport) = morpho_transport {
                let aave_transport = runtime_config
                    .deployments
                    .iter()
                    .any(|deployment| deployment.aave.is_some())
                    .then(|| {
                        aave_mcp_fork_fixture::ForkAaveMcp::new()
                            as Arc<dyn gofin_ethereum::aave_mcp::AaveMcpTransport>
                    });
                gofin_broker::build_applications_with_owned_morpho_fork_fixture(
                    &runtime_config,
                    credentials,
                    morpho_prices.ok_or("Owned Morpho fork valuation fixture is missing")?,
                    transports,
                    morpho_transport,
                    aave_transport,
                )
            } else if runtime_config
                .deployments
                .iter()
                .any(|deployment| deployment.aave.is_some())
            {
                gofin_broker::build_applications_with_owned_aave_fork_mcp_fixture(
                    &runtime_config,
                    credentials,
                    source,
                    transports,
                    aave_mcp_fork_fixture::ForkAaveMcp::new(),
                )
            } else {
                gofin_broker::build_applications_with_transports(
                    &runtime_config,
                    credentials,
                    source,
                    transports,
                )
            }
        } else {
            gofin_broker::build_applications_with_price_source(&runtime_config, credentials, source)
        }
        .map_err(|_| "live development adapters could not be initialized")?;
        (
            runtime_config,
            applications.adapters,
            applications.valuator,
            applications.bridge,
        )
    } else if matches!(
        args.fixture.as_deref(),
        Some("binance-futures" | "binance-futures-subaccount")
    ) {
        let (runtime_config, applications) = binance_rehearsal::applications(
            environment_dir,
            args.fixture.as_deref() == Some("binance-futures-subaccount"),
        )
        .map_err(|_| "synthetic Binance adapters could not be initialized")?;
        (
            runtime_config,
            applications.adapters,
            applications.valuator,
            applications.bridge,
        )
    } else if args.fixture.as_deref() == Some("solana") {
        let (runtime_config, applications) = solana_rehearsal::applications()
            .map_err(|_| "synthetic Solana adapters could not be initialized")?;
        (
            runtime_config,
            applications.adapters,
            applications.valuator,
            applications.bridge,
        )
    } else if args.fixture.as_deref() == Some("solana-validator") {
        let (runtime_config, applications) = solana_validator::applications(environment_dir)
            .map_err(|_| "synthetic local-validator adapters could not be initialized")?;
        (
            runtime_config,
            applications.adapters,
            applications.valuator,
            applications.bridge,
        )
    } else {
        (
            config(),
            vec![Arc::new(SyntheticAdapter {
                receipt_dir: environment_dir.join("synthetic-receipts"),
                movement_usd_micros: "0".into(),
            }) as Arc<dyn Adapter>],
            Arc::new(SyntheticValuator) as Arc<dyn AssetValuator>,
            None,
        )
    };
    validate_bridge_composition(&adapters, &bridge)?;
    let requires_identity_check = runtime_config
        .connections
        .iter()
        .any(|connection| matches!(connection, gofin_broker::ConnectionConfig::Binance { .. }));
    let sub_email_mapping = if environment_config.credentials.binance_sub.is_none() {
        "not_configured"
    } else if environment_config.credentials.binance.is_none() {
        "standalone_own_credential_no_parent_mapping"
    } else if environment_config.binance_sub_email_matched_by_venue_id {
        "venue_subaccount_id_matched"
    } else {
        "explicit_email_mapping"
    };
    let operation_definitions = adapters
        .iter()
        .flat_map(|adapter| adapter.definitions())
        .map(|mut definition| {
            gofin_broker::annotate_plan_discovery(&mut definition);
            definition
        })
        .collect::<Vec<_>>();
    let development_metadata = serde_json::json!({
        "identity":args.identity,
        "build_id":args.build_id,
        "environment_name":args.environment_name,
        "mode":args.mode,
        "synthetic":args.mode == "synthetic",
        "approval_mechanism":if yolo_fixture_paths.is_some() { "development-yolo-fixture" } else { "development-auto-approval-no-os-authentication" },
        "authorization_protection":if yolo_fixture_paths.is_some() { "environment-local fixture state and admission gate only; no production sudo or terminal protection" } else { "development auto-approval; no production authorization protection" },
        "touch_id_required":false,
        "real_funds_possible":args.mode == "live",
        "evm_transport":if owned_fork_metadata.is_empty() { "reviewed_public_rpc" } else { "owned_local_fork_only" },
        "aave_mcp_transport":if !owned_fork_metadata.is_empty() && runtime_config.deployments.iter().any(|deployment| deployment.aave.is_some()) { "deterministic_owned_fork_fixture" } else { "fixed_official_http" },
        "owned_forks":owned_fork_metadata,
        "account_identity_verification":if requires_identity_check { "required_before_account_operations" } else { "not_configured" },
        "sub_email_mapping":sub_email_mapping,
        "operations":operation_definitions,
    });
    let authorizer: Arc<dyn Authorizer> = if let Some((state_path, gate_path)) = &yolo_fixture_paths
    {
        Arc::new(
            gofin_broker::yolo_runtime::YoloAuthorizer::development_fixture(
                Arc::new(gofin_authorization::UnavailableAuthorizer),
                state_path.clone(),
                gate_path.clone(),
            ),
        )
    } else {
        Arc::new(DevelopmentAutoAuthorizer)
    };
    let runtime = compose_runtime(
        runtime_config,
        RuntimeDependencies {
            store: Store::open(&args.database).map_err(|e| e.to_string())?,
            adapters,
            authorizer,
            valuator,
            bridge,
        },
    )
    .map_err(|e| e.to_string())?;
    if let Some((state_path, gate_path)) = yolo_fixture_paths {
        runtime
            .attach_development_yolo_fixture(state_path, gate_path)
            .map_err(|_| "development Yolo fixture paths could not be attached")?;
    }
    runtime
        .attach_development_configuration_authorizer(Arc::new(
            DevelopmentAutoConfigurationAuthorizer,
        ))
        .map_err(|e| e.to_string())?;
    runtime
        .attach_kraken_artifacts(artifact_profiles)
        .map_err(|e| e.to_string())?;
    if args.mode == "synthetic" {
        let transport: Arc<dyn gofin_ethereum::aave_mcp::AaveMcpTransport> =
            match args.fixture.as_deref() {
                Some("aave-mcp-read") => Arc::new(aave_mcp_read_fixture::AaveMcpReadFixture::new()),
                Some("aave-mcp-read-tool-error") => {
                    Arc::new(aave_mcp_read_fixture::AaveMcpReadFixture::tool_error())
                }
                Some("aave-mcp-read-stale-catalog") => {
                    Arc::new(aave_mcp_read_fixture::AaveMcpReadFixture::stale_catalog())
                }
                Some("aave-mcp-read-unavailable")
                | None
                | Some("binance-futures")
                | Some("binance-futures-subaccount")
                | Some("solana")
                | Some("solana-validator") => {
                    Arc::new(aave_mcp_read_fixture::AaveMcpReadFixture::unavailable())
                }
                _ => return Err("unsupported synthetic Aave MCP fixture selection".into()),
            };
        runtime
            .with_aave_mcp_client(gofin_ethereum::aave_mcp::AaveMcpClient::with_transport(
                transport,
            ))
            .map_err(|_| "synthetic Aave MCP read fixture could not be attached")?;
    }
    if args.mode == "live" {
        runtime.require_account_identity_verification();
    }
    // Bind fails if the selected path is occupied. Never unlink an unknown
    // socket or attach to a process already listening there.
    let listener = UnixListener::bind(&args.socket)
        .map_err(|e| format!("could not bind selected socket: {e}"))?;
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("could not protect socket: {e}"))?;
    let socket_identity = fs::symlink_metadata(&args.socket).map_err(|e| e.to_string())?;
    let _owned_socket = OwnedSocket {
        path: args.socket.clone(),
        device: socket_identity.dev(),
        inode: socket_identity.ino(),
    };
    // Repair only after this process has claimed its isolated socket. As with
    // the protected listener, preserve every dispatched submission while
    // releasing interrupted candidates proven never to have been submitted.
    runtime
        .engine
        .recover_interrupted()
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|error| error.to_string())?;
    let mut peers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| error.to_string())?;
                break;
            }
            _ = terminate.recv() => break,
            _ = hangup.recv() => break,
            _ = peers.join_next(), if !peers.is_empty() => {}
            accepted = listener.accept(), if peers.len() < gofin_broker::MAX_BROKER_CONNECTIONS => {
                let (stream, _) = accepted.map_err(|e| e.to_string())?;
                peers.spawn(serve_peer(stream, runtime.clone(), args.identity.clone(), args.build_id.clone(), args.mode.clone(), development_metadata.clone()));
            }
        }
    }
    // Stop accepting peers, then finish journaled attempts under the same
    // runtime. Do not drop a financial dispatch on a generic IPC deadline.
    while peers.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_bridge_operations_require_the_shared_repository_composition() {
        let bridge = Arc::new(
            gofin_broker::BridgeAdapter::new(
                std::collections::BTreeMap::new(),
                Arc::new(gofin_broker::SharedUsdValuator::new().unwrap()),
            )
            .unwrap(),
        );
        let adapters: Vec<Arc<dyn Adapter>> = vec![bridge.clone()];
        assert!(validate_bridge_composition(&adapters, &None).is_err());
        assert!(validate_bridge_composition(&adapters, &Some(bridge)).is_ok());
        assert!(validate_bridge_composition(&[], &None).is_ok());
    }

    #[test]
    fn discovery_requires_an_import_and_import_requires_live_mode() {
        assert!(validate_import_options("live", None, true, None).is_err());
        assert!(
            validate_import_options("synthetic", Some(&PathBuf::from("keys.env")), false, None)
                .is_err()
        );
        assert!(
            validate_import_options("live", Some(&PathBuf::from("keys.env")), true, None).is_ok()
        );
        assert!(validate_import_options("live", None, false, None).is_ok());
        assert!(
            validate_import_options(
                "live",
                Some(&PathBuf::from("keys.env")),
                false,
                Some("solana")
            )
            .is_ok()
        );
        assert!(
            validate_import_options("live", Some(&PathBuf::from("keys.env")), false, Some("bad"))
                .is_err()
        );
    }

    #[test]
    fn environment_lifetime_lock_excludes_second_broker_and_releases_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let first = EnvironmentLifetimeLock::acquire(directory.path()).unwrap();
        assert!(EnvironmentLifetimeLock::acquire(directory.path()).is_err());
        drop(first);
        assert!(EnvironmentLifetimeLock::acquire(directory.path()).is_ok());
    }

    fn challenge(expires_at: String) -> AuthorizationChallenge {
        let binding = serde_json::json!({"operation":"fixture","amount":"1"});
        AuthorizationChallenge {
            review: None,
            purpose: "financial_write".into(),
            operation_id: "operation-1".into(),
            binding_hash: gofin_domain::binding_hash(&binding).unwrap(),
            binding,
            summary: "Development approval fixture.".into(),
            nonce: "a".repeat(64),
            expires_at,
        }
    }

    #[tokio::test]
    async fn development_authorizer_checks_expiry_and_exact_binding() {
        let valid = challenge((chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339());
        let audit = DevelopmentAutoAuthorizer.authorize(&valid).await.unwrap();
        assert_eq!(
            audit.mechanism,
            "development-auto-approval-no-os-authentication"
        );
        assert_eq!(audit.binding_hash, valid.binding_hash);
        assert_eq!(audit.challenge_nonce, valid.nonce);

        let expired = challenge((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        assert!(DevelopmentAutoAuthorizer.authorize(&expired).await.is_err());

        let mut mismatched = valid;
        mismatched.binding["amount"] = serde_json::json!("2");
        assert!(
            DevelopmentAutoAuthorizer
                .authorize(&mismatched)
                .await
                .is_err()
        );

        let mut malformed =
            challenge((chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339());
        malformed.nonce = "not-a-nonce".into();
        assert!(
            DevelopmentAutoAuthorizer
                .authorize(&malformed)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn development_auto_approval_does_not_bypass_financial_policy() {
        let directory = tempfile::tempdir().unwrap();
        let receipt_dir = directory.path().join("receipts");
        let adapter = Arc::new(SyntheticAdapter {
            receipt_dir: receipt_dir.clone(),
            movement_usd_micros: "2".into(),
        });
        let mut broker_config = config();
        broker_config.policy.max_movement_usd_micros = "1".into();
        let runtime = compose_runtime(
            broker_config,
            RuntimeDependencies {
                store: Store::open(directory.path().join("operations.sqlite")).unwrap(),
                adapters: vec![adapter],
                authorizer: Arc::new(DevelopmentAutoAuthorizer),
                valuator: Arc::new(SyntheticValuator),
                bridge: None,
            },
        )
        .unwrap();
        let response = runtime
            .handle(BrokerRequest::Execute {
                request: gofin_domain::OperationRequest {
                    request_id: "policy-fixture-request".into(),
                    operation: "synthetic.uncertain-write".into(),
                    input: serde_json::json!({"value":"fixture"}),
                    run_id: None,
                },
            })
            .await;
        assert!(response.ok);
        assert_eq!(response.result.as_ref().unwrap()["status"], "failed");
        assert_eq!(
            response.result.as_ref().unwrap()["error"]["code"],
            "MOVEMENT_LIMIT"
        );
        assert!(
            !receipt_dir.exists(),
            "over-limit action must not be submitted"
        );
    }
}

struct OwnedSocket {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.device
            && meta.ino() == self.inode
            && meta.file_type().is_socket()
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn serve_peer(
    mut stream: UnixStream,
    runtime: Arc<gofin_broker::BrokerRuntime>,
    identity: String,
    build_id: String,
    mode: String,
    development_metadata: Value,
) {
    if peer_uid(&stream) != Some(unsafe { libc::geteuid() }) {
        return;
    }
    let mut reader = BufReader::new(&mut stream);
    let Some(line) = read_line(&mut reader, 4096).await else {
        return;
    };
    let Ok(hello) = serde_json::from_slice::<HelloRequest>(&line) else {
        return;
    };
    if hello.kind != "hello" || hello.mode != mode {
        return;
    }
    let hello_response = serde_json::to_vec(&HelloResponse {
        kind: "hello",
        identity: &identity,
        build_id: &build_id,
        mode: &mode,
    })
    .unwrap_or_default();
    if send_line(reader.get_mut(), &hello_response).await.is_err()
        || hello.identity != identity
        || hello.build_id != build_id
        || hello.mode != mode
    {
        return;
    }
    let Some(line) = read_line(&mut reader, MAX_FRAME_BYTES).await else {
        return;
    };
    let request: DevRequest = match serde_json::from_slice(&line) {
        Ok(request) => request,
        Err(_) => return,
    };
    let is_capabilities = matches!(&request.request, BrokerRequest::Capabilities);
    let mut response =
        if request.identity != identity || request.build_id != build_id || request.mode != mode {
            BrokerResponse::failure(GoFinError::new(
                "DEVELOPMENT_IDENTITY_MISMATCH",
                "development",
                "The request identity or mode does not match this broker.",
                "Use the CLI built for this development environment.",
            ))
        } else {
            let cancellable_read = request.cancel_on_disconnect
                && request.request.may_use_rpc_reads()
                && runtime.request_uses_rpc_reads(&request.request);
            let Some(response) = gofin_protocol::progress::observe_request_with_details(
                reader.get_mut(),
                request.progress,
                request.progress_details,
                cancellable_read,
                runtime.handle_client_request(request.request, cancellable_read),
            )
            .await
            else {
                return;
            };
            response
        };
    if is_capabilities
        && response.ok
        && let Some(result) = response.result.as_mut()
    {
        result["development"] = development_metadata;
    }
    let Ok(bytes) = serde_json::to_vec(&DevResponse {
        identity: &identity,
        build_id: &build_id,
        mode: &mode,
        response,
    }) else {
        return;
    };
    if bytes.len() + 1 > MAX_RESPONSE_FRAME_BYTES {
        return;
    }
    let _ =
        tokio::time::timeout(Duration::from_secs(10), send_line(reader.get_mut(), &bytes)).await;
}

async fn read_line<R: tokio::io::AsyncRead + Unpin>(reader: &mut R, max: usize) -> Option<Vec<u8>> {
    use tokio::io::AsyncBufReadExt;
    let mut reader = BufReader::new(reader.take((max + 1) as u64));
    let mut bytes = Vec::with_capacity(256);
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        reader.read_until(b'\n', &mut bytes),
    )
    .await
    .ok()?
    .ok()?;
    if read == 0 || bytes.len() > max || bytes.last() != Some(&b'\n') {
        return None;
    }
    bytes.pop();
    Some(bytes)
}

async fn send_line(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(payload).await?;
    stream.write_all(b"\n").await
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut uid = 0u32;
    let mut gid = 0u32;
    // SAFETY: getpeereid writes fixed-width peer uid/gid values.
    (unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}
#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::{
        mem::{MaybeUninit, size_of},
        os::fd::AsRawFd,
    };
    let mut cred = MaybeUninit::<libc::ucred>::uninit();
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt initializes ucred on success with the checked size.
    (unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut len,
        )
    } == 0
        && len as usize == size_of::<libc::ucred>())
    .then(|| unsafe { cred.assume_init().uid })
}

#[tokio::main]
async fn main() {
    match parse_args() {
        Ok(args) => {
            if let Err(error) = run(args).await {
                eprintln!("gofin-dev-broker: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("gofin-dev-broker: {error}");
            std::process::exit(2);
        }
    }
}
