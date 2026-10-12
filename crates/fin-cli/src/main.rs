use async_trait::async_trait;
use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use fin_discovery::CompletionShell;
use fin_domain::{FinError, OperationRequest};
use fin_presentation::Ui;
use fin_protocol::BrokerRequest;
#[cfg(any(target_os = "macos", test))]
use fin_protocol::{BrokerResponse, MAX_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES};
mod companion_skill;
mod daemon;
pub(crate) mod json_stream;
mod onboarding;
mod shell_integration;
use serde_json::{Value, json};
use std::time::Duration;
#[cfg(target_os = "macos")]
use tokio::io::AsyncWriteExt;
#[cfg(any(target_os = "macos", test))]
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
#[cfg(target_os = "macos")]
use tokio::net::UnixStream;

#[cfg(target_os = "macos")]
const SOCKET_PATH: &str = "/Library/Fin/run/broker.sock";
#[cfg(target_os = "macos")]
const BROKER_PATH: &str = "/Library/Fin/runtime/fin-broker";
#[cfg(target_os = "macos")]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const BROKER_TIMEOUT: Duration = Duration::from_secs(8);
const FINANCIAL_WRITE_TIMEOUT: Duration = Duration::from_secs(300);
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
#[command(
    name = "fin",
    color = clap::ColorChoice::Never,
    version,
    about = "Discover and safely operate Fin's protected financial broker"
)]
struct Cli {
    /// Machine contract: one JSON result, no terminal prompts or color.
    #[arg(long, global = true)]
    json: bool,
    /// Emit NDJSON progress and one final result/error; requires --json.
    #[arg(long, global = true, requires = "json")]
    stream: bool,
    /// Show additional human-readable diagnostics; ignored with --json.
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Installer-owned shell configuration, always run as the human user.
    #[command(hide = true)]
    InternalCompletions {
        #[arg(value_enum)]
        action: shell_integration::Action,
    },
    /// Refresh previously installed, unmodified skills as the human user.
    #[command(hide = true)]
    InternalSkillsRefresh,
    /// Install, inspect, update or remove the optional bundled companion skill.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// List available commands and financial operations.
    Capabilities,
    /// Generate shell completion from the installed command definitions.
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Explain a command or operation; omit NAME for usage and examples.
    #[command(
        after_help = "Examples:\n  fin explain wallet.balances\n  fin explain connections.add\n\nList available commands and operations: fin capabilities"
    )]
    Explain {
        /// Exact command or financial operation name.
        name: Option<String>,
    },
    /// Show input, output and error schemas for a command or operation.
    #[command(
        after_help = "Examples:\n  fin schema wallet.balances\n  fin schema wallet.transfer\n\nList available commands and operations: fin capabilities"
    )]
    Schema {
        /// Exact command or financial operation name.
        name: Option<String>,
    },
    /// Read balances, positions or other financial data.
    Read(OperationArgs),
    /// Review a financial operation without moving funds.
    Preview(OperationArgs),
    /// Execute under exact approval or an existing bounded authorization; --dry-run previews.
    Execute(ExecuteArgs),
    /// Configure a human-only venue setting; real requests require your Terminal and fresh Touch ID.
    Configure(ConfigureArgs),
    /// Manage bounded financial authorizations.
    Authorizations {
        #[command(subcommand)]
        command: AuthorizationsCommand,
    },
    /// Review, authorize and track named sequential financial plans.
    Plans {
        #[command(subcommand)]
        command: PlansCommand,
    },
    /// Check and reconcile an existing operation.
    Status { id: String },
    /// List recorded operations and unresolved submissions.
    Operations,
    /// Check configuration and protected service readiness.
    Doctor,
    /// Read selected financial state with explicit coverage and incomplete results.
    Snapshot {
        /// Optional scope JSON: resources, networks and exact assets; omitted means full portfolio.
        #[arg(long)]
        input: Option<String>,
    },
    /// Install and start the protected service; macOS may request your Mac login password.
    #[command(
        after_help = "Setup asks for movement and run-allocation limits in your protected Terminal. Enter keeps $100; 0 means no personal limit. Fresh Touch ID approves the selected limits. After success, select Codex and/or Claude Code with arrows and Space, then Enter to install. Enter with none selected or Esc skips."
    )]
    Setup,
    /// Start, stop or check the protected service.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Show or change protected human-owned financial policy.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// List or securely manage exchange, wallet and pricing connections.
    Connections {
        #[command(subcommand)]
        command: Option<ConnectionsCommand>,
    },
}

#[derive(Subcommand, Debug)]
enum SkillCommand {
    /// Install for an explicitly selected agent; no administrator access required.
    Install(fin_discovery::companion_skill::SkillChange),
    /// Inspect the destination, version and managed-content status.
    Inspect(fin_discovery::companion_skill::SkillTarget),
    /// Refresh an existing installation using the bundled skill.
    Update(fin_discovery::companion_skill::SkillChange),
    /// Remove the skill; modified content requires explicit --backup handling.
    Remove(fin_discovery::companion_skill::SkillChange),
}

#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("provider_choice").args(["provider_positional", "provider"]).multiple(false)))]
struct ConnectionCommandArgs {
    /// Provider to enroll; omit to choose it interactively.
    #[arg(value_enum, value_name = "PROVIDER")]
    provider_positional: Option<fin_discovery::Provider>,
    /// Provider to enroll (alternative to the positional provider).
    #[arg(long, value_enum)]
    provider: Option<fin_discovery::Provider>,
    /// Stable local connection name.
    #[arg(long, requires = "provider_choice")]
    name: Option<String>,
    /// Binance account role: main or sub.
    #[arg(long, value_enum, requires_all = ["provider_choice", "name"])]
    role: Option<fin_discovery::BinanceRole>,
    /// Optional existing main connection for hierarchy verification and parent-authorized reads.
    #[arg(long, requires_all = ["provider_choice", "name"])]
    parent: Option<String>,
    /// Binance subaccount email; required with --parent and accepted only for role=sub.
    #[arg(long, requires_all = ["provider_choice", "name"])]
    email: Option<String>,
    /// Optional enrollment read-check network from Fin's reviewed EVM registry.
    /// The EVM wallet remains reusable across all reviewed networks.
    #[arg(long, requires_all = ["provider_choice", "name"])]
    network: Option<String>,
    /// Expected public wallet address; checked against the protected signer.
    #[arg(long, requires_all = ["provider_choice", "name"])]
    address: Option<String>,
    /// Hyperliquid account scope (owner, subaccount or vault).
    #[arg(long, value_enum, requires_all = ["provider_choice", "name"])]
    hyperliquid_scope: Option<fin_discovery::HyperliquidScopeKind>,
    /// Hyperliquid delegated subaccount or vault address.
    #[arg(long, requires_all = ["provider_choice", "name"])]
    execution_address: Option<String>,
    /// Credential category only; keys are requested privately afterward.
    /// Binance requires an API key and signing secret/key. PEM input accepts real
    /// line breaks or literal \n separators; include BEGIN/END and press Enter.
    #[arg(long, value_enum, requires_all = ["provider_choice", "name"])]
    credential_type: Option<fin_discovery::CredentialType>,
    /// CoinGecko API plan: demo or pro (defaults to demo).
    #[arg(long, value_parser = ["demo", "pro"], requires_all = ["provider_choice", "name"])]
    api_plan: Option<String>,
}

impl ConnectionCommandArgs {
    fn provider(&self) -> Option<fin_discovery::Provider> {
        self.provider_positional.or(self.provider)
    }
    fn connection(self) -> Option<fin_discovery::AddConnectionArgs> {
        Some(fin_discovery::AddConnectionArgs {
            provider: self.provider()?,
            name: self.name?,
            role: self.role,
            parent: self.parent,
            email: self.email,
            network: self.network,
            address: self.address,
            hyperliquid_scope: self.hyperliquid_scope,
            execution_address: self.execution_address,
            credential_type: self.credential_type,
            api_plan: self.api_plan,
        })
    }
}

#[derive(Args, Debug)]
struct OperationArgs {
    operation: String,
    #[arg(long)]
    input: String,
}

#[derive(Args, Debug)]
struct ConfigureArgs {
    /// Exact human-interactive configuration operation from `fin capabilities`.
    operation: String,
    #[arg(long)]
    input: String,
    /// Stable identity for this configuration action; retries must reuse it.
    #[arg(long, required_unless_present = "dry_run")]
    id: Option<String>,
    /// Prepare a safe read-only configuration preview without authorization or dispatch.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("execution_authority").args(["authorization", "plan"]).multiple(false)))]
struct ExecuteArgs {
    operation: String,
    #[arg(long)]
    input: String,
    /// Stable logical identity for retries of this exact operation.
    #[arg(long, required_unless_present = "dry_run")]
    id: Option<String>,
    /// Preview without approval or submission; --id is not consumed.
    #[arg(long)]
    dry_run: bool,
    /// Existing bounded authorization created by `fin authorizations authorize`.
    #[arg(long)]
    authorization: Option<String>,
    /// Named plan whose active revision authorizes this ordered step.
    #[arg(long, requires = "step")]
    plan: Option<String>,
    /// Stable step ID in --plan.
    #[arg(long, requires = "plan")]
    step: Option<String>,
    /// Atomically replace a prior unsubmitted candidate under the selected authority.
    #[arg(long, requires = "execution_authority")]
    replaces_attempt: Option<String>,
}

#[derive(Args, Debug)]
struct BoundaryAuthorizationArgs {
    /// Supported write operation to authorize.
    operation: String,
    /// Stable logical intent identity; retries must reuse it.
    #[arg(long)]
    id: String,
    /// Operation-specific candidate input JSON.
    #[arg(long)]
    input: String,
    /// Operation-specific permitted variations and cumulative limits as JSON.
    #[arg(long)]
    bounds: String,
    /// Consent lifetime from successful approval (for example 10m, default 10m).
    #[arg(long, default_value = "10m", value_parser = parse_duration_seconds)]
    valid_for: u64,
    /// Replace an existing authorization for the same logical intent.
    #[arg(long)]
    replaces: Option<String>,
}

#[derive(Subcommand, Debug)]
enum AuthorizationsCommand {
    /// Review and grant bounded authority for one logical intent.
    Authorize(BoundaryAuthorizationArgs),
    /// Check and reconcile a bounded authorization and its attempts.
    Status { id: String },
    /// Revoke future dispatch under a bounded authorization.
    Cancel { id: String },
}

#[derive(Args, Debug)]
struct PlanInputArgs {
    /// Complete versioned named plan JSON.
    #[arg(long)]
    input: String,
    /// Plan lifetime; omission calculates sequential allowances plus headroom.
    #[arg(long, value_parser = parse_duration_seconds)]
    valid_for: Option<u64>,
    /// Review a proposed replacement revision and its carried-forward usage.
    #[arg(long)]
    replaces: Option<String>,
}

#[derive(Args, Debug)]
struct PlanAuthorizeArgs {
    /// Complete versioned named plan JSON.
    #[arg(long)]
    input: String,
    /// Stable request identity used to recover approval response loss.
    #[arg(long)]
    id: String,
    /// Plan lifetime from successful approval (for example 45m); omission budgets the sequence.
    #[arg(long, value_parser = parse_duration_seconds)]
    valid_for: Option<u64>,
    /// Replace this active immutable plan revision.
    #[arg(long)]
    replaces: Option<String>,
    /// Bind authorization to the exact review_hash returned by plans preview.
    #[arg(long)]
    review_hash: Option<String>,
}

#[derive(Subcommand, Debug)]
enum PlansCommand {
    /// Regenerate and display read-only plan review material.
    Preview(PlanInputArgs),
    /// Approve or replace a named plan revision after fresh Touch ID.
    Authorize(PlanAuthorizeArgs),
    /// Check authorization, attempts and sequential progress.
    Status { id: String },
    /// Revoke future dispatch under a named plan without Touch ID.
    Cancel { id: String },
}

#[derive(Subcommand, Debug)]
enum DaemonCommand {
    /// Start the service and verify that it is running.
    Start,
    /// Check running or stopped state without waking the service.
    Status,
    /// Keep the service stopped until an explicit start.
    Stop,
}

#[derive(Subcommand, Debug)]
enum PolicyCommand {
    /// Show personal limits, included costs and allocation usage.
    Show,
    /// Change financial limits and an optional unpriced exchange exception in your own Terminal with fresh Touch ID; refuses unresolved work.
    Set(fin_discovery::PolicySetArgs),
}

#[derive(Subcommand, Debug)]
enum ConnectionsCommand {
    /// List enrolled connections without exposing credentials.
    List,
    /// Enroll a connection privately in your own Terminal.
    Add(ConnectionCommandArgs),
    /// Replace an existing connection's credentials with human approval.
    Update(fin_discovery::AddConnectionArgs),
    /// Remove a connection and its stored credentials with human approval.
    Remove(fin_discovery::NamedConnectionArgs),
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn call(&self, request: BrokerRequest) -> Result<Value, FinError>;
    fn status_recovery_command(&self, _id: &str) -> Option<String> {
        None
    }
    fn development_mode(&self) -> bool {
        false
    }
    fn live_development_mode(&self) -> bool {
        false
    }
}

struct UnixTransport {
    #[cfg(target_os = "macos")]
    path: std::path::PathBuf,
    #[cfg(target_os = "macos")]
    expected_uid: u32,
}

impl UnixTransport {
    fn installed() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            path: SOCKET_PATH.into(),
            #[cfg(target_os = "macos")]
            expected_uid: 0,
        }
    }

    #[cfg(target_os = "macos")]
    async fn exchange(&self, request: BrokerRequest) -> Result<Value, FinError> {
        // Explicit stop blocks ordinary socket activation. Fixture transports
        // never inspect the installed marker or touch the real service.
        if self.path == std::path::Path::new(SOCKET_PATH) {
            daemon::check_activation(&request, fin_protocol::daemon::stop_requested)?;
        }
        let response_timeout = broker_response_timeout(&request);
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.path))
            .await
            .map_err(|_| broker_timeout(&request))?
            .map_err(|_| broker_unavailable())?;
        if peer_uid(&stream)? != self.expected_uid {
            return Err(protocol_error());
        }
        let mut request_bytes = if fin_domain::execution_progress::enabled() {
            serde_json::to_vec(&fin_protocol::progress::RequestEnvelope {
                request: request.clone(),
                progress: true,
            })
        } else {
            serde_json::to_vec(&request)
        }
        .map_err(|_| protocol_error())?;
        request_bytes.push(b'\n');
        if request_bytes.len() > MAX_FRAME_BYTES {
            return Err(protocol_error());
        }
        tokio::time::timeout(CONNECT_TIMEOUT, stream.write_all(&request_bytes))
            .await
            .map_err(|_| broker_timeout(&request))?
            .map_err(|_| broker_unavailable())?;
        stream.shutdown().await.map_err(|_| broker_unavailable())?;
        tokio::time::timeout(response_timeout, read_response(BufReader::new(stream)))
            .await
            .map_err(|_| broker_timeout(&request))?
    }

    #[cfg(not(target_os = "macos"))]
    async fn exchange(&self, _request: BrokerRequest) -> Result<Value, FinError> {
        Err(FinError::new(
            "PROTECTED_PROVIDER_UNAVAILABLE",
            "transport",
            "This platform has no implemented protected broker transport.",
            "Use the reviewed macOS protected installation.",
        ))
    }
}

#[cfg(any(target_os = "macos", test))]
pub async fn read_response<R: AsyncRead + Unpin>(reader: R) -> Result<Value, FinError> {
    let mut reader = BufReader::new(reader);
    for _ in 0..64 {
        let mut bytes = Vec::with_capacity(1024);
        let read = (&mut reader)
            .take((MAX_RESPONSE_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|_| broker_unavailable())?;
        if read == 0 || bytes.len() > MAX_RESPONSE_FRAME_BYTES || bytes.last() != Some(&b'\n') {
            return Err(protocol_error());
        }
        if let Ok(frame) = serde_json::from_slice::<fin_protocol::progress::ProgressFrame>(&bytes) {
            fin_domain::execution_progress::emit(frame.progress);
            continue;
        }
        let response: BrokerResponse =
            serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        if !response.validate() {
            return Err(protocol_error());
        }
        if let Some(error) = response.error {
            return Err(error);
        }
        return response.result.ok_or_else(protocol_error);
    }
    Err(protocol_error())
}

#[async_trait]
impl Transport for UnixTransport {
    async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
        self.exchange(request).await
    }
}

async fn execute_with_recovery(
    transport: &dyn Transport,
    request: OperationRequest,
) -> Result<Value, FinError> {
    match transport
        .call(BrokerRequest::Execute {
            request: request.clone(),
        })
        .await
    {
        Ok(result) => Ok(result),
        Err(error) if error.stage == "transport" => {
            // Never submit again after a transport failure. Search the durable
            // journal by logical request identity and, if present, read status.
            if let Ok(records) = transport.call(BrokerRequest::Operations).await
                && let Some(id) = find_original_operation(&records, &request.request_id)
                && let Ok(status) = transport.call(BrokerRequest::Status { id }).await
            {
                return Ok(status);
            }
            Err(FinError::new(
                "OUTCOME_UNKNOWN",
                "transport",
                "The broker response was interrupted; Fin did not submit the request again.",
                "Retry status with the same logical request ID or inspect fin operations.",
            ))
        }
        Err(error) => Err(error),
    }
}

async fn execute_boundary_with_recovery(
    transport: &dyn Transport,
    authorization_id: String,
    request: OperationRequest,
) -> Result<Value, FinError> {
    match transport
        .call(BrokerRequest::ExecuteBoundary {
            authorization_id: authorization_id.clone(),
            request: request.clone(),
        })
        .await
    {
        Ok(result) => Ok(result),
        Err(error) if error.stage == "transport" => {
            // Reconciliation observes the durable attempt; it never resubmits it.
            if let Ok(status) = transport
                .call(BrokerRequest::BoundaryStatus {
                    id: authorization_id,
                })
                .await
                && let Some(attempt) =
                    status
                        .get("attempts")
                        .and_then(Value::as_array)
                        .and_then(|attempts| {
                            attempts.iter().find(|attempt| {
                                attempt.get("request_id").and_then(Value::as_str)
                                    == Some(request.request_id.as_str())
                            })
                        })
            {
                return Ok(attempt.clone());
            }
            Err(FinError::new(
                "OUTCOME_UNKNOWN",
                "transport",
                "The broker response was interrupted; Fin did not submit the attempt again.",
                "Run fin authorizations status with the same authorization ID and reconcile before creating another attempt.",
            ))
        }
        Err(error) => Err(error),
    }
}

async fn replace_boundary_candidate_with_recovery(
    transport: &dyn Transport,
    authorization_id: String,
    previous_attempt_id: String,
    request: OperationRequest,
) -> Result<Value, FinError> {
    match transport
        .call(BrokerRequest::ReplaceBoundaryCandidate {
            authorization_id: authorization_id.clone(),
            previous_attempt_id,
            request: request.clone(),
        })
        .await
    {
        Ok(result) => Ok(result),
        Err(error) if error.stage == "transport" => {
            if let Ok(status) = transport
                .call(BrokerRequest::BoundaryStatus {
                    id: authorization_id,
                })
                .await
                && let Some(attempt) =
                    status
                        .get("attempts")
                        .and_then(Value::as_array)
                        .and_then(|attempts| {
                            attempts.iter().find(|attempt| {
                                attempt.get("request_id").and_then(Value::as_str)
                                    == Some(request.request_id.as_str())
                            })
                        })
            {
                return Ok(attempt.clone());
            }
            Err(FinError::new(
                "OUTCOME_UNKNOWN",
                "transport",
                "The broker response was interrupted; Fin did not submit the replacement again.",
                "Run fin authorizations status with the same authorization ID and reconcile before creating another attempt.",
            ))
        }
        Err(error) => Err(error),
    }
}

async fn execute_plan_step_with_recovery(
    transport: &dyn Transport,
    plan_id: String,
    step_id: String,
    request: OperationRequest,
) -> Result<Value, FinError> {
    match transport
        .call(BrokerRequest::ExecutePlanStep {
            plan_id: plan_id.clone(),
            step_id,
            request: request.clone(),
        })
        .await
    {
        Ok(result) => Ok(result),
        Err(error) if error.stage == "transport" => {
            if let Ok(status) = transport
                .call(BrokerRequest::PlanStatus {
                    plan_id: plan_id.clone(),
                })
                .await
                && let Some(attempt) = find_plan_attempt(&status, &request.request_id)
                && let Some(attempt_id) = attempt
                    .get("attempt_id")
                    .or_else(|| attempt.get("id"))
                    .and_then(Value::as_str)
            {
                return transport
                    .call(BrokerRequest::Status {
                        id: attempt_id.to_owned(),
                    })
                    .await;
            }
            Err(FinError::new(
                "OUTCOME_UNKNOWN",
                "transport",
                "The broker response was interrupted; Fin did not submit the plan step again.",
                "Run fin plans status with the same plan ID and reconcile before creating another attempt.",
            ))
        }
        Err(error) => Err(error),
    }
}

async fn replace_plan_candidate_with_recovery(
    transport: &dyn Transport,
    plan_id: String,
    step_id: String,
    previous_attempt_id: String,
    request: OperationRequest,
) -> Result<Value, FinError> {
    match transport
        .call(BrokerRequest::ReplacePlanCandidate {
            plan_id: plan_id.clone(),
            step_id,
            previous_attempt_id,
            request: request.clone(),
        })
        .await
    {
        Ok(result) => Ok(result),
        Err(error) if error.stage == "transport" => {
            if let Ok(status) = transport
                .call(BrokerRequest::PlanStatus {
                    plan_id: plan_id.clone(),
                })
                .await
                && let Some(attempt) = find_plan_attempt(&status, &request.request_id)
                && let Some(attempt_id) = attempt
                    .get("attempt_id")
                    .or_else(|| attempt.get("id"))
                    .and_then(Value::as_str)
            {
                return transport
                    .call(BrokerRequest::Status {
                        id: attempt_id.to_owned(),
                    })
                    .await;
            }
            Err(FinError::new(
                "OUTCOME_UNKNOWN",
                "transport",
                "The broker response was interrupted; Fin did not submit the replacement again.",
                "Run fin plans status with the same plan ID and reconcile before creating another attempt.",
            ))
        }
        Err(error) => Err(error),
    }
}

fn find_plan_attempt<'a>(status: &'a Value, request_id: &str) -> Option<&'a Value> {
    status
        .get("attempts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            status
                .get("steps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|step| {
                    step.get("attempts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                }),
        )
        .find(|attempt| attempt.get("request_id").and_then(Value::as_str) == Some(request_id))
}

fn find_original_operation(records: &Value, request_id: &str) -> Option<String> {
    let records = records
        .as_array()
        .or_else(|| records.get("operations")?.as_array())?;
    records
        .iter()
        .find(|record| record.get("request_id").and_then(Value::as_str) == Some(request_id))
        .and_then(|record| record.get("id").and_then(Value::as_str))
        .map(str::to_owned)
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> Result<u32, FinError> {
    use std::os::fd::AsRawFd;
    let mut uid = 0u32;
    let mut gid = 0u32;
    // SAFETY: getpeereid writes two fixed-width ids for this connected socket.
    let result = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result == 0 {
        Ok(uid)
    } else {
        Err(protocol_error())
    }
}

#[cfg(target_os = "macos")]
#[link(name = "System")]
unsafe extern "C" {
    fn getpeereid(
        socket: libc::c_int,
        euid: *mut libc::uid_t,
        egid: *mut libc::gid_t,
    ) -> libc::c_int;
}

#[cfg(any(target_os = "macos", test))]
fn protocol_error() -> FinError {
    FinError::new(
        "BROKER_PROTOCOL_ERROR",
        "transport",
        "The protected broker returned an invalid response.",
        "Check the installed broker service and retry with the same request ID.",
    )
}
#[cfg(any(target_os = "macos", test))]
fn broker_unavailable() -> FinError {
    FinError::new(
        "BROKER_UNAVAILABLE",
        "transport",
        "The protected broker is unavailable.",
        "Start the installed Fin service and retry the same request ID.",
    )
}
pub fn broker_response_timeout(request: &BrokerRequest) -> Duration {
    match request {
        BrokerRequest::Read { operation, .. }
            if matches!(operation.as_str(), "bridge.quotes" | "bridge.status") =>
        {
            OBSERVER_TIMEOUT
        }
        BrokerRequest::Execute { .. }
        | BrokerRequest::AuthorizeBoundary { .. }
        | BrokerRequest::ExecuteBoundary { .. }
        | BrokerRequest::ReplaceBoundaryCandidate { .. }
        | BrokerRequest::AuthorizePlan { .. }
        | BrokerRequest::ExecutePlanStep { .. }
        | BrokerRequest::ReplacePlanCandidate { .. } => FINANCIAL_WRITE_TIMEOUT,
        BrokerRequest::RunBegin { .. }
        | BrokerRequest::RunReplan { .. }
        | BrokerRequest::Snapshot
        | BrokerRequest::Status { .. }
        | BrokerRequest::Read { .. }
        | BrokerRequest::PlanStatus { .. } => OBSERVER_TIMEOUT,
        BrokerRequest::Preview { .. }
        | BrokerRequest::PreviewBoundaryAuthorization { .. }
        | BrokerRequest::PreviewBoundary { .. }
        | BrokerRequest::PreviewPlan { .. }
        | BrokerRequest::PreviewPlanStep { .. } => FINANCIAL_WRITE_TIMEOUT,
        _ => BROKER_TIMEOUT,
    }
}

#[cfg(any(target_os = "macos", test))]
fn broker_timeout(request: &BrokerRequest) -> FinError {
    let next_action = if matches!(
        request,
        BrokerRequest::AuthorizeBoundary { .. } | BrokerRequest::AuthorizePlan { .. }
    ) {
        "Repeat authorization with the same request ID and parameters to recover its result; do not create another grant."
    } else if matches!(request, BrokerRequest::Status { .. }) {
        "Retry status for the same operation; do not submit another financial request."
    } else if matches!(
        request,
        BrokerRequest::Execute { .. }
            | BrokerRequest::ExecuteBoundary { .. }
            | BrokerRequest::ReplaceBoundaryCandidate { .. }
            | BrokerRequest::ExecutePlanStep { .. }
            | BrokerRequest::ReplacePlanCandidate { .. }
    ) {
        "Check status with the same request ID; do not create another financial request."
    } else {
        "Run fin daemon status --json to check the service; if startup is failing, inspect the broker error log in your own Terminal."
    };
    FinError::new(
        "BROKER_TIMEOUT",
        "transport",
        "The protected broker did not respond before the local timeout.",
        next_action,
    )
}

fn generic_input_error() -> FinError {
    FinError::new(
        "INVALID_INPUT",
        "validation",
        "The supplied JSON input is invalid.",
        "Inspect the operation schema and provide valid JSON.",
    )
}
fn parse_input(input: &str) -> Result<Value, FinError> {
    serde_json::from_str(input).map_err(|_| generic_input_error())
}

fn parse_duration_seconds(value: &str) -> Result<u64, String> {
    let (digits, multiplier) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1u64),
        Some('m') => (&value[..value.len() - 1], 60),
        Some('h') => (&value[..value.len() - 1], 60 * 60),
        Some('d') => (&value[..value.len() - 1], 24 * 60 * 60),
        Some(last) if last.is_ascii_digit() => (value, 1),
        _ => return Err("Use a positive duration such as 600s, 10m, 2h or 1d.".into()),
    };
    let amount = digits
        .parse::<u64>()
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or_else(|| "Use a positive duration such as 600s, 10m, 2h or 1d.".to_owned())?;
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| "The requested duration is too large.".to_owned())
}

fn definitions() -> Vec<fin_domain::Definition> {
    fin_discovery::operation_definitions()
}

fn financial_catalog() -> Vec<Value> {
    serde_json::from_str(include_str!("../data/financial-catalog.json"))
        .expect("bundled financial catalog must be valid JSON")
}

fn find_definition(name: &str) -> Option<Value> {
    definitions()
        .into_iter()
        .find(|definition| definition.name == name)
        .and_then(|definition| serde_json::to_value(definition).ok())
        .or_else(|| {
            financial_catalog()
                .into_iter()
                .find(|definition| definition.get("name").and_then(Value::as_str) == Some(name))
        })
}

fn capabilities() -> Value {
    json!({
        "commands": definitions(),
        "companion_skill": fin_discovery::companion_skill::metadata(),
        "operations": financial_catalog(),
        "service_required": false,
        "machine_contract": fin_discovery::machine_contract(),
        "exchanges": fin_discovery::exchange_capabilities(),
        "evm_networks": fin_discovery::evm_network_profiles(),
        "bridging": fin_discovery::bridge_profile(),
        "pricing": {"provider":"coingecko","authentication":"Optional protected human-enrolled Demo/Pro API key; otherwise anonymous public access is best effort.","enrollment_command":"fin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo","api_key_values_accepted_in_arguments":false,"fallback":"none; missing/stale/rate-limited quotes block writes"},
        "platforms": {
            "macos": {"protected_broker": true, "human_enrollment": true},
            "linux": {"offline_discovery": true, "protected_broker": false, "human_enrollment": false},
            "windows": {"offline_discovery": true, "protected_broker": false, "human_enrollment": false}
        }
    })
}

fn schema(name: &str) -> Option<Value> {
    let definition = find_definition(name)?;
    Some(json!({
        "name":name,
        "input":definition.get("input_schema"),
        "output":definition.get("output_schema"),
        "error":definition.get("error_schema"),
        "examples":definition.get("examples"),
        "semantics":definition.get("semantics"),
        "execution_actor":definition.get("execution_actor"),
    }))
}

async fn development_definition(client: &dyn Client, name: &str) -> Option<Value> {
    if !client.development_mode() {
        return None;
    }
    client
        .call(BrokerRequest::Capabilities)
        .await
        .ok()?
        .get("development")?
        .get("operations")?
        .as_array()?
        .iter()
        .find(|definition| definition.get("name").and_then(Value::as_str) == Some(name))
        .cloned()
}

fn definition_schema(name: &str, definition: &Value) -> Value {
    json!({
        "name":name,
        "input":definition.get("input_schema"),
        "output":definition.get("output_schema"),
        "error":definition.get("error_schema"),
        "examples":definition.get("examples"),
        "semantics":definition.get("semantics"),
        "execution_actor":definition.get("execution_actor"),
    })
}

fn discovery_help(name: &str) -> Value {
    let mut command = Cli::command();
    command.build();
    let help = command
        .find_subcommand_mut(name)
        .expect("derived discovery command")
        .render_help()
        .to_string();
    json!({"help": help})
}

#[async_trait]
pub trait Client: Send + Sync {
    async fn call(&self, request: BrokerRequest) -> Result<Value, FinError>;
    async fn execute(&self, request: OperationRequest) -> Result<Value, FinError>;
    fn status_recovery_command(&self, _id: &str) -> Option<String> {
        None
    }
    fn development_mode(&self) -> bool {
        false
    }
    fn live_development_mode(&self) -> bool {
        false
    }
    async fn execute_boundary(
        &self,
        authorization_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        self.call(BrokerRequest::ExecuteBoundary {
            authorization_id,
            request,
        })
        .await
    }
    async fn replace_boundary_candidate(
        &self,
        authorization_id: String,
        previous_attempt_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        self.call(BrokerRequest::ReplaceBoundaryCandidate {
            authorization_id,
            previous_attempt_id,
            request,
        })
        .await
    }
    async fn execute_plan_step(
        &self,
        plan_id: String,
        step_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        self.call(BrokerRequest::ExecutePlanStep {
            plan_id,
            step_id,
            request,
        })
        .await
    }
    async fn replace_plan_candidate(
        &self,
        plan_id: String,
        step_id: String,
        previous_attempt_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        self.call(BrokerRequest::ReplacePlanCandidate {
            plan_id,
            step_id,
            previous_attempt_id,
            request,
        })
        .await
    }
}
#[async_trait]
impl<T: Transport> Client for T {
    async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
        Transport::call(self, request).await
    }
    fn status_recovery_command(&self, id: &str) -> Option<String> {
        Transport::status_recovery_command(self, id)
    }
    fn development_mode(&self) -> bool {
        Transport::development_mode(self)
    }
    fn live_development_mode(&self) -> bool {
        Transport::live_development_mode(self)
    }
    async fn execute(&self, request: OperationRequest) -> Result<Value, FinError> {
        execute_with_recovery(self, request).await
    }
    async fn execute_boundary(
        &self,
        authorization_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        execute_boundary_with_recovery(self, authorization_id, request).await
    }
    async fn replace_boundary_candidate(
        &self,
        authorization_id: String,
        previous_attempt_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        replace_boundary_candidate_with_recovery(
            self,
            authorization_id,
            previous_attempt_id,
            request,
        )
        .await
    }
    async fn execute_plan_step(
        &self,
        plan_id: String,
        step_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        execute_plan_step_with_recovery(self, plan_id, step_id, request).await
    }
    async fn replace_plan_candidate(
        &self,
        plan_id: String,
        step_id: String,
        previous_attempt_id: String,
        request: OperationRequest,
    ) -> Result<Value, FinError> {
        replace_plan_candidate_with_recovery(self, plan_id, step_id, previous_attempt_id, request)
            .await
    }
}

#[cfg(test)]
async fn dispatch(cli: Cli, client: &dyn Client) -> Result<Value, FinError> {
    dispatch_with_review_hash(cli, client, None).await
}

async fn dispatch_with_review_hash(
    cli: Cli,
    client: &dyn Client,
    expected_review_hash: Option<String>,
) -> Result<Value, FinError> {
    let json = cli.json;
    let verbose = cli.verbose;
    if json && human_only(&cli.command) {
        return Err(human_required());
    }
    // Dry-run takes the same read-only broker route as the preview command.
    // Keep execution identities/run flags on reusable commands, but do not
    // consume them, reserve funds or enter write recovery for a preview.
    let command = match cli.command {
        Command::Execute(args)
            if args.dry_run && args.authorization.is_none() && args.plan.is_none() =>
        {
            Command::Preview(OperationArgs {
                operation: args.operation,
                input: args.input,
            })
        }
        command => command,
    };
    match command {
        Command::InternalCompletions { action } => shell_integration::run(action),
        Command::InternalSkillsRefresh => companion_skill::refresh(),
        Command::Skill { command } => {
            use companion_skill::Action;
            match command {
                SkillCommand::Inspect(args) => {
                    companion_skill::run(Action::Inspect, args.agent, false)
                }
                SkillCommand::Install(args) => {
                    companion_skill::run(Action::Install, args.target.agent, args.backup)
                }
                SkillCommand::Update(args) => {
                    companion_skill::run(Action::Update, args.target.agent, args.backup)
                }
                SkillCommand::Remove(args) => {
                    companion_skill::run(Action::Remove, args.target.agent, args.backup)
                }
            }
        }
        Command::Capabilities if client.development_mode() => {
            let broker = client.call(BrokerRequest::Capabilities).await?;
            let mut value = capabilities();
            if let Some(development) = broker.get("development") {
                value["development"] = development.clone();
                if let Some(operations) = development.get("operations") {
                    value["operations"] = operations.clone();
                }
            }
            value["service_required"] = json!(true);
            value["platforms"]["macos"]["protected_broker"] = json!(false);
            value["platforms"]["macos"]["human_enrollment"] = json!(false);
            Ok(value)
        }
        Command::Capabilities => Ok(capabilities()),
        Command::Completions { shell } => {
            let shell = match shell {
                CompletionShell::Bash => clap_complete::Shell::Bash,
                CompletionShell::Zsh => clap_complete::Shell::Zsh,
                CompletionShell::Fish => clap_complete::Shell::Fish,
            };
            let mut bytes = Vec::new();
            clap_complete::generate(shell, &mut completion_definition(), "fin", &mut bytes);
            Ok(
                json!({"shell":shell.to_string(),"script":String::from_utf8(bytes).map_err(|_| generic_input_error())?}),
            )
        }
        Command::Explain { name: None } => Ok(discovery_help("explain")),
        Command::Schema { name: None } => Ok(discovery_help("schema")),
        Command::Explain { name: Some(name) } => {
            match find_definition(&name).or(development_definition(client, &name).await) {
                Some(definition) => Ok(definition),
                None => Err(FinError::new(
                    "UNKNOWN_DEFINITION",
                    "discovery",
                    "No bundled definition has that name.",
                    "Inspect fin capabilities --json.",
                )),
            }
        }
        Command::Schema { name: Some(name) } => match schema(&name) {
            Some(schema) => Ok(schema),
            None => match development_definition(client, &name).await {
                Some(definition) => Ok(definition_schema(&name, &definition)),
                None => Err(FinError::new(
                    "UNKNOWN_DEFINITION",
                    "discovery",
                    "No bundled definition has that name.",
                    "Inspect fin capabilities --json.",
                )),
            },
        },
        Command::Read(args) => {
            client
                .call(BrokerRequest::Read {
                    operation: args.operation,
                    input: parse_input(&args.input)?,
                })
                .await
        }
        Command::Preview(args) => {
            client
                .call(BrokerRequest::Preview {
                    operation: args.operation,
                    input: parse_input(&args.input)?,
                })
                .await
        }
        Command::Execute(args) => {
            if args.dry_run && args.replaces_attempt.is_some() {
                return Err(FinError::validation(
                    "--replaces-attempt cannot be combined with --dry-run.",
                ));
            }
            let input = parse_input(&args.input)?;
            if let Some(plan_id) = args.plan {
                let step_id = args.step.ok_or_else(generic_input_error)?;
                if args.dry_run {
                    client
                        .call(BrokerRequest::PreviewPlanStep {
                            plan_id,
                            step_id,
                            operation: args.operation,
                            input,
                        })
                        .await
                } else {
                    let request = OperationRequest {
                        request_id: args.id.ok_or_else(generic_input_error)?,
                        operation: args.operation,
                        input,
                        run_id: None,
                    };
                    if let Some(previous_attempt_id) = args.replaces_attempt {
                        client
                            .replace_plan_candidate(plan_id, step_id, previous_attempt_id, request)
                            .await
                    } else {
                        client.execute_plan_step(plan_id, step_id, request).await
                    }
                }
            } else if let Some(authorization_id) = args.authorization {
                if args.dry_run {
                    client
                        .call(BrokerRequest::PreviewBoundary {
                            id: authorization_id,
                            operation: args.operation,
                            input,
                        })
                        .await
                } else {
                    let request = OperationRequest {
                        request_id: args.id.ok_or_else(generic_input_error)?,
                        operation: args.operation,
                        input,
                        run_id: None,
                    };
                    if let Some(previous_attempt_id) = args.replaces_attempt {
                        client
                            .replace_boundary_candidate(
                                authorization_id,
                                previous_attempt_id,
                                request,
                            )
                            .await
                    } else {
                        client.execute_boundary(authorization_id, request).await
                    }
                }
            } else {
                if args.replaces_attempt.is_some() {
                    return Err(FinError::validation(
                        "--replaces-attempt requires --authorization or --plan.",
                    ));
                }
                client
                    .execute(OperationRequest {
                        request_id: args.id.ok_or_else(generic_input_error)?,
                        operation: args.operation,
                        input,
                        run_id: None,
                    })
                    .await
            }
        }
        Command::Configure(args) => {
            let input = parse_input(&args.input)?;
            if args.dry_run {
                client
                    .call(BrokerRequest::ConfigurationPreview {
                        operation: args.operation,
                        input,
                    })
                    .await
            } else {
                launch_human_privileged(
                    PrivilegedAction::Configure(OperationRequest {
                        request_id: args.id.ok_or_else(generic_input_error)?,
                        operation: args.operation,
                        input,
                        run_id: None,
                    }),
                    json,
                    verbose,
                )
            }
        }
        Command::Authorizations { command } => match command {
            AuthorizationsCommand::Authorize(args) => {
                let request = boundary_authorization_request(&args)?;
                client
                    .call(BrokerRequest::AuthorizeBoundary { request })
                    .await
            }
            AuthorizationsCommand::Status { id } => {
                client.call(BrokerRequest::BoundaryStatus { id }).await
            }
            AuthorizationsCommand::Cancel { id } => {
                client.call(BrokerRequest::RevokeBoundary { id }).await
            }
        },
        Command::Plans { command } => match command {
            PlansCommand::Preview(args) => {
                let request = financial_plan_request(
                    &args.input,
                    "preview",
                    args.valid_for,
                    args.replaces,
                    None,
                )?;
                client.call(BrokerRequest::PreviewPlan { request }).await
            }
            PlansCommand::Authorize(args) => {
                let request = financial_plan_request(
                    &args.input,
                    &args.id,
                    args.valid_for,
                    args.replaces,
                    args.review_hash.or(expected_review_hash),
                )?;
                client.call(BrokerRequest::AuthorizePlan { request }).await
            }
            PlansCommand::Status { id } => {
                client.call(BrokerRequest::PlanStatus { plan_id: id }).await
            }
            PlansCommand::Cancel { id } => {
                client.call(BrokerRequest::RevokePlan { plan_id: id }).await
            }
        },
        Command::Status { id } => client.call(BrokerRequest::Status { id }).await,
        Command::Operations => client.call(BrokerRequest::Operations).await,
        Command::Doctor => client.call(BrokerRequest::Doctor).await,
        Command::Snapshot { input } => {
            if let Some(input) = input {
                let scope: fin_domain::SnapshotScope =
                    serde_json::from_str(&input).map_err(|_| generic_input_error())?;
                client
                    .call(BrokerRequest::Read {
                        operation: "fin.snapshot".into(),
                        input: serde_json::to_value(scope).map_err(|_| generic_input_error())?,
                    })
                    .await
            } else {
                client.call(BrokerRequest::Snapshot).await
            }
        }
        Command::Daemon { command } => daemon::run(command, client).await,
        Command::Setup => launch_human_privileged(PrivilegedAction::Setup, json, verbose),
        Command::Policy { command } => match command {
            PolicyCommand::Show => client.call(BrokerRequest::PolicyShow).await,
            PolicyCommand::Set(args) => {
                args.micros()?;
                launch_human_privileged(PrivilegedAction::PolicySet(args), json, verbose)
            }
        },
        Command::Connections { command } => match command {
            None | Some(ConnectionsCommand::List) => client.call(BrokerRequest::Connections).await,
            Some(ConnectionsCommand::Add(args)) => {
                let provider = args.provider();
                let args = match args.connection() {
                    Some(args) => args,
                    None if stdin_is_terminal() => onboarding::guided_add(provider)?,
                    None => return Err(human_required()),
                };
                fin_discovery::validate_add(&args)?;
                launch_human_privileged(PrivilegedAction::Add(args), json, verbose)
            }
            Some(ConnectionsCommand::Update(args)) => {
                fin_discovery::validate_add(&args)?;
                launch_human_privileged(PrivilegedAction::Update(args), json, verbose)
            }
            Some(ConnectionsCommand::Remove(args)) => {
                fin_discovery::validate_name(&args.name)?;
                launch_human_privileged(PrivilegedAction::Remove(args), json, verbose)
            }
        },
    }
}

async fn review_human_execution(cli: &Cli, client: &dyn Client, ui: &Ui) -> Result<(), FinError> {
    let Command::Execute(args) = &cli.command else {
        return Ok(());
    };
    if cli.json || args.dry_run || args.authorization.is_some() || args.plan.is_some() {
        return Ok(());
    }
    // Reject missing execution identity before any service work. Preview is
    // read-only; execution/recovery and its fresh bound approval remain separate.
    let request_id = args.id.as_ref().ok_or_else(generic_input_error)?;
    let progress = ui.progress("Preparing preview...");
    let records = client.call(BrokerRequest::Operations).await?;
    if find_original_operation(&records, request_id).is_some() {
        // An existing request must retain its normal duplicate/recovery path.
        // A fresh preview could fail against holdings already spent by that
        // request, preventing the user from obtaining its original result.
        drop(progress);
        return Ok(());
    }
    let result = client
        .call(BrokerRequest::Preview {
            operation: args.operation.clone(),
            input: parse_input(&args.input)?,
        })
        .await;
    drop(progress);
    let preview = result?;
    if preview["summary"].as_str().is_none_or(str::is_empty) {
        return Err(FinError::new(
            "PREVIEW_INVALID",
            "preview",
            "The operation preview has no approval summary.",
            "Inspect the operation before executing it.",
        ));
    }
    let review_result = if client.live_development_mode() {
        ui.live_development_review(&preview, false)
    } else if client.development_mode() {
        ui.synthetic_review(&preview, false)
    } else {
        ui.approval_review(&preview)
    };
    review_result.map_err(|_| {
        FinError::new(
            "REVIEW_OUTPUT_FAILED",
            "cli",
            "The operation summary could not be displayed. Nothing was submitted.",
            "Restore terminal output before retrying.",
        )
    })
}

fn boundary_authorization_request(
    args: &BoundaryAuthorizationArgs,
) -> Result<fin_domain::boundaries::BoundaryAuthorizationRequest, FinError> {
    Ok(fin_domain::boundaries::BoundaryAuthorizationRequest {
        request_id: args.id.clone(),
        operation: args.operation.clone(),
        input: parse_input(&args.input)?,
        bounds: parse_input(&args.bounds)?,
        valid_for_seconds: args.valid_for,
        replaces: args.replaces.clone(),
    })
}

fn financial_plan_request(
    input: &str,
    request_id: &str,
    valid_for_seconds: Option<u64>,
    replaces_revision_id: Option<String>,
    expected_review_hash: Option<String>,
) -> Result<fin_domain::financial_plans::FinancialPlanAuthorizationRequest, FinError> {
    let raw = parse_input(input)?;
    let plan: fin_domain::financial_plans::FinancialPlanSpec =
        serde_json::from_value(raw).map_err(|_| generic_input_error())?;
    let request = fin_domain::financial_plans::FinancialPlanAuthorizationRequest {
        request_id: request_id.to_owned(),
        plan,
        valid_for_seconds,
        replaces_revision_id,
        expected_review_hash,
    };
    request.validate()?;
    Ok(request)
}

async fn review_boundary_authorization(
    cli: &Cli,
    client: &dyn Client,
    ui: &Ui,
) -> Result<(), FinError> {
    let Command::Authorizations {
        command: AuthorizationsCommand::Authorize(args),
    } = &cli.command
    else {
        return Ok(());
    };
    if cli.json {
        return Ok(());
    }
    let progress = ui.progress("Preparing authorization review...");
    let result = client
        .call(BrokerRequest::PreviewBoundaryAuthorization {
            request: boundary_authorization_request(args)?,
        })
        .await;
    drop(progress);
    let preview = result?;
    if preview["summary"].as_str().is_none_or(str::is_empty) {
        return Err(FinError::new(
            "PREVIEW_INVALID",
            "authorization",
            "The boundary authorization review has no human-readable summary.",
            "Inspect the operation and its bounds before requesting Touch ID.",
        ));
    }
    let review_result = if client.live_development_mode() {
        ui.live_development_review(&preview, true)
    } else if client.development_mode() {
        ui.synthetic_review(&preview, true)
    } else {
        ui.authorization_review(&preview)
    };
    review_result.map_err(|_| {
        FinError::new(
            "REVIEW_OUTPUT_FAILED",
            "cli",
            "The authorization summary could not be displayed. No authorization was requested.",
            "Restore terminal output before retrying.",
        )
    })
}

async fn review_plan_authorization(
    cli: &Cli,
    client: &dyn Client,
    ui: &Ui,
) -> Result<Option<String>, FinError> {
    let Command::Plans {
        command: PlansCommand::Authorize(args),
    } = &cli.command
    else {
        return Ok(None);
    };
    if cli.json {
        return Ok(None);
    }
    let request = financial_plan_request(
        &args.input,
        &args.id,
        args.valid_for,
        args.replaces.clone(),
        None,
    )?;
    let expected_plan = request.plan.clone();
    let progress = ui.progress("Preparing named plan review...");
    let result = client.call(BrokerRequest::PreviewPlan { request }).await;
    drop(progress);
    let preview = result?;
    let valid_permissions = preview["permissions"].as_array().is_some_and(|rows| {
        rows.len() == expected_plan.steps.len()
            && rows
                .iter()
                .zip(&expected_plan.steps)
                .all(|(row, step)| row["step_id"].as_str() == Some(step.step_id.as_str()))
    });
    if preview["review_hash"].as_str().is_none_or(str::is_empty)
        || preview["plan"] != serde_json::to_value(&expected_plan).unwrap_or(Value::Null)
        || !valid_permissions
        || preview["valid_for_seconds"].as_u64().is_none_or(|seconds| {
            seconds == 0 || args.valid_for.is_some_and(|expected| expected != seconds)
        })
    {
        return Err(FinError::new(
            "PREVIEW_INVALID",
            "authorization",
            "The named plan review is incomplete; no authorization was requested.",
            "Inspect plan status and retry after the broker can prepare every step.",
        ));
    }
    if args
        .review_hash
        .as_ref()
        .is_some_and(|expected| preview["review_hash"].as_str() != Some(expected))
    {
        return Err(FinError::new(
            "PLAN_REVIEW_CHANGED",
            "authorization",
            "The supplied review hash no longer matches freshly prepared plan permissions.",
            "Inspect the latest fin plans preview output, then authorize with its review hash if the change is intended.",
        ));
    }
    // This is display-only metadata. It is not forwarded to the broker and is
    // not part of the canonical review hash or authorization binding.
    let display_review = plan_review_for_display(&preview, &args.id);
    let output = if client.live_development_mode() {
        ui.live_development_review(&display_review, true)
    } else if client.development_mode() {
        ui.synthetic_review(&display_review, true)
    } else {
        ui.plan_authorization_review(&display_review)
    };
    output.map_err(|_| {
        FinError::new(
            "REVIEW_OUTPUT_FAILED",
            "cli",
            "The named plan summary could not be displayed. No authorization was requested.",
            "Restore terminal output before retrying.",
        )
    })?;
    Ok(preview["review_hash"].as_str().map(str::to_owned))
}

fn plan_review_for_display(preview: &Value, request_id: &str) -> Value {
    let mut review = preview.clone();
    review["authorization_request_id"] = json!(request_id);
    review
}

// clap_complete includes hidden subcommands in some generated shells. Keep
// installer-only plumbing out of both automatic and manual completion menus.
fn completion_definition() -> clap::Command {
    let definition = Cli::command();
    clap::Command::new("fin")
        .version(env!("CARGO_PKG_VERSION"))
        .args(definition.get_arguments().cloned())
        .subcommands(
            definition
                .get_subcommands()
                .filter(|command| !command.is_hide_set())
                .cloned(),
        )
}

enum PrivilegedAction {
    Setup,
    PolicySet(fin_discovery::PolicySetArgs),
    Configure(OperationRequest),
    Add(fin_discovery::AddConnectionArgs),
    Update(fin_discovery::AddConnectionArgs),
    Remove(fin_discovery::NamedConnectionArgs),
}

fn launch_human_privileged(
    action: PrivilegedAction,
    json: bool,
    verbose: bool,
) -> Result<Value, FinError> {
    launch_human_privileged_with_terminal(action, stdin_is_terminal(), json, verbose)
}

fn launch_human_privileged_with_terminal(
    action: PrivilegedAction,
    is_terminal: bool,
    json: bool,
    verbose: bool,
) -> Result<Value, FinError> {
    if json {
        return Err(human_required());
    }
    if !is_terminal {
        return Err(FinError::new(
            "HUMAN_TERMINAL_REQUIRED",
            "enrollment",
            "This command requires a human-owned interactive terminal.",
            "Run it yourself in Terminal; never provide credential values through an agent-controlled interface.",
        ));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = verbose;
        // Consume the public metadata without starting an installer, privilege
        // command, or secret prompt on an unsupported platform.
        match action {
            PrivilegedAction::Setup => {}
            PrivilegedAction::Add(args) | PrivilegedAction::Update(args) => drop(args),
            PrivilegedAction::Remove(args) => drop(args),
            PrivilegedAction::PolicySet(args) => drop(args),
            PrivilegedAction::Configure(request) => drop(request),
        }
        Err(FinError::new(
            "PROTECTED_PROVIDER_UNAVAILABLE",
            "enrollment",
            "Protected installation and credential enrollment are implemented only for macOS.",
            "Use the reviewed macOS Fin installation.",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        let reason = administrator_authentication_reason(&action);
        let mut command = prepare_privileged_command(action, verbose)?;
        exec_after_administrator_notice(reason, &mut std::io::stderr().lock(), || command.exec())
    }
}

#[cfg(target_os = "macos")]
fn prepare_privileged_command(
    action: PrivilegedAction,
    verbose: bool,
) -> Result<std::process::Command, FinError> {
    prepare_privileged_command_with_presentation(
        action,
        verbose,
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var_os("TERM").is_some_and(|value| value == "dumb"),
    )
}

#[cfg(target_os = "macos")]
fn prepare_privileged_command_with_presentation(
    action: PrivilegedAction,
    verbose: bool,
    no_color: bool,
    dumb: bool,
) -> Result<std::process::Command, FinError> {
    let mut command = std::process::Command::new("/usr/bin/sudo");
    command.env_clear().env("PATH", "/usr/bin:/bin").args([
        "-k",
        "-p",
        fin_discovery::MACOS_ADMIN_PASSWORD_PROMPT,
        "--",
    ]);
    // Fixed presentation-only values survive sudo's environment reset. Never
    // forward arbitrary caller environment into the protected process.
    if no_color {
        command.arg("NO_COLOR=1");
    }
    if dumb {
        command.arg("TERM=dumb");
    }
    match action {
        PrivilegedAction::Setup => {
            let script = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|parent| parent.join("install.sh")))
                .filter(|path| path.is_absolute())
                .ok_or_else(|| {
                    FinError::new(
                        "INSTALLER_MISSING",
                        "setup",
                        "The bundled Fin installer could not be located beside this executable.",
                        "Run setup from the reviewed installation bundle.",
                    )
                })?;
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            if uid < 501 {
                return Err(FinError::new(
                    "HUMAN_ACCOUNT_REQUIRED",
                    "setup",
                    "Fin setup requires a standard human account.",
                    "Run setup from your own macOS Terminal account.",
                ));
            }
            command
                .arg("/bin/bash")
                .arg(script)
                .arg("--approver-uid")
                .arg(uid.to_string())
                .arg("--approver-gid")
                .arg(gid.to_string());
        }
        PrivilegedAction::Add(args) => {
            command
                .arg(BROKER_PATH)
                .args(["enrollment", "add", "--request"])
                .arg(serde_json::to_string(&args).map_err(|_| generic_input_error())?);
        }
        PrivilegedAction::Update(args) => {
            command
                .arg(BROKER_PATH)
                .args(["enrollment", "update", "--request"])
                .arg(serde_json::to_string(&args).map_err(|_| generic_input_error())?);
        }
        PrivilegedAction::PolicySet(args) => {
            command
                .arg(BROKER_PATH)
                .args(["enrollment", "policy-set", "--request"])
                .arg(serde_json::to_string(&args).map_err(|_| generic_input_error())?);
        }
        PrivilegedAction::Configure(request) => {
            command
                .arg(BROKER_PATH)
                .args(["enrollment", "configure", "--request"])
                .arg(serde_json::to_string(&request).map_err(|_| generic_input_error())?);
        }
        PrivilegedAction::Remove(args) => {
            command
                .arg(BROKER_PATH)
                .args(["enrollment", "remove", "--request"])
                .arg(serde_json::to_string(&args).map_err(|_| generic_input_error())?);
        }
    }
    if verbose {
        command.arg("--verbose");
    }
    Ok(command)
}

#[cfg(any(target_os = "macos", test))]
fn administrator_authentication_reason(action: &PrivilegedAction) -> &'static str {
    match action {
        PrivilegedAction::Setup => {
            "Fin needs administrator access to install its protected service and keep your secrets safe from the LLMs"
        }
        PrivilegedAction::Add(_) => {
            "Fin needs administrator access to securely add this connection."
        }
        PrivilegedAction::Update(_) => {
            "Fin needs administrator access to securely update this connection."
        }
        PrivilegedAction::PolicySet(_) => {
            "Fin needs administrator access to change protected financial policy. Touch ID separately approves the exact limits and unpriced exchange setting."
        }
        PrivilegedAction::Configure(_) => {
            "Fin needs administrator access to review and configure this Hyperliquid account. Touch ID separately approves the exact venue setting."
        }
        PrivilegedAction::Remove(_) => "Fin needs administrator access to remove this connection.",
    }
}

#[cfg(any(target_os = "macos", test))]
fn exec_after_administrator_notice(
    reason: &str,
    output: &mut impl std::io::Write,
    exec: impl FnOnce() -> std::io::Error,
) -> Result<Value, FinError> {
    writeln!(
        output,
        "{reason}\n{}",
        fin_discovery::MACOS_ADMIN_PASSWORD_HELP
    )
    .and_then(|_| output.flush())
    .map_err(|_| {
        FinError::new(
            "ADMINISTRATOR_NOTICE_UNAVAILABLE",
            "enrollment",
            "Fin could not display the administrator authentication explanation.",
            "Check terminal output and run the command again in your own Terminal.",
        )
    })?;
    let error = exec();
    Err(FinError::new(
        "PRIVILEGED_EXEC_FAILED",
        "enrollment",
        "The fixed protected Fin command could not be started.",
        "Check the reviewed installation and run the command again in Terminal.",
    )
    .into_with_detail(error))
}

#[cfg(any(target_os = "macos", test))]
trait ErrorWithoutEcho {
    fn into_with_detail(self, error: std::io::Error) -> FinError;
}
#[cfg(any(target_os = "macos", test))]
impl ErrorWithoutEcho for FinError {
    fn into_with_detail(mut self, _error: std::io::Error) -> FinError {
        // Deliberately suppress OS error text because exec diagnostics can
        // contain command-line context and are not safe for structured output.
        self.next_action =
            "Check the reviewed installation and run the command again in Terminal.".into();
        self
    }
}

fn stdin_is_terminal() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty only inspects the process's standard input descriptor.
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn human_required() -> FinError {
    FinError::new(
        "HUMAN_TERMINAL_REQUIRED",
        "enrollment",
        "This command requires your own interactive Terminal.",
        "Run the command yourself without --json; never send credentials through chat or an agent terminal.",
    )
}

fn human_only(command: &Command) -> bool {
    matches!(
        command,
        Command::Setup
            | Command::Policy {
                command: PolicyCommand::Set(_)
            }
            | Command::Configure(ConfigureArgs { dry_run: false, .. })
            | Command::Connections {
                command: Some(
                    ConnectionsCommand::Add(_)
                        | ConnectionsCommand::Update(_)
                        | ConnectionsCommand::Remove(_)
                )
            }
    )
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::InternalCompletions { .. } => "internal-completions",
        Command::InternalSkillsRefresh => "internal-skills-refresh",
        Command::Skill { .. } => "skill",
        Command::Capabilities => "capabilities",
        Command::Explain { .. } => "explain",
        Command::Schema { .. } => "schema",
        Command::Completions { .. } => "completions",
        Command::Read(_) => "read",
        Command::Preview(_) => "preview",
        Command::Execute(args) if args.dry_run => "preview",
        Command::Execute(_) => "execute",
        Command::Configure(args) if args.dry_run => "preview",
        Command::Configure(_) => "configure",
        Command::Authorizations { .. } => "authorizations",
        Command::Plans { .. } => "plans",
        Command::Status { .. } => "status",
        Command::Operations => "operations",
        Command::Doctor => "doctor",
        Command::Snapshot { .. } => "snapshot",
        Command::Setup => "setup",
        Command::Daemon { .. } => "daemon",
        Command::Connections { .. } => "connections",
        Command::Policy { .. } => "policy",
    }
}

// Resolve output mode even for parse failures, without inspecting or echoing values.
fn requested_json(args: &[std::ffi::OsString]) -> bool {
    requested_flag(args, "--json")
}

fn requested_flag(args: &[std::ffi::OsString], flag: &str) -> bool {
    let mut value = false;
    for argument in args.iter().skip(1) {
        if value {
            value = false;
            continue;
        }
        if argument == "--" {
            break;
        }
        if argument == flag {
            return true;
        }
        value = matches!(
            argument.to_str(),
            Some(
                "--input"
                    | "--bounds"
                    | "--valid-for"
                    | "--replaces"
                    | "--review-hash"
                    | "--authorization"
                    | "--plan"
                    | "--step"
                    | "--replaces-attempt"
                    | "--id"
                    | "--agent"
                    | "--provider"
                    | "--name"
                    | "--role"
                    | "--parent"
                    | "--email"
                    | "--network"
                    | "--address"
                    | "--credential-type"
            )
        );
    }
    false
}

fn emit(result: Result<Value, FinError>, ui: &Ui, command: &str) -> i32 {
    emit_with_status_command(result, ui, command, None)
}

fn emit_with_status_command(
    result: Result<Value, FinError>,
    ui: &Ui,
    command: &str,
    status_command: Option<&str>,
) -> i32 {
    match result {
        Ok(value) => {
            let output = if command == "completions" && !ui.is_machine() {
                use std::io::Write;
                std::io::stdout()
                    .lock()
                    .write_all(value["script"].as_str().unwrap_or_default().as_bytes())
            } else if matches!(command, "explain" | "schema")
                && value.get("help").is_some()
                && !ui.is_machine()
            {
                use std::io::Write;
                std::io::stdout()
                    .lock()
                    .write_all(value["help"].as_str().unwrap_or_default().as_bytes())
            } else if command == "internal-completions" && !ui.is_machine() {
                let message = value["message"].as_str().unwrap_or_default();
                if matches!(value["status"].as_str(), Some("installed" | "removed")) {
                    ui.success(message)
                } else {
                    ui.warning(message)
                }
            } else if command == "skill" && !ui.is_machine() && !ui.is_verbose() {
                let text = format!(
                    "{} {}",
                    value["message"].as_str().unwrap_or_default(),
                    value["next_action"].as_str().unwrap_or_default()
                );
                if matches!(value["status"].as_str(), Some("modified" | "unmanaged")) {
                    ui.warning(&text)
                } else {
                    ui.success(&text)
                }
            } else if command == "internal-skills-refresh" && !ui.is_machine() {
                for result in value["results"].as_array().into_iter().flatten() {
                    if matches!(result["status"].as_str(), Some("current" | "updated")) {
                        let _ = ui.success(result["message"].as_str().unwrap_or_default());
                    } else {
                        let _ = ui.warning(&format!(
                            "Fin skill preserved; run fin skill inspect --agent {} for details.",
                            result["agent"].as_str().unwrap_or("codex")
                        ));
                    }
                }
                Ok(())
            } else if command == "daemon" && !ui.is_machine() {
                ui.success(fin_presentation::render_human(command, &value, false).trim_end())
            } else {
                ui.write_result_with_status_command(&value, command, status_command)
            };
            i32::from(output.is_err())
        }
        Err(error) => {
            let _ = ui.write_error(&error);
            1
        }
    }
}

pub async fn run_with_client(arguments: Vec<std::ffi::OsString>, client: &dyn Client) -> i32 {
    let machine = requested_json(&arguments);
    let verbose = requested_flag(&arguments, "--verbose");
    let stream =
        (machine && requested_flag(&arguments, "--stream")).then(json_stream::JsonStream::stdout);
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            if machine {
                let value = if error.kind() == ErrorKind::DisplayVersion {
                    json!({"name":"fin", "version":env!("CARGO_PKG_VERSION")})
                } else {
                    json!({"help":error.to_string()})
                };
                return if let Some(stream) = stream {
                    stream.finish(Ok(value))
                } else {
                    emit(Ok(value), &Ui::new(true, false), "help")
                };
            }
            let _ = error.print();
            return 0;
        }
        Err(_) => {
            let error = FinError::new(
                "INVALID_COMMAND",
                "cli",
                "The command or arguments are invalid.",
                "Run fin --help to inspect supported commands.",
            );
            return if let Some(stream) = stream {
                stream.finish(Err(error))
            } else {
                emit(Err(error), &Ui::new(machine, verbose), "cli")
            };
        }
    };
    let stream = stream.map(|stream| stream.with_context(stream_context(&cli.command)));
    let ui = Ui::new(cli.json, cli.verbose);
    let name = command_name(&cli.command);
    if client.development_mode() && !development_command_allowed(&cli.command) {
        let error = FinError::new(
            "DEVELOPMENT_COMMAND_UNAVAILABLE",
            "development",
            "Protected setup and connection management are unavailable in development environments.",
            "Use read-only discovery or a supported development operation in this environment.",
        );
        return if let Some(stream) = stream {
            stream.finish(Err(error))
        } else {
            emit(Err(error), &ui, name)
        };
    }
    // Privileged commands replace this process; never leave a spinner alive across exec.
    let offline = matches!(
        cli.command,
        Command::Capabilities
            | Command::Explain { .. }
            | Command::Schema { .. }
            | Command::Completions { .. }
            | Command::InternalCompletions { .. }
            | Command::InternalSkillsRefresh
            | Command::Skill { .. }
    );
    let human_execution = !cli.json
        && (matches!(&cli.command, Command::Execute(args) if !args.dry_run && args.authorization.is_none() && args.plan.is_none())
            || matches!(
                &cli.command,
                Command::Authorizations {
                    command: AuthorizationsCommand::Authorize(_)
                }
            )
            || matches!(
                &cli.command,
                Command::Plans {
                    command: PlansCommand::Authorize(_)
                }
            ));
    let stage_progress = (!cli.json || cli.stream)
        && matches!(
            &cli.command,
            Command::Execute(_)
                | Command::Status { .. }
                | Command::Preview(_)
                | Command::Authorizations {
                    command: AuthorizationsCommand::Authorize(_)
                        | AuthorizationsCommand::Status { .. }
                }
                | Command::Plans {
                    command: PlansCommand::Authorize(_)
                        | PlansCommand::Preview(_)
                        | PlansCommand::Status { .. }
                }
        );
    let progress = if !offline && !human_only(&cli.command) && !human_execution && !stage_progress {
        Some(ui.progress(match &cli.command {
            Command::Daemon {
                command: DaemonCommand::Start,
            } => "Starting Fin daemon...",
            Command::Daemon {
                command: DaemonCommand::Stop,
            } => "Stopping Fin daemon...",
            Command::Daemon {
                command: DaemonCommand::Status,
            } => "Checking daemon status...",
            Command::Execute(args) if args.dry_run => "Preparing preview...",
            Command::Execute(args) if args.plan.is_some() => "Executing approved plan step...",
            Command::Execute(args) if args.authorization.is_some() => {
                "Executing authorized attempt..."
            }
            Command::Preview(_) => "Preparing preview...",
            Command::Execute(_) => "Waiting for Touch ID and execution...",
            Command::Authorizations {
                command: AuthorizationsCommand::Authorize(_),
            } if !cli.json => "Preparing authorization review...",
            Command::Authorizations {
                command: AuthorizationsCommand::Authorize(_),
            } => "Requesting boundary authorization...",
            Command::Plans {
                command: PlansCommand::Authorize(_),
            } if !cli.json => "Preparing named plan review...",
            Command::Plans {
                command: PlansCommand::Authorize(_),
            } => "Requesting plan authorization...",
            Command::Plans {
                command: PlansCommand::Preview(_),
            } => "Preparing named plan preview...",
            _ => "Checking Fin...",
        }))
    } else {
        None
    };
    let result = match review_human_execution(&cli, client, &ui).await {
        Ok(()) => match review_boundary_authorization(&cli, client, &ui).await {
            Ok(()) => match review_plan_authorization(&cli, client, &ui).await {
                Ok(review_hash) => {
                    execution_feedback(
                        &ui,
                        stage_progress,
                        client.development_mode(),
                        stream.as_ref(),
                        dispatch_with_review_hash(cli, client, review_hash),
                    )
                    .await
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    drop(progress);
    let status_command = result
        .as_ref()
        .ok()
        .and_then(|value| value["id"].as_str())
        .and_then(|id| client.status_recovery_command(id));
    if let Some(stream) = stream {
        stream.finish(result)
    } else {
        emit_with_status_command(result, &ui, name, status_command.as_deref())
    }
}

fn stream_context(command: &Command) -> Value {
    let mut context = json!({"command":command_name(command)});
    match command {
        Command::Execute(args) => {
            context["operation"] = json!(args.operation);
            if let Some(id) = &args.id {
                context["request_id"] = json!(id);
            }
            if let Some(id) = &args.plan {
                context["plan_id"] = json!(id);
            }
            if let Some(id) = &args.step {
                context["step_id"] = json!(id);
            }
        }
        Command::Read(args) | Command::Preview(args) => {
            context["operation"] = json!(args.operation);
        }
        Command::Status { id } => context["operation_id"] = json!(id),
        Command::Plans {
            command: PlansCommand::Authorize(args),
        } => {
            context["request_id"] = json!(args.id);
        }
        Command::Plans {
            command: PlansCommand::Status { id } | PlansCommand::Cancel { id },
        } => {
            context["plan_id"] = json!(id);
        }
        Command::Authorizations {
            command: AuthorizationsCommand::Authorize(args),
        } => {
            context["operation"] = json!(args.operation);
            context["request_id"] = json!(args.id);
        }
        Command::Authorizations {
            command: AuthorizationsCommand::Status { id } | AuthorizationsCommand::Cancel { id },
        } => {
            context["authorization_id"] = json!(id);
        }
        _ => {}
    }
    context
}

async fn execution_feedback<F: std::future::Future>(
    ui: &Ui,
    enabled: bool,
    development: bool,
    stream: Option<&json_stream::JsonStream>,
    work: F,
) -> F::Output {
    if !enabled {
        return work.await;
    }
    let (sender, mut receiver) = fin_domain::execution_progress::channel();
    let feedback = async {
        let mut active = if let Some(stream) = stream {
            stream.connecting();
            None
        } else {
            Some(ui.progress("Connecting to Fin..."))
        };
        let mut previous = None;
        while let Some(stage) = receiver.recv().await {
            if previous == Some(stage) {
                continue;
            }
            previous = Some(stage);
            drop(active.take());
            if let Some(stream) = stream {
                stream.progress(stage);
            } else {
                active = ui.execution_stage(stage, development);
            }
        }
        drop(active);
    };
    let (result, ()) = tokio::join!(
        fin_domain::execution_progress::observe(sender, work),
        feedback
    );
    result
}

fn development_command_allowed(command: &Command) -> bool {
    matches!(
        command,
        Command::Capabilities
            | Command::Explain { .. }
            | Command::Schema { .. }
            | Command::Completions { .. }
            | Command::Read(_)
            | Command::Preview(_)
            | Command::Execute(_)
            | Command::Authorizations { .. }
            | Command::Plans { .. }
            | Command::Status { .. }
            | Command::Operations
            | Command::Doctor
            | Command::Snapshot { .. }
            | Command::Policy {
                command: PolicyCommand::Show
            }
            | Command::Connections {
                command: None | Some(ConnectionsCommand::List)
            }
    )
}

#[tokio::main]
async fn main() {
    let arguments: Vec<_> = std::env::args_os().collect();
    std::process::exit(run_with_client(arguments, &UnixTransport::installed()).await);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;
    #[cfg(target_os = "macos")]
    use tokio::net::UnixListener;

    fn sample_plan_json() -> &'static str {
        r#"{"version":1,"name":"Synthetic transfer","steps":[{"step_id":"transfer","operation":"wallet.transfer","input":{"request_id":"synthetic-input","wallet":"main-wallet","network":"evm","chain_id":1,"token":"native","amount":"0.01","recipient":"0x0000000000000000000000000000000000000001"},"permitted_variations":{"mode":"exact","maximum_debits":[],"maximum_costs":[],"possible_credits":[]},"completion":{"operation_succeeded":true},"funding_dependencies":[]}],"aggregate_bounds":{"maximum_debits":[],"maximum_costs":[]}}"#
    }

    #[tokio::test]
    async fn progress_frames_are_observations_and_final_json_remains_exact() {
        use fin_domain::execution_progress::{self, Stage};
        let bytes = b"{\"progress\":\"preparing\"}\n{\"progress\":\"verifying\"}\n{\"ok\":true,\"result\":{\"status\":\"pending\"},\"error\":null}\n";
        let (tx, mut rx) = execution_progress::channel();
        let value = execution_progress::observe(tx, read_response(&bytes[..]))
            .await
            .unwrap();
        assert_eq!(value, json!({"status":"pending"}));
        assert_eq!(rx.recv().await, Some(Stage::Preparing));
        assert_eq!(rx.recv().await, Some(Stage::Verifying));
        assert_eq!(rx.recv().await, None);
        let invalid = b"{\"progress\":\"submitted_successfully\"}\n";
        assert!(read_response(&invalid[..]).await.is_err());
    }

    #[tokio::test]
    async fn human_configuration_actual_is_handed_off_before_any_broker_call_but_preview_is_read_only()
     {
        struct CaptureClient(std::sync::Mutex<Vec<BrokerRequest>>);
        #[async_trait]
        impl Client for CaptureClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                self.0.lock().unwrap().push(request);
                Ok(json!({"summary":"synthetic preview"}))
            }
            async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
                panic!("configuration must never use ordinary execute")
            }
        }
        let client = CaptureClient(Default::default());
        let actual = Cli::try_parse_from([
            "fin",
            "--json",
            "configure",
            "hyperliquid.account.set_agent_abstraction",
            "--input",
            r#"{"account":"hl-main","enabled":true}"#,
            "--id",
            "synthetic-config-1",
        ])
        .unwrap();
        let error = dispatch(actual, &client).await.unwrap_err();
        assert_eq!(error.code, "HUMAN_TERMINAL_REQUIRED");
        assert!(client.0.lock().unwrap().is_empty());

        let preview = Cli::try_parse_from([
            "fin",
            "--json",
            "configure",
            "hyperliquid.account.set_agent_abstraction",
            "--input",
            r#"{"account":"hl-main","enabled":true}"#,
            "--dry-run",
        ])
        .unwrap();
        assert!(!human_only(&preview.command));
        dispatch(preview, &client).await.unwrap();
        assert!(
            matches!(client.0.lock().unwrap().as_slice(), [BrokerRequest::ConfigurationPreview { operation, .. }] if operation == "hyperliquid.account.set_agent_abstraction")
        );
    }

    #[tokio::test]
    async fn plan_cli_routes_review_authority_and_step_candidates_without_child_grants() {
        struct PlanClient(std::sync::Mutex<Vec<BrokerRequest>>);
        #[async_trait]
        impl Client for PlanClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                self.0.lock().unwrap().push(request);
                Ok(json!({"ok":true}))
            }
            async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
                panic!("plan authority must not use standalone exact approval")
            }
        }
        let client = PlanClient(Default::default());
        let plan_input = sample_plan_json();
        let preview =
            Cli::try_parse_from(["fin", "plans", "preview", "--input", plan_input]).unwrap();
        dispatch(preview, &client).await.unwrap();
        assert!(
            matches!(client.0.lock().unwrap().last(), Some(BrokerRequest::PreviewPlan { request })
            if request.plan.name == "Synthetic transfer" && request.request_id == "preview")
        );

        let authorize = Cli::try_parse_from([
            "fin",
            "plans",
            "authorize",
            "--input",
            plan_input,
            "--id",
            "request-1",
            "--valid-for",
            "10m",
            "--review-hash",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])
        .unwrap();
        dispatch_with_review_hash(authorize, &client, Some("review-hash".into()))
            .await
            .unwrap();
        assert!(
            matches!(client.0.lock().unwrap().last(), Some(BrokerRequest::AuthorizePlan { request })
            if request.request_id == "request-1" && request.valid_for_seconds == Some(600)
                && request.expected_review_hash.as_deref()
                    == Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
        );

        let dry_run = Cli::try_parse_from([
            "fin",
            "execute",
            "wallet.transfer",
            "--input",
            "{}",
            "--plan",
            "plan-1",
            "--step",
            "transfer",
            "--dry-run",
        ])
        .unwrap();
        dispatch(dry_run, &client).await.unwrap();
        assert!(
            matches!(client.0.lock().unwrap().last(), Some(BrokerRequest::PreviewPlanStep { plan_id, step_id, .. })
            if plan_id == "plan-1" && step_id == "transfer")
        );

        let execute = Cli::try_parse_from([
            "fin",
            "execute",
            "wallet.transfer",
            "--input",
            "{}",
            "--id",
            "attempt-1",
            "--plan",
            "plan-1",
            "--step",
            "transfer",
        ])
        .unwrap();
        dispatch(execute, &client).await.unwrap();
        assert!(
            matches!(client.0.lock().unwrap().last(), Some(BrokerRequest::ExecutePlanStep { plan_id, step_id, request })
            if plan_id == "plan-1" && step_id == "transfer" && request.request_id == "attempt-1")
        );

        assert!(
            Cli::try_parse_from([
                "fin",
                "execute",
                "wallet.transfer",
                "--input",
                "{}",
                "--id",
                "attempt-1",
                "--plan",
                "plan-1",
                "--step",
                "sell",
                "--authorization",
                "boundary-1"
            ])
            .is_err()
        );
    }

    #[tokio::test]
    async fn changed_explicit_plan_review_hash_fails_before_authorization() {
        struct PreviewClient(std::sync::Mutex<Vec<BrokerRequest>>);
        #[async_trait]
        impl Client for PreviewClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                let BrokerRequest::PreviewPlan { request: preview } = &request else {
                    panic!("a human plan review may only prepare a read-only preview");
                };
                let steps = preview
                    .plan
                    .steps
                    .iter()
                    .map(|step| json!({"step_id":step.step_id}))
                    .collect::<Vec<_>>();
                let result = json!({
                    "review_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "plan":preview.plan,
                    "permissions":steps,
                    "valid_for_seconds":preview.valid_for_seconds.unwrap_or(600),
                });
                self.0.lock().unwrap().push(request);
                Ok(result)
            }
            async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
                panic!("plan preview must never submit an operation");
            }
        }

        let client = PreviewClient(Default::default());
        let input = sample_plan_json();
        let cli = Cli::try_parse_from([
            "fin",
            "plans",
            "authorize",
            "--input",
            input,
            "--id",
            "request-1",
            "--review-hash",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ])
        .unwrap();
        let error = review_plan_authorization(&cli, &client, &Ui::new(false, false))
            .await
            .unwrap_err();
        assert_eq!(error.code, "PLAN_REVIEW_CHANGED");
        assert_eq!(client.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn plan_review_request_identity_is_display_only() {
        let preview = json!({"plan_id":"existing-plan","review_hash":"review-hash"});
        let display = plan_review_for_display(&preview, "authorization-request-1");
        assert_eq!(display["plan_id"], "existing-plan");
        assert_eq!(
            display["authorization_request_id"],
            "authorization-request-1"
        );
        assert_eq!(display["review_hash"], preview["review_hash"]);
        assert!(preview.get("authorization_request_id").is_none());
    }

    #[tokio::test]
    async fn human_and_machine_plan_execution_skip_standalone_preview() {
        struct PlanClient(std::sync::Mutex<Vec<BrokerRequest>>);
        #[async_trait]
        impl Client for PlanClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                assert!(
                    matches!(
                        request,
                        BrokerRequest::ExecutePlanStep { .. }
                            | BrokerRequest::PreviewPlanStep { .. }
                    ),
                    "plan steps must not request standalone previews or approvals"
                );
                self.0.lock().unwrap().push(request);
                Ok(json!({"status":"completed", "summary":"Synthetic plan step result"}))
            }
            async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
                panic!("plan execution must retain its existing authority")
            }
        }
        for machine in [false, true] {
            for preview in [false, true] {
                let client = PlanClient(Default::default());
                // A relative deadline is deliberately absent from the input:
                // only the broker's plan-aware preparation may resolve it.
                let mut args = vec![
                    "fin",
                    "execute",
                    "uniswap.swap",
                    "--input",
                    "{}",
                    "--id",
                    "synthetic-attempt",
                    "--plan",
                    "synthetic-plan",
                    "--step",
                    "swap",
                ];
                if machine {
                    args.push("--json");
                }
                if preview {
                    args.push("--dry-run");
                }
                assert_eq!(
                    run_with_client(args.into_iter().map(Into::into).collect(), &client).await,
                    0
                );
                let calls = client.0.lock().unwrap();
                assert_eq!(calls.len(), 1);
                if preview {
                    assert!(
                        matches!(&calls[0], BrokerRequest::PreviewPlanStep { plan_id, step_id, input, .. }
                        if plan_id == "synthetic-plan" && step_id == "swap" && input.get("deadline").is_none())
                    );
                } else {
                    assert!(
                        matches!(&calls[0], BrokerRequest::ExecutePlanStep { plan_id, step_id, request }
                        if plan_id == "synthetic-plan" && step_id == "swap" && request.request_id == "synthetic-attempt"
                        && request.input.get("deadline").is_none())
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn terminal_execution_reviews_without_writing_and_machine_dry_runs_do_not() {
        struct ReviewClient {
            calls: std::sync::Mutex<Vec<Value>>,
            fail: bool,
        }
        #[async_trait]
        impl Client for ReviewClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                if matches!(request, BrokerRequest::Operations) {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(json!({"operation":"operations"}));
                    return Ok(json!([]));
                }
                let BrokerRequest::Preview { operation, input } = request else {
                    panic!("review must only look up existing requests and preview");
                };
                self.calls
                    .lock()
                    .unwrap()
                    .push(json!({"operation":operation,"input":input}));
                if self.fail {
                    Err(FinError::validation("Synthetic failed preview"))
                } else {
                    Ok(json!({"summary":"Send 1 SYNTHETIC to SYNTHETIC-wallet.","warnings":[]}))
                }
            }
            async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
                panic!("review must not execute or authorize");
            }
        }
        for extra in [None, Some("--json"), Some("--dry-run")] {
            let mut args = vec![
                "fin",
                "execute",
                "wallet.transfer",
                "--input",
                "{\"amount\":\"1\"}",
                "--id",
                "synthetic-id",
            ];
            if let Some(extra) = extra {
                args.push(extra);
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let client = ReviewClient {
                calls: Default::default(),
                fail: false,
            };
            review_human_execution(&cli, &client, &Ui::new(cli.json, false))
                .await
                .unwrap();
            assert_eq!(
                client.calls.lock().unwrap().len(),
                2 * usize::from(extra.is_none())
            );
        }
        let cli = Cli::try_parse_from([
            "fin",
            "execute",
            "wallet.transfer",
            "--input",
            "{}",
            "--id",
            "synthetic-id",
        ])
        .unwrap();
        let client = ReviewClient {
            calls: Default::default(),
            fail: true,
        };
        assert!(
            review_human_execution(&cli, &client, &Ui::new(false, false))
                .await
                .is_err()
        );
        let mut cli = Cli::try_parse_from([
            "fin",
            "execute",
            "wallet.transfer",
            "--input",
            "{}",
            "--id",
            "synthetic-id",
        ])
        .unwrap();
        if let Command::Execute(args) = &mut cli.command {
            args.id = None;
        }
        let client = ReviewClient {
            calls: Default::default(),
            fail: false,
        };
        assert!(
            review_human_execution(&cli, &client, &Ui::new(false, false))
                .await
                .is_err()
        );
        assert!(client.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn existing_request_skips_new_preview_and_retains_execution_recovery() {
        struct ExistingClient;
        #[async_trait]
        impl Client for ExistingClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                assert!(matches!(request, BrokerRequest::Operations));
                Ok(
                    json!([{"id":"synthetic-record","request_id":"synthetic-id","status":"unknown"}]),
                )
            }
            async fn execute(&self, request: OperationRequest) -> Result<Value, FinError> {
                assert_eq!(request.request_id, "synthetic-id");
                Ok(json!({"id":"synthetic-record","status":"unknown"}))
            }
        }
        let cli = Cli::try_parse_from([
            "fin",
            "execute",
            "wallet.transfer",
            "--input",
            "{}",
            "--id",
            "synthetic-id",
        ])
        .unwrap();
        review_human_execution(&cli, &ExistingClient, &Ui::new(false, false))
            .await
            .unwrap();
        assert_eq!(
            dispatch(cli, &ExistingClient).await.unwrap()["id"],
            "synthetic-record"
        );
    }

    struct PreviewOnlyClient {
        calls: std::sync::Mutex<Vec<Value>>,
        fail: bool,
    }
    #[async_trait]
    impl Client for PreviewOnlyClient {
        async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
            let BrokerRequest::Preview { operation, input } = request else {
                panic!("dry-run must only send Preview, never recovery/status/write requests")
            };
            self.calls
                .lock()
                .unwrap()
                .push(json!({"operation":operation,"input":input}));
            if self.fail {
                Err(FinError::new(
                    "BROKER_TIMEOUT",
                    "transport",
                    "Synthetic preview timeout.",
                    "Retry the preview.",
                ))
            } else {
                Ok(json!({"checks":[{"status":"unchecked"}],"synthetic":"preview"}))
            }
        }
        async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
            panic!("dry-run must never enter execution or financial approval")
        }
    }

    #[tokio::test]
    async fn execute_dry_run_is_preview_with_or_without_execution_flags() {
        for extras in [vec![], vec!["--id", "synthetic-write-id"]] {
            for machine in [false, true] {
                let client = PreviewOnlyClient {
                    calls: Default::default(),
                    fail: false,
                };
                let mut arguments = vec![
                    "fin",
                    "execute",
                    "binance.transfer",
                    "--input",
                    "{\"amount\":\"1.000000000000000001\"}",
                    "--dry-run",
                ];
                arguments.extend(extras.clone());
                if machine {
                    arguments.push("--json");
                }
                let cli = Cli::try_parse_from(arguments).unwrap();
                assert_eq!(command_name(&cli.command), "preview");
                let result = dispatch(cli, &client).await.unwrap();
                let alias = Cli::try_parse_from([
                    "fin",
                    "preview",
                    "binance.transfer",
                    "--input",
                    "{\"amount\":\"1.000000000000000001\"}",
                ])
                .unwrap();
                assert_eq!(result, dispatch(alias, &client).await.unwrap());
                let calls = client.calls.lock().unwrap();
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0], calls[1]);
                assert_eq!(calls[0]["input"]["amount"], "1.000000000000000001");
            }
        }
        assert!(
            Cli::try_parse_from(["fin", "execute", "binance.transfer", "--input", "{}"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "fin",
                "execute",
                "binance.transfer",
                "--input",
                "{}",
                "--id",
                "synthetic-write-id"
            ])
            .is_ok()
        );
    }

    #[tokio::test]
    async fn execute_dry_run_errors_never_enter_write_recovery_or_echo_input() {
        let client = PreviewOnlyClient {
            calls: Default::default(),
            fail: true,
        };
        let cli = Cli::try_parse_from([
            "fin",
            "execute",
            "binance.transfer",
            "--input",
            "{}",
            "--dry-run",
            "--id",
            "synthetic-id",
        ])
        .unwrap();
        let error = dispatch(cli, &client).await.unwrap_err();
        assert_eq!(error.code, "BROKER_TIMEOUT");
        assert_eq!(client.calls.lock().unwrap().len(), 1);
        let cli = Cli::try_parse_from([
            "fin",
            "execute",
            "binance.transfer",
            "--input",
            "synthetic-secret-invalid-json",
            "--dry-run",
        ])
        .unwrap();
        let error = dispatch(cli, &client).await.unwrap_err();
        assert!(
            !serde_json::to_string(&error)
                .unwrap()
                .contains("synthetic-secret")
        );
        assert_eq!(client.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn removing_dry_run_retains_normal_execution_identity_without_run() {
        struct ExecutionClient(std::sync::Mutex<Vec<Value>>);
        #[async_trait]
        impl Client for ExecutionClient {
            async fn call(&self, _: BrokerRequest) -> Result<Value, FinError> {
                panic!("normal execution must use the existing execution/recovery path")
            }
            async fn execute(&self, request: OperationRequest) -> Result<Value, FinError> {
                self.0
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(request).unwrap());
                Ok(json!({"synthetic":"normal-execution"}))
            }
        }
        let client = ExecutionClient(Default::default());
        let cli = Cli::try_parse_from([
            "fin",
            "execute",
            "binance.transfer",
            "--input",
            "{\"requestId\":\"synthetic-venue-id\"}",
            "--id",
            "synthetic-logical-id",
        ])
        .unwrap();
        assert_eq!(command_name(&cli.command), "execute");
        dispatch(cli, &client).await.unwrap();
        let calls = client.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["request_id"], "synthetic-logical-id");
        assert!(calls[0]["run_id"].is_null());
        assert_eq!(calls[0]["input"]["requestId"], "synthetic-venue-id");
    }

    #[tokio::test]
    async fn offline_discovery_is_available_without_a_broker() {
        let caps = capabilities();
        assert_eq!(caps["service_required"], false);
        assert!(!caps["commands"].as_array().unwrap().is_empty());
        assert!(find_definition("setup").is_some());
        assert!(schema("connections.add").is_some());
    }

    #[tokio::test]
    async fn dry_run_calls_only_preview_and_preserves_normal_execute() {
        struct PreviewOnly;
        #[async_trait]
        impl Client for PreviewOnly {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                match request {
                    BrokerRequest::Preview { operation, input } => {
                        assert_eq!(operation, "fixture.write");
                        assert_eq!(input, json!({"amount":"1"}));
                        Ok(json!({"preview":true}))
                    }
                    _ => panic!("dry-run must only preview"),
                }
            }
            async fn execute(&self, request: OperationRequest) -> Result<Value, FinError> {
                assert_eq!(request.request_id, "synthetic-1");
                Ok(json!({"execute":true}))
            }
        }
        let args = [
            "fin",
            "execute",
            "fixture.write",
            "--input",
            r#"{"amount":"1"}"#,
            "--id",
            "synthetic-1",
            "--json",
        ];
        let dry = Cli::try_parse_from(args.into_iter().chain(["--dry-run"])).unwrap();
        assert_eq!(
            dispatch(dry, &PreviewOnly).await.unwrap(),
            json!({"preview":true})
        );
        let normal = Cli::try_parse_from(args).unwrap();
        assert_eq!(
            dispatch(normal, &PreviewOnly).await.unwrap(),
            json!({"execute":true})
        );
    }

    #[test]
    fn skill_definitions_examples_flags_and_embedded_metadata_agree() {
        use fin_discovery::companion_skill::{CONTENT, SkillChange, SkillTarget, VERSION};
        assert!(CONTENT.contains(&format!("version: \"{VERSION}\"")));
        for definition in fin_discovery::companion_skill::definitions() {
            for example in &definition.examples {
                if definition.name == "skill.inspect" {
                    serde_json::from_value::<SkillTarget>(example.clone()).unwrap();
                } else {
                    serde_json::from_value::<SkillChange>(example.clone()).unwrap();
                }
            }
            for example in definition.semantics["command_line_examples"]
                .as_array()
                .unwrap()
            {
                let parsed =
                    Cli::try_parse_from(example.as_str().unwrap().split_whitespace()).unwrap();
                assert!(!human_only(&parsed.command));
                assert_eq!(command_name(&parsed.command), "skill");
            }
            assert_eq!(
                definition.output_schema,
                fin_discovery::schema::<fin_discovery::companion_skill::SkillResult>()
            );
            assert_eq!(
                schema(&definition.name).unwrap()["input"],
                definition.input_schema
            );
        }
        assert!(
            Cli::try_parse_from(["fin", "skill", "inspect", "--agent", "codex", "--backup"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["fin", "skill", "install", "--agent", "unreviewed"]).is_err());
        assert!(Cli::try_parse_from(["fin", "skill", "install"]).is_err());
    }

    #[tokio::test]
    async fn scoped_snapshot_uses_public_read_contract_and_keeps_partial_json() {
        struct SnapshotClient(std::sync::Mutex<Vec<BrokerRequest>>);
        #[async_trait]
        impl Client for SnapshotClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                self.0.lock().unwrap().push(request);
                Ok(
                    json!({"state":{"holdings":[],"positions":[],"authorities":[],"in_flight":[]},"observed_at":"2026-10-11T00:00:00Z","complete":false,"coverage":["ethereum:8453"],"pending":["observer:ethereum:8453:RPC_READ_TIMEOUT"]}),
                )
            }
            async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
                panic!("snapshot cannot execute")
            }
        }
        let client = SnapshotClient(Default::default());
        let cli = Cli::try_parse_from([
            "fin",
            "snapshot",
            "--input",
            "{\"networks\":[\"ethereum:8453\"]}",
            "--json",
            "--verbose",
        ])
        .unwrap();
        let result = dispatch(cli, &client).await.unwrap();
        assert_eq!(result["complete"], false);
        assert_eq!(
            result["pending"][0],
            "observer:ethereum:8453:RPC_READ_TIMEOUT"
        );
        assert!(
            matches!(&client.0.lock().unwrap()[0], BrokerRequest::Read { operation, input } if operation == "fin.snapshot" && input["networks"][0] == "ethereum:8453")
        );
        let bad =
            Cli::try_parse_from(["fin", "snapshot", "--input", "{\"unknown\":true}", "--json"])
                .unwrap();
        assert_eq!(
            dispatch(bad, &client).await.unwrap_err().code,
            "INVALID_INPUT"
        );
        assert_eq!(client.0.lock().unwrap().len(), 1);
        let full = Cli::try_parse_from(["fin", "snapshot", "--json"]).unwrap();
        dispatch(full, &client).await.unwrap();
        assert!(matches!(
            client.0.lock().unwrap()[1],
            BrokerRequest::Snapshot
        ));
    }

    #[test]
    fn malformed_user_json_never_appears_in_error() {
        let secret = "synthetic-private-key-should-not-echo";
        let error = parse_input(secret).unwrap_err();
        let rendered = serde_json::to_string(&error).unwrap();
        assert!(!rendered.contains(secret));
    }

    #[test]
    fn execution_recovery_matches_logical_request_identity() {
        let records = json!([{"id":"record-123","request_id":"logical-abc"}]);
        assert_eq!(
            find_original_operation(&records, "logical-abc").as_deref(),
            Some("record-123")
        );
        assert_eq!(find_original_operation(&records, "other"), None);
    }

    #[tokio::test]
    async fn plan_response_loss_recovers_the_original_operation_shape_without_resubmission() {
        struct LostPlanResponse(std::sync::Mutex<Vec<BrokerRequest>>);
        #[async_trait]
        impl Transport for LostPlanResponse {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                let mut requests = self.0.lock().unwrap();
                match &request {
                    BrokerRequest::ExecutePlanStep { .. }
                    | BrokerRequest::ReplacePlanCandidate { .. } => {
                        requests.push(request);
                        Err(FinError::new(
                            "CONNECTION_RESET",
                            "transport",
                            "Synthetic response loss.",
                            "Inspect the same request identity.",
                        ))
                    }
                    BrokerRequest::PlanStatus { plan_id } => {
                        requests.push(request.clone());
                        assert_eq!(plan_id, "plan-1");
                        Ok(
                            json!({"attempts":[{"attempt_id":"operation-1","request_id":"attempt-1","step_id":"sell"}]}),
                        )
                    }
                    BrokerRequest::Status { id } => {
                        requests.push(request.clone());
                        assert_eq!(id, "operation-1");
                        Ok(json!({"id":id,"request_id":"attempt-1","status":"completed"}))
                    }
                    _ => {
                        panic!("plan response recovery must only inspect plan and operation status")
                    }
                }
            }
        }

        for replace in [false, true] {
            let transport = LostPlanResponse(Default::default());
            let request = OperationRequest {
                request_id: "attempt-1".into(),
                operation: "binance.spot.order-place".into(),
                input: json!({"request_id":"attempt-1"}),
                run_id: None,
            };
            let recovered = if replace {
                replace_plan_candidate_with_recovery(
                    &transport,
                    "plan-1".into(),
                    "sell".into(),
                    "old-candidate".into(),
                    request,
                )
                .await
            } else {
                execute_plan_step_with_recovery(&transport, "plan-1".into(), "sell".into(), request)
                    .await
            }
            .unwrap();
            assert_eq!(recovered["id"], "operation-1");
            assert_eq!(recovered["request_id"], "attempt-1");
            assert_eq!(recovered["status"], "completed");
            let calls = transport.0.lock().unwrap();
            assert_eq!(calls.len(), 3);
            assert!(matches!(calls[1], BrokerRequest::PlanStatus { .. }));
            assert!(matches!(calls[2], BrokerRequest::Status { .. }));
        }
    }

    #[test]
    fn paced_readonly_preview_has_a_bounded_preparation_wait() {
        assert_eq!(
            broker_response_timeout(&BrokerRequest::Preview {
                operation: "binance.spot.order-place".into(),
                input: json!({})
            }),
            FINANCIAL_WRITE_TIMEOUT
        );
        assert_eq!(
            broker_response_timeout(&BrokerRequest::Doctor),
            BROKER_TIMEOUT
        );
        for operation in [
            "bridge.quotes",
            "bridge.status",
            "wallet.balances",
            "wallet.token",
            "uniswap.quote",
            "ramses.quote",
        ] {
            assert_eq!(
                broker_response_timeout(&BrokerRequest::Read {
                    operation: operation.into(),
                    input: json!({"wallet":"synthetic","request_id":"read-only"}),
                }),
                OBSERVER_TIMEOUT,
                "financial read {operation} uses the observer timeout without write authority"
            );
        }
    }

    #[test]
    fn boundary_grant_waits_for_human_approval_and_preserves_retry_identity() {
        let request = BrokerRequest::AuthorizeBoundary {
            request: fin_domain::boundaries::BoundaryAuthorizationRequest {
                request_id: "synthetic-grant-timeout".into(),
                operation: "binance.spot.order-place".into(),
                input: json!({}),
                bounds: json!({}),
                valid_for_seconds: 600,
                replaces: None,
            },
        };
        assert_eq!(broker_response_timeout(&request), FINANCIAL_WRITE_TIMEOUT);
        assert!(broker_response_timeout(&request) > Duration::from_secs(90));
        let error = broker_timeout(&request);
        assert!(error.next_action.contains("same request ID and parameters"));
        assert!(error.next_action.contains("do not create another grant"));
        assert!(!error.next_action.contains("daemon status"));
        assert!(!error.next_action.contains("synthetic-grant-timeout"));
    }

    #[test]
    fn timeout_guidance_distinguishes_service_requests_from_uncertain_writes() {
        for request in [
            BrokerRequest::Connections,
            BrokerRequest::Doctor,
            BrokerRequest::DaemonStart,
            BrokerRequest::DaemonStop,
        ] {
            let error = broker_timeout(&request);
            assert_eq!(error.code, "BROKER_TIMEOUT");
            assert!(error.next_action.contains("fin daemon status --json"));
            assert!(!error.next_action.contains("request ID"));
        }
        let error = broker_timeout(&BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "synthetic-timeout".into(),
                operation: "wallet.transfer".into(),
                input: json!({}),
                run_id: None,
            },
        });
        assert!(error.next_action.contains("same request ID"));
        assert!(
            error
                .next_action
                .contains("do not create another financial request")
        );
        assert!(!error.next_action.contains("synthetic-timeout"));

        let error = broker_timeout(&BrokerRequest::Status {
            id: "synthetic-operation".into(),
        });
        assert_eq!(
            broker_response_timeout(&BrokerRequest::Status {
                id: "synthetic-operation".into(),
            }),
            OBSERVER_TIMEOUT
        );
        assert!(error.next_action.contains("same operation"));
        assert!(
            error
                .next_action
                .contains("do not submit another financial request")
        );
        assert!(!error.next_action.contains("daemon status"));
    }

    #[test]
    fn enrollment_dtos_reject_secret_fields_and_unknown_flags() {
        let args: fin_discovery::AddConnectionArgs = serde_json::from_value(json!({
            "provider":"binance","name":"main","role":"main","credential_type":"hmac"
        }))
        .unwrap();
        let mut value = serde_json::to_value(args).unwrap();
        value["api_key"] = json!("SYNTHETIC_SECRET");
        assert!(serde_json::from_value::<fin_discovery::AddConnectionArgs>(value).is_err());
        let parse = Cli::try_parse_from([
            "fin",
            "connections",
            "add",
            "--provider",
            "binance",
            "--name",
            "main",
            "--role",
            "main",
            "--api-key",
            "SYNTHETIC_SECRET",
        ]);
        assert!(parse.is_err());
        assert!(Cli::try_parse_from(["fin", "connections", "remove", "--name", "main"]).is_ok());
    }

    #[test]
    fn every_visible_command_and_subcommand_has_a_help_description() {
        fn check(command: &clap::Command) {
            for child in command
                .get_subcommands()
                .filter(|child| !child.is_hide_set())
            {
                assert!(
                    child
                        .get_about()
                        .is_some_and(|about| !about.to_string().trim().is_empty()),
                    "missing description: {}",
                    child.get_name()
                );
                check(child);
            }
        }
        let mut command = Cli::command();
        command.build();
        check(&command);
    }

    #[test]
    fn generated_help_and_catalog_do_not_need_broker() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("capabilities"));
        assert!(help.contains("connections"));
        assert_eq!(
            financial_catalog().len(),
            fin_broker::operation_definitions().len()
        );
        for name in [
            "aave.markets",
            "wallet.permit2.allowance",
            "wallet.permit2.approve",
            "ramses.quote",
            "ramses.swap",
            "hyperliquid.orders.place",
            "hyperliquid.account.withdraw",
            "sushi.quote",
            "sushi.swap",
            "pancakeswap.quote",
            "pancakeswap.swap",
        ] {
            assert!(
                financial_catalog()
                    .iter()
                    .any(|definition| definition["name"] == name)
            );
        }

        assert!(financial_catalog().iter().all(|v| v.get("name").is_some()));
    }

    #[test]
    fn bundled_financial_catalog_matches_compiled_broker_registry() {
        let compiled = serde_json::to_value(fin_broker::operation_definitions()).unwrap();
        let bundled = serde_json::to_value(financial_catalog()).unwrap();
        let expected = compiled.as_array().unwrap();
        let actual = bundled.as_array().unwrap();
        let mismatches = actual
            .iter()
            .zip(expected)
            .filter(|(actual, expected)| actual != expected)
            .map(|(actual, _)| {
                actual
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual.len(),
            expected.len(),
            "catalog definition count changed"
        );
        assert!(
            mismatches.is_empty(),
            "bundled catalog differs from broker definitions: {mismatches:?}"
        );
    }

    #[tokio::test]
    async fn oversized_broker_response_is_rejected() {
        let mut bytes = vec![b'x'; MAX_RESPONSE_FRAME_BYTES + 2];
        bytes.push(b'\n');
        let error = read_response(&bytes[..]).await.unwrap_err();
        assert_eq!(error.code, "BROKER_PROTOCOL_ERROR");
    }

    #[tokio::test]
    async fn full_catalog_response_over_one_mib_is_read_without_truncation() {
        let value = json!({"markets":"x".repeat(MAX_FRAME_BYTES + 1)});
        let encoded =
            fin_protocol::encode_response(&BrokerResponse::success(value.clone())).unwrap();
        assert_eq!(read_response(encoded.as_slice()).await.unwrap(), value);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn rpc_rejects_a_peer_that_is_not_the_expected_uid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });
        let transport = UnixTransport {
            path,
            expected_uid: u32::MAX,
        };
        let error = Transport::call(&transport, BrokerRequest::Capabilities)
            .await
            .unwrap_err();
        assert_eq!(error.code, "BROKER_PROTOCOL_ERROR");
        server.await.unwrap();
    }

    #[test]
    fn enrollment_fails_closed_before_privileged_action_without_terminal() {
        let error =
            launch_human_privileged_with_terminal(PrivilegedAction::Setup, false, false, false)
                .unwrap_err();
        assert_eq!(error.code, "HUMAN_TERMINAL_REQUIRED");
    }

    #[test]
    fn policy_machine_mode_requires_human_even_with_a_terminal() {
        for tty in [true, false] {
            let action = PrivilegedAction::PolicySet(fin_discovery::PolicySetArgs {
                max_movement_usd: "1000".into(),
                max_run_allocation_usd: "5000".into(),
                allow_unpriced_exchange_assets: None,
            });
            let result = launch_human_privileged_with_terminal(action, tty, true, true);
            assert_eq!(result.unwrap_err().code, "HUMAN_TERMINAL_REQUIRED");
        }
    }

    struct NoticeWriter<'a> {
        bytes: Vec<u8>,
        flushes: &'a std::cell::Cell<usize>,
        fail_write: bool,
        fail_flush: bool,
    }

    impl std::io::Write for NoticeWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                return Err(std::io::Error::other("synthetic notice write failure"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes.set(self.flushes.get() + 1);
            if self.fail_flush {
                return Err(std::io::Error::other("synthetic notice flush failure"));
            }
            Ok(())
        }
    }

    #[test]
    fn administrator_notice_is_flushed_before_exec_for_every_privileged_action() {
        let args = || {
            serde_json::from_value(json!({
            "provider":"binance", "name":"synthetic-main", "role":"main", "credential_type":"hmac"
        })).unwrap()
        };
        let remove = serde_json::from_value(json!({"name":"synthetic-main"})).unwrap();
        for (action, expected_reason) in [
            (
                PrivilegedAction::Setup,
                "Fin needs administrator access to install its protected service and keep your secrets safe from the LLMs",
            ),
            (PrivilegedAction::Add(args()), "add this connection"),
            (PrivilegedAction::Update(args()), "update this connection"),
            (PrivilegedAction::Remove(remove), "remove this connection"),
        ] {
            let flushes = std::cell::Cell::new(0);
            let mut output = NoticeWriter {
                bytes: Vec::new(),
                flushes: &flushes,
                fail_write: false,
                fail_flush: false,
            };
            let reason = administrator_authentication_reason(&action);
            let error = exec_after_administrator_notice(reason, &mut output, || {
                assert_eq!(
                    flushes.get(),
                    1,
                    "explanation must be flushed before execution"
                );
                std::io::Error::other("synthetic exec failure")
            })
            .unwrap_err();
            assert_eq!(error.code, "PRIVILEGED_EXEC_FAILED");
            let notice = String::from_utf8(output.bytes).unwrap();
            assert!(notice.contains(expected_reason));
            assert!(notice.contains("Touch ID follows"));
            assert!(notice.contains(fin_discovery::MACOS_ADMIN_PASSWORD_HELP));
            assert!(!notice.contains("synthetic-main"));
        }
    }

    #[test]
    fn administrator_notice_output_failure_prevents_exec() {
        for (fail_write, fail_flush) in [(true, false), (false, true)] {
            let flushes = std::cell::Cell::new(0);
            let executed = std::cell::Cell::new(false);
            let mut output = NoticeWriter {
                bytes: Vec::new(),
                flushes: &flushes,
                fail_write,
                fail_flush,
            };
            let error = exec_after_administrator_notice(
                administrator_authentication_reason(&PrivilegedAction::Setup),
                &mut output,
                || {
                    executed.set(true);
                    std::io::Error::other("synthetic exec failure")
                },
            )
            .unwrap_err();
            assert_eq!(error.code, "ADMINISTRATOR_NOTICE_UNAVAILABLE");
            assert!(!executed.get());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn privileged_commands_keep_fixed_sudo_prompt_executable_and_public_metadata() {
        let args = || {
            serde_json::from_value(json!({
            "provider":"binance", "name":"synthetic-main", "role":"main", "credential_type":"hmac"
        })).unwrap()
        };
        let remove = serde_json::from_value(json!({"name":"synthetic-main"})).unwrap();
        for (action, verb) in [
            (PrivilegedAction::Add(args()), "add"),
            (PrivilegedAction::Update(args()), "update"),
            (PrivilegedAction::Remove(remove), "remove"),
        ] {
            let command =
                prepare_privileged_command_with_presentation(action, false, false, false).unwrap();
            assert_eq!(command.get_program(), "/usr/bin/sudo");
            let arguments = command
                .get_args()
                .map(|v| v.to_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                &arguments[..8],
                &[
                    "-k",
                    "-p",
                    fin_discovery::MACOS_ADMIN_PASSWORD_PROMPT,
                    "--",
                    BROKER_PATH,
                    "enrollment",
                    verb,
                    "--request"
                ]
            );
            assert_eq!(arguments.len(), 9);
            let request: Value = serde_json::from_str(arguments[8]).unwrap();
            assert_eq!(request["name"], "synthetic-main");
            let environment = command
                .get_envs()
                .map(|(name, value)| (name.to_str().unwrap(), value.unwrap().to_str().unwrap()))
                .collect::<Vec<_>>();
            assert_eq!(environment, vec![("PATH", "/usr/bin:/bin")]);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn presentation_preferences_cross_sudo_as_fixed_values_only() {
        let args = fin_discovery::NamedConnectionArgs {
            name: "synthetic-main".into(),
        };
        let command = prepare_privileged_command_with_presentation(
            PrivilegedAction::Remove(args),
            true,
            true,
            true,
        )
        .unwrap();
        let arguments: Vec<_> = command.get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(&arguments[4..7], &["NO_COLOR=1", "TERM=dumb", BROKER_PATH]);
        assert_eq!(arguments.last(), Some(&"--verbose"));
    }

    #[test]
    fn machine_mode_refuses_privilege_even_with_terminal() {
        let error =
            launch_human_privileged_with_terminal(PrivilegedAction::Setup, true, true, true)
                .unwrap_err();
        assert_eq!(error.code, "HUMAN_TERMINAL_REQUIRED");
    }

    #[test]
    fn add_accepts_positional_or_flag_provider_without_discarding_metadata() {
        for arguments in [
            vec!["fin", "connections", "add"],
            vec!["fin", "connections", "add", "binance"],
            vec!["fin", "connections", "add", "--provider", "binance"],
        ] {
            let parsed = Cli::try_parse_from(arguments).unwrap();
            let Command::Connections {
                command: Some(ConnectionsCommand::Add(args)),
            } = parsed.command
            else {
                panic!("add command");
            };
            assert!(args.connection().is_none());
        }
        for prefix in [
            vec!["fin", "connections", "add", "binance"],
            vec!["fin", "connections", "add", "--provider", "binance"],
        ] {
            let mut arguments = prefix;
            arguments.extend([
                "--name",
                "master",
                "--role",
                "main",
                "--credential-type",
                "ed25519",
            ]);
            let parsed = Cli::try_parse_from(arguments).unwrap();
            let Command::Connections {
                command: Some(ConnectionsCommand::Add(args)),
            } = parsed.command
            else {
                panic!("add command");
            };
            let args = args.connection().unwrap();
            assert_eq!(args.provider, fin_discovery::Provider::Binance);
            assert_eq!(args.name, "master");
            fin_discovery::validate_add(&args).unwrap();
        }
        for arguments in [
            vec!["fin", "connections", "add", "--role", "main"],
            vec!["fin", "connections", "add", "binance", "--role", "main"],
            vec![
                "fin",
                "connections",
                "add",
                "binance",
                "--provider",
                "ethereum",
            ],
            vec!["fin", "connections", "add", "unknown"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn output_mode_does_not_read_flag_values_as_options() {
        for args in [
            vec!["fin", "read", "x", "--input", "--json"],
            vec!["fin", "--", "--json"],
        ] {
            assert!(!requested_json(
                &args.into_iter().map(Into::into).collect::<Vec<_>>()
            ));
        }
        assert!(requested_json(
            &["fin", "--verbose", "--json", "bad"].map(Into::into)
        ));
    }

    #[test]
    fn installed_cli_does_not_expose_development_mode_flags() {
        for mode in ["synthetic", "live"] {
            assert!(Cli::try_parse_from(["fin", "--mode", mode, "capabilities"]).is_err());
            assert!(Cli::try_parse_from(["fin", "capabilities", "--mode", mode]).is_err());
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_refuses_every_privileged_action_even_with_a_terminal() {
        let args: fin_discovery::AddConnectionArgs = serde_json::from_value(json!({
            "provider":"binance","name":"synthetic-main","role":"main","credential_type":"hmac"
        }))
        .unwrap();
        let update: fin_discovery::AddConnectionArgs =
            serde_json::from_value(serde_json::to_value(&args).unwrap()).unwrap();
        let remove: fin_discovery::NamedConnectionArgs =
            serde_json::from_value(json!({"name":"synthetic-main"})).unwrap();
        for action in [
            PrivilegedAction::Setup,
            PrivilegedAction::PolicySet(fin_discovery::PolicySetArgs {
                max_movement_usd: "100".into(),
                max_run_allocation_usd: "100".into(),
                allow_unpriced_exchange_assets: None,
            }),
            PrivilegedAction::Add(args),
            PrivilegedAction::Update(update),
            PrivilegedAction::Remove(remove),
            PrivilegedAction::Configure(OperationRequest {
                operation: "synthetic.configuration".into(),
                input: json!({"account":"synthetic-main"}),
                request_id: "synthetic-configuration-1".into(),
                run_id: None,
            }),
        ] {
            let error =
                launch_human_privileged_with_terminal(action, true, false, false).unwrap_err();
            assert_eq!(error.code, "PROTECTED_PROVIDER_UNAVAILABLE");
            assert_eq!(error.stage, "enrollment");
            assert!(
                !serde_json::to_string(&error)
                    .unwrap()
                    .contains("synthetic-main")
            );
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn unsupported_platform_refuses_broker_transport_for_reads_and_writes() {
        let transport = UnixTransport::installed();
        for request in [
            BrokerRequest::Capabilities,
            BrokerRequest::Execute {
                request: OperationRequest {
                    request_id: "synthetic-request".into(),
                    operation: "wallet.transfer".into(),
                    input: json!({}),
                    run_id: None,
                },
            },
        ] {
            let error = Transport::call(&transport, request).await.unwrap_err();
            assert_eq!(error.code, "PROTECTED_PROVIDER_UNAVAILABLE");
            assert_eq!(error.stage, "transport");
        }
    }

    #[tokio::test]
    async fn boundary_commands_route_grants_candidate_previews_and_attempts() {
        struct BoundaryClient(std::sync::Mutex<Vec<String>>);
        #[async_trait]
        impl Client for BoundaryClient {
            async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
                let value = match request {
                    BrokerRequest::AuthorizeBoundary { request } => {
                        assert_eq!(request.request_id, "synthetic-intent");
                        assert_eq!(request.valid_for_seconds, 600);
                        self.0.lock().unwrap().push("authorize".into());
                        json!({"id":"synthetic-authorization"})
                    }
                    BrokerRequest::BoundaryStatus { id } => {
                        assert_eq!(id, "synthetic-authorization");
                        self.0.lock().unwrap().push("status".into());
                        json!({"authorization":{"id":id,"status":"active"},"attempts":[]})
                    }
                    BrokerRequest::RevokeBoundary { id } => {
                        assert_eq!(id, "synthetic-authorization");
                        self.0.lock().unwrap().push("cancel".into());
                        json!({"id":id,"status":"revoked"})
                    }
                    BrokerRequest::PreviewBoundary {
                        id,
                        operation,
                        input,
                    } => {
                        assert_eq!(id, "synthetic-authorization");
                        assert_eq!(operation, "binance.spot-order");
                        assert_eq!(input["side"], "SELL");
                        self.0.lock().unwrap().push("preview".into());
                        json!({"authorization_id":id,"input":input})
                    }
                    BrokerRequest::ExecuteBoundary {
                        authorization_id,
                        request,
                    } => {
                        assert_eq!(authorization_id, "synthetic-authorization");
                        assert_eq!(request.request_id, "synthetic-attempt");
                        self.0.lock().unwrap().push("execute".into());
                        json!({"id":"synthetic-operation"})
                    }
                    BrokerRequest::ReplaceBoundaryCandidate {
                        authorization_id,
                        previous_attempt_id,
                        request,
                    } => {
                        assert_eq!(authorization_id, "synthetic-authorization");
                        assert_eq!(previous_attempt_id, "synthetic-old-candidate");
                        assert_eq!(request.request_id, "synthetic-attempt-replacement");
                        self.0.lock().unwrap().push("replace".into());
                        json!({"id":"synthetic-replacement-operation"})
                    }
                    _ => panic!("unexpected boundary CLI request"),
                };
                Ok(value)
            }
            async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
                panic!("boundary command used legacy execution")
            }
        }

        assert_eq!(parse_duration_seconds("10m").unwrap(), 600);
        assert_eq!(parse_duration_seconds("2h").unwrap(), 7200);
        assert!(parse_duration_seconds("0m").is_err());
        let client = BoundaryClient(Default::default());

        let grant = Cli::try_parse_from([
            "fin",
            "authorizations",
            "authorize",
            "binance.spot-order",
            "--id",
            "synthetic-intent",
            "--input",
            r#"{"side":"SELL"}"#,
            "--bounds",
            r#"{"max_quantity":"1"}"#,
            "--json",
        ])
        .unwrap();
        assert_eq!(
            dispatch(grant, &client).await.unwrap()["id"],
            "synthetic-authorization"
        );

        for (command, expected) in [("status", "status"), ("cancel", "cancel")] {
            let cli = Cli::try_parse_from([
                "fin",
                "authorizations",
                command,
                "synthetic-authorization",
                "--json",
            ])
            .unwrap();
            dispatch(cli, &client).await.unwrap();
            assert_eq!(client.0.lock().unwrap().last().unwrap(), expected);
        }

        for (id_args, expected) in [
            (vec!["--dry-run"], "preview"),
            (vec!["--id", "synthetic-attempt"], "execute"),
            (
                vec![
                    "--id",
                    "synthetic-attempt-replacement",
                    "--replaces-attempt",
                    "synthetic-old-candidate",
                ],
                "replace",
            ),
        ] {
            let mut args = vec![
                "fin",
                "execute",
                "binance.spot-order",
                "--input",
                r#"{"side":"SELL"}"#,
                "--authorization",
                "synthetic-authorization",
            ];
            args.extend(id_args);
            args.push("--json");
            let cli = Cli::try_parse_from(args).unwrap();
            dispatch(cli, &client).await.unwrap();
            assert_eq!(client.0.lock().unwrap().last().unwrap(), expected);
        }
        assert_eq!(
            *client.0.lock().unwrap(),
            vec![
                "authorize",
                "status",
                "cancel",
                "preview",
                "execute",
                "replace"
            ]
        );
        assert!(
            Cli::try_parse_from([
                "fin",
                "execute",
                "binance.spot-order",
                "--input",
                "{}",
                "--id",
                "synthetic-attempt",
                "--replaces-attempt",
                "synthetic-old-candidate",
            ])
            .is_err()
        );
    }

    struct BoundaryEngineClient(std::sync::Arc<fin_engine::Engine>);

    #[async_trait]
    impl Client for BoundaryEngineClient {
        async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
            // Exercise the same typed JSON envelope crossing the Unix socket.
            let bytes = serde_json::to_vec(&request).unwrap();
            let request: BrokerRequest = serde_json::from_slice(&bytes).unwrap();
            match request {
                BrokerRequest::PreviewBoundaryAuthorization { request } => {
                    self.0.preview_boundary_authorization(request).await
                }
                BrokerRequest::AuthorizeBoundary { request } => {
                    serde_json::to_value(self.0.authorize_boundary(request).await?)
                        .map_err(|_| generic_input_error())
                }
                BrokerRequest::BoundaryStatus { id } => {
                    serde_json::to_value(self.0.boundary_status(&id).await?)
                        .map_err(|_| generic_input_error())
                }
                BrokerRequest::RevokeBoundary { id } => {
                    serde_json::to_value(self.0.revoke_boundary(&id).await?)
                        .map_err(|_| generic_input_error())
                }
                BrokerRequest::PreviewBoundary {
                    id,
                    operation,
                    input,
                } => serde_json::to_value(self.0.preview_boundary(&id, &operation, &input).await?)
                    .map_err(|_| generic_input_error()),
                BrokerRequest::ExecuteBoundary {
                    authorization_id,
                    request,
                } => {
                    serde_json::to_value(self.0.execute_boundary(&authorization_id, request).await?)
                        .map_err(|_| generic_input_error())
                }
                BrokerRequest::ReplaceBoundaryCandidate {
                    authorization_id,
                    previous_attempt_id,
                    request,
                } => serde_json::to_value(
                    self.0
                        .execute_boundary_replacing(
                            &authorization_id,
                            request,
                            &previous_attempt_id,
                        )
                        .await?,
                )
                .map_err(|_| generic_input_error()),
                _ => panic!("unexpected request in synthetic boundary test"),
            }
        }

        async fn execute(&self, _: OperationRequest) -> Result<Value, FinError> {
            panic!("boundary test must not enter legacy exact approval")
        }
    }

    #[derive(Default)]
    struct SyntheticBoundaryAdapter {
        submissions: std::sync::atomic::AtomicUsize,
    }

    fn synthetic_boundary_definition() -> fin_domain::Definition {
        fin_domain::Definition {
            name: "synthetic.trade".into(),
            description: "Synthetic boundary test operation".into(),
            kind: fin_domain::OperationKind::Write,
            execution_actor: fin_domain::ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic fixture".into(),
            completion: "verified synthetic receipt".into(),
            semantics: json!({}),
        }
    }

    #[async_trait]
    impl fin_domain::Adapter for SyntheticBoundaryAdapter {
        fn definitions(&self) -> Vec<fin_domain::Definition> {
            vec![synthetic_boundary_definition()]
        }

        async fn read(&self, _: &str, _: &Value) -> Result<Value, FinError> {
            Ok(json!({}))
        }

        async fn prepare(
            &self,
            operation: &str,
            input: &Value,
        ) -> Result<fin_domain::PreparedOperation, FinError> {
            let units = input["units"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    FinError::validation("Synthetic units must be a whole decimal string.")
                })?;
            let amount = fin_domain::Quantity::new((units * 1_000_000).to_string())?;
            let resource = fin_domain::ResourceId("synthetic:cash".into());
            let asset = fin_domain::AssetId {
                network: "synthetic".into(),
                reference: "USD".into(),
                decimals: 6,
            };
            let now = chrono::Utc::now();
            Ok(fin_domain::PreparedOperation {
                operation: operation.into(),
                input: input.clone(),
                binding: json!({"account":input["account"],"units":input["units"]}),
                summary: format!("Execute {units} synthetic units."),
                checks: vec![],
                warnings: vec![],
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(2)).to_rfc3339(),
                valuation: Some(fin_domain::Valuation {
                    movement_usd_micros: amount.clone(),
                    maximum_fee_usd_micros: fin_domain::Quantity::new("0")?,
                    additional_allocation_usd_micros: fin_domain::Quantity::new("0")?,
                    observed_at: now.to_rfc3339(),
                    source: "synthetic fixture".into(),
                    prices: vec![fin_domain::AssetPrice {
                        asset: asset.clone(),
                        usd_micros_per_unit: fin_domain::Quantity::new("1000000")?,
                        unit_atoms: fin_domain::Quantity::new("1000000")?,
                        observed_at: now.to_rfc3339(),
                        source: "synthetic fixture".into(),
                    }],
                }),
                intent: fin_domain::FinancialIntent {
                    debits: vec![fin_domain::ResourceAmount {
                        resource: resource.clone(),
                        amount: fin_domain::Amount {
                            asset: asset.clone(),
                            atoms: amount.clone(),
                        },
                    }],
                    maximum_costs: vec![],
                    expected: vec![fin_domain::EffectConstraint::Debit {
                        resource,
                        asset,
                        maximum_debit: amount,
                    }],
                    locks: vec![],
                },
                operation_version: 1,
                action_deadline: None,
            })
        }

        async fn prepare_boundaries(
            &self,
            prepared: &fin_domain::PreparedOperation,
            bounds: &Value,
        ) -> Result<fin_domain::boundaries::BoundarySpec, FinError> {
            let requested = prepared.input["units"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| FinError::validation("Synthetic requested units are invalid."))?;
            let maximum = bounds["max_units"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| FinError::validation("Synthetic max_units bound is invalid."))?;
            if maximum > requested {
                return Err(FinError::validation(
                    "Synthetic maximum cannot exceed the requested target.",
                ));
            }
            Ok(fin_domain::boundaries::BoundarySpec {
                fixed: json!({"account":prepared.input["account"]}),
                parameters: bounds.clone(),
                maximum_debits: prepared.intent.debits.clone(),
                maximum_costs: vec![],
                summary: format!(
                    "Authorize up to {maximum} synthetic units for account {}.",
                    prepared.input["account"]
                ),
            })
        }

        fn validate_boundary_candidate(
            &self,
            spec: &fin_domain::boundaries::BoundarySpec,
            prepared: &fin_domain::PreparedOperation,
        ) -> Result<(), FinError> {
            let maximum = spec.parameters["max_units"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| FinError::validation("Synthetic max_units bound is invalid."))?;
            let candidate = prepared.input["units"]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| FinError::validation("Synthetic units are invalid."))?;
            if spec.fixed["account"] != prepared.input["account"] || candidate > maximum {
                let constraint = if spec.fixed["account"] != prepared.input["account"] {
                    "account"
                } else {
                    "max_units"
                };
                let (approved_bound, candidate_value) = if constraint == "account" {
                    (
                        spec.fixed["account"].clone(),
                        prepared.input["account"].clone(),
                    )
                } else {
                    (json!(maximum.to_string()), json!(candidate.to_string()))
                };
                return Err(fin_domain::boundaries::violation_error(
                    "Synthetic candidate exceeds its approved scope.",
                    fin_domain::boundaries::BoundaryViolation {
                        constraint: constraint.into(),
                        approved_bound,
                        candidate_value,
                        evidence_time: Some(chrono::Utc::now().to_rfc3339()),
                        reauthorization_required: constraint == "account",
                        reconciliation_required: false,
                    },
                ));
            }
            Ok(())
        }

        async fn revalidate(
            &self,
            prepared: &fin_domain::PreparedOperation,
        ) -> Result<fin_domain::PreparedOperation, FinError> {
            self.prepare(&prepared.operation, &prepared.input).await
        }

        async fn submit(
            &self,
            prepared: &fin_domain::PreparedOperation,
            journal: std::sync::Arc<dyn fin_domain::SubmissionJournal>,
        ) -> Result<fin_domain::Submission, FinError> {
            journal.assert_candidate(prepared)?;
            let number = self
                .submissions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let submission = fin_domain::Submission {
                kind: "synthetic".into(),
                reference: format!("synthetic-submission-{number}"),
                data: json!({"sequence":number}),
            };
            journal.persist_submission(&submission)?;
            Ok(submission)
        }

        async fn reconcile(
            &self,
            record: &fin_domain::OperationRecord,
        ) -> Result<fin_domain::Reconciliation, FinError> {
            let submission = record
                .submission
                .clone()
                .ok_or_else(|| FinError::validation("Synthetic submission missing."))?;
            let debit = record.preview.as_ref().unwrap().intent.debits[0].clone();
            let mut effects = fin_domain::VerifiedEffects {
                evidence_version: 1,
                submission,
                effects: vec![fin_domain::FinancialEffect::CashDebit {
                    resource: debit.resource,
                    amount: debit.amount,
                }],
                evidence: json!({"receipt":"synthetic"}),
                observed_at: chrono::Utc::now().to_rfc3339(),
                proof_digest: String::new(),
            };
            effects.proof_digest = fin_domain::verified_effect_digest(&effects)?;
            Ok(fin_domain::Reconciliation {
                status: fin_domain::Status::Completed,
                evidence: effects.evidence.clone(),
                verified: Some(effects),
            })
        }
    }

    struct SyntheticBoundaryAuthorizer(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl fin_domain::Authorizer for SyntheticBoundaryAuthorizer {
        async fn authorize(
            &self,
            challenge: &fin_domain::AuthorizationChallenge,
        ) -> Result<fin_domain::ApprovalAudit, FinError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(fin_domain::ApprovalAudit {
                mechanism: "synthetic-test-only".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: chrono::Utc::now().to_rfc3339(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[tokio::test]
    async fn cli_wire_and_engine_allow_two_attempts_under_one_synthetic_grant() {
        use std::sync::atomic::Ordering;
        let authorizer = std::sync::Arc::new(SyntheticBoundaryAuthorizer(
            std::sync::atomic::AtomicUsize::new(0),
        ));
        let adapter = std::sync::Arc::new(SyntheticBoundaryAdapter::default());
        let engine = std::sync::Arc::new(
            fin_engine::Engine::new(
                fin_store::Store::in_memory().unwrap(),
                vec![adapter.clone()],
                authorizer.clone(),
                fin_engine::EngineOptions::default(),
            )
            .unwrap(),
        );
        let client = BoundaryEngineClient(engine.clone());
        let grant = Cli::try_parse_from([
            "fin",
            "authorizations",
            "authorize",
            "synthetic.trade",
            "--id",
            "synthetic-intent-cli",
            "--input",
            r#"{"account":"synthetic-account","units":"2"}"#,
            "--bounds",
            r#"{"max_units":"2"}"#,
            "--json",
        ])
        .unwrap();
        let authorization = dispatch(grant, &client).await.unwrap();
        let authorization_id = authorization["id"].as_str().unwrap().to_owned();

        let out_of_bounds = Cli::try_parse_from([
            "fin",
            "execute",
            "synthetic.trade",
            "--input",
            r#"{"account":"synthetic-account","units":"3"}"#,
            "--authorization",
            &authorization_id,
            "--dry-run",
            "--json",
        ])
        .unwrap();
        let error = dispatch(out_of_bounds, &client).await.unwrap_err();
        assert_eq!(error.code, "BOUNDARY_VIOLATION");
        let details = error.details.unwrap()["boundary_violation"].clone();
        assert_eq!(details["constraint"], "max_units");
        assert_eq!(details["approved_bound"], "2");
        assert_eq!(details["candidate_value"], "3");
        assert_eq!(details["reauthorization_required"], false);
        assert_eq!(details["reconciliation_required"], false);

        let rejected_attempt = Cli::try_parse_from([
            "fin",
            "execute",
            "synthetic.trade",
            "--input",
            r#"{"account":"synthetic-account","units":"3"}"#,
            "--id",
            "synthetic-attempt-out-of-bounds",
            "--authorization",
            &authorization_id,
            "--json",
        ])
        .unwrap();
        let rejected = dispatch(rejected_attempt, &client).await.unwrap();
        assert_eq!(rejected["status"], "failed");
        assert_eq!(rejected["error"]["code"], "BOUNDARY_VIOLATION");
        assert_eq!(
            rejected["error"]["details"]["boundary_violation"]["candidate_value"],
            "3"
        );
        for (attempt_id, units) in [
            ("synthetic-attempt-one", "1"),
            ("synthetic-attempt-two", "1"),
        ] {
            let execute = Cli::try_parse_from([
                "fin",
                "execute",
                "synthetic.trade",
                "--input",
                &format!(r#"{{"account":"synthetic-account","units":"{units}"}}"#),
                "--id",
                attempt_id,
                "--authorization",
                &authorization_id,
                "--json",
            ])
            .unwrap();
            let result = dispatch(execute, &client).await.unwrap();
            assert_eq!(result["status"], "completed", "attempt result: {result}");
        }

        let status = dispatch(
            Cli::try_parse_from([
                "fin",
                "authorizations",
                "status",
                &authorization_id,
                "--json",
            ])
            .unwrap(),
            &client,
        )
        .await
        .unwrap();
        let attempts = status["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 3);
        // Several attempts can share one millisecond timestamp. Match durable
        // identities instead of relying on the order of equal-time rows.
        let attempt = |request_id: &str| {
            attempts
                .iter()
                .find(|row| row["request_id"] == request_id)
                .unwrap()
        };
        assert_eq!(
            attempt("synthetic-attempt-out-of-bounds")["status"],
            "failed"
        );
        assert_eq!(attempt("synthetic-attempt-one")["status"], "completed");
        assert_eq!(attempt("synthetic-attempt-two")["status"], "completed");
        assert_eq!(
            status["authorization"]["remaining_debits"][0]["amount"]["atoms"],
            "0"
        );
        assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 2);

        // Replacement is allowed only for an unsubmitted active candidate.
        // The synthetic adapter has completed both prior attempts, so the CLI
        // route must preserve the engine's refusal and must not submit again.
        let completed_attempt_id = attempt("synthetic-attempt-two")["id"].as_str().unwrap();
        let replacement = Cli::try_parse_from([
            "fin",
            "execute",
            "synthetic.trade",
            "--input",
            r#"{"account":"synthetic-account","units":"1"}"#,
            "--id",
            "synthetic-attempt-replacement",
            "--authorization",
            &authorization_id,
            "--replaces-attempt",
            completed_attempt_id,
            "--json",
        ])
        .unwrap();
        let error = dispatch(replacement, &client).await.unwrap_err();
        assert_eq!(error.code, "BOUNDARY_AUTHORIZATION_INVALID");
        assert_eq!(adapter.submissions.load(Ordering::SeqCst), 2);

        let untrusted = Cli::try_parse_from([
            "fin",
            "authorizations",
            "authorize",
            "synthetic.trade",
            "--id",
            "synthetic-intent-untrusted",
            "--input",
            r#"{"account":"synthetic-account","units":"1"}"#,
            "--bounds",
            r#"{"max_units":"2"}"#,
            "--json",
        ])
        .unwrap();
        let error = dispatch(untrusted, &client).await.unwrap_err();
        assert_eq!(error.code, "INVALID_INPUT");
        assert_eq!(authorizer.0.load(Ordering::SeqCst), 1);
    }
}
