use async_trait::async_trait;
use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use fin_discovery::CompletionShell;
use fin_domain::{FinError, OperationRequest};
use fin_presentation::Ui;
use fin_protocol::BrokerRequest;
#[cfg(any(target_os = "macos", test))]
use fin_protocol::{BrokerResponse, MAX_FRAME_BYTES};
mod daemon;
mod onboarding;
mod shell_integration;
use serde_json::{Value, json};
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
const BROKER_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(target_os = "macos")]
const FINANCIAL_WRITE_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(target_os = "macos")]
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
#[command(
    name = "fin",
    color = clap::ColorChoice::Never,
    version,
    about = "Discover and safely operate Fin's protected financial broker"
)]
struct Cli {
    /// Machine contract: JSON only, no terminal prompts, progress or color.
    #[arg(long, global = true)]
    json: bool,
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
    /// Execute an operation with fresh Touch ID approval.
    Execute(ExecuteArgs),
    /// Check and reconcile an existing operation.
    Status { id: String },
    /// List recorded operations and unresolved submissions.
    Operations,
    /// Check configuration and protected service readiness.
    Doctor,
    /// Read financial state within the configured tracking scope.
    Snapshot,
    /// Install protected service/storage; macOS may request your Mac login password.
    #[command(
        after_help = "Setup asks for movement and run-allocation limits in your protected Terminal. Enter keeps $100; 0 means no personal limit. Fresh Touch ID approves the selected limits."
    )]
    Setup,
    /// Start, stop or check the protected service.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Show or change protected human-owned financial limits.
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

#[derive(Args, Debug)]
#[command(mut_arg("provider", |arg| arg.required(false)), mut_arg("name", |arg| arg.required(false)))]
struct ConnectionCommandArgs {
    #[command(flatten)]
    connection: Option<fin_discovery::AddConnectionArgs>,
}

#[derive(Args, Debug)]
struct OperationArgs {
    operation: String,
    #[arg(long)]
    input: String,
}

#[derive(Args, Debug)]
struct ExecuteArgs {
    operation: String,
    #[arg(long)]
    input: String,
    /// Stable logical identity for retries of this exact operation.
    #[arg(long)]
    id: String,
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
    /// Change both limits in your own Terminal with fresh Touch ID; refuses unresolved work.
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
trait Transport: Send + Sync {
    async fn call(&self, request: BrokerRequest) -> Result<Value, FinError>;
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
        let response_timeout = match &request {
            BrokerRequest::Execute { .. } => FINANCIAL_WRITE_TIMEOUT,
            BrokerRequest::RunBegin { .. }
            | BrokerRequest::RunReplan { .. }
            | BrokerRequest::Snapshot => OBSERVER_TIMEOUT,
            _ => BROKER_TIMEOUT,
        };
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.path))
            .await
            .map_err(|_| broker_timeout())?
            .map_err(|_| broker_unavailable())?;
        if peer_uid(&stream)? != self.expected_uid {
            return Err(protocol_error());
        }
        let mut request_bytes = serde_json::to_vec(&request).map_err(|_| protocol_error())?;
        request_bytes.push(b'\n');
        if request_bytes.len() > MAX_FRAME_BYTES {
            return Err(protocol_error());
        }
        tokio::time::timeout(CONNECT_TIMEOUT, stream.write_all(&request_bytes))
            .await
            .map_err(|_| broker_timeout())?
            .map_err(|_| broker_unavailable())?;
        stream.shutdown().await.map_err(|_| broker_unavailable())?;
        tokio::time::timeout(response_timeout, read_response(BufReader::new(stream)))
            .await
            .map_err(|_| broker_timeout())?
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
async fn read_response<R: AsyncRead + Unpin>(reader: R) -> Result<Value, FinError> {
    let reader = BufReader::new(reader);
    let mut bytes = Vec::with_capacity(1024);
    let read = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| broker_unavailable())?;
    if read == 0 || bytes.len() > MAX_FRAME_BYTES || bytes.last() != Some(&b'\n') {
        return Err(protocol_error());
    }
    let response: BrokerResponse = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
    if !response.validate() {
        return Err(protocol_error());
    }
    if let Some(error) = response.error {
        return Err(error);
    }
    response.result.ok_or_else(protocol_error)
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
#[cfg(target_os = "macos")]
fn broker_timeout() -> FinError {
    FinError::new(
        "BROKER_TIMEOUT",
        "transport",
        "The protected broker did not respond before the local timeout.",
        "Check status with the same request ID; do not create another financial request.",
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
        "operations": financial_catalog(),
        "service_required": false,
        "machine_contract": fin_discovery::machine_contract(),
        "evm_networks": fin_discovery::evm_network_profiles(),
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
trait Client {
    async fn call(&self, request: BrokerRequest) -> Result<Value, FinError>;
    async fn execute(&self, request: OperationRequest) -> Result<Value, FinError>;
}
#[async_trait]
impl Client for UnixTransport {
    async fn call(&self, request: BrokerRequest) -> Result<Value, FinError> {
        Transport::call(self, request).await
    }
    async fn execute(&self, request: OperationRequest) -> Result<Value, FinError> {
        execute_with_recovery(self, request).await
    }
}

async fn dispatch(cli: Cli, client: &dyn Client) -> Result<Value, FinError> {
    let json = cli.json;
    let verbose = cli.verbose;
    if json && human_only(&cli.command) {
        return Err(human_required());
    }
    match cli.command {
        Command::InternalCompletions { action } => shell_integration::run(action),
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
        Command::Explain { name: Some(name) } => find_definition(&name).ok_or_else(|| {
            FinError::new(
                "UNKNOWN_DEFINITION",
                "discovery",
                "No bundled definition has that name.",
                "Inspect fin capabilities --json.",
            )
        }),
        Command::Schema { name: Some(name) } => schema(&name).ok_or_else(|| {
            FinError::new(
                "UNKNOWN_DEFINITION",
                "discovery",
                "No bundled schema has that name.",
                "Inspect fin capabilities --json.",
            )
        }),
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
            client
                .execute(OperationRequest {
                    request_id: args.id,
                    operation: args.operation,
                    input: parse_input(&args.input)?,
                    run_id: None,
                })
                .await
        }
        Command::Status { id } => client.call(BrokerRequest::Status { id }).await,
        Command::Operations => client.call(BrokerRequest::Operations).await,
        Command::Doctor => client.call(BrokerRequest::Doctor).await,
        Command::Snapshot => client.call(BrokerRequest::Snapshot).await,
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
                let args = match args.connection {
                    Some(args) => args,
                    None if stdin_is_terminal() => onboarding::guided_add()?,
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
            "Fin needs administrator access to change protected financial limits. Touch ID separately approves the exact limits."
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
        Command::Capabilities => "capabilities",
        Command::Explain { .. } => "explain",
        Command::Schema { .. } => "schema",
        Command::Completions { .. } => "completions",
        Command::Read(_) => "read",
        Command::Preview(_) => "preview",
        Command::Execute(_) => "execute",
        Command::Status { .. } => "status",
        Command::Operations => "operations",
        Command::Doctor => "doctor",
        Command::Snapshot => "snapshot",
        Command::Setup => "setup",
        Command::Daemon { .. } => "daemon",
        Command::Connections { .. } => "connections",
        Command::Policy { .. } => "policy",
    }
}

// Resolve output mode even for parse failures, without inspecting or echoing values.
fn requested_json(args: &[std::ffi::OsString]) -> bool {
    let mut value = false;
    for argument in args.iter().skip(1) {
        if value {
            value = false;
            continue;
        }
        if argument == "--" {
            break;
        }
        if argument == "--json" {
            return true;
        }
        value = matches!(
            argument.to_str(),
            Some(
                "--input"
                    | "--id"
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
            } else if command == "daemon" && !ui.is_machine() {
                ui.success(fin_presentation::render_human(command, &value, false).trim_end())
            } else {
                ui.write_result(&value, command)
            };
            i32::from(output.is_err())
        }
        Err(error) => {
            let _ = ui.write_error(&error);
            1
        }
    }
}

#[tokio::main]
async fn main() {
    let arguments: Vec<_> = std::env::args_os().collect();
    let machine = requested_json(&arguments);
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
                std::process::exit(emit(Ok(value), &Ui::new(true, false), "help"));
            }
            let _ = error.print();
            std::process::exit(0);
        }
        Err(_) => {
            let error = FinError::new(
                "INVALID_COMMAND",
                "cli",
                "The command or arguments are invalid.",
                "Run fin --help to inspect supported commands.",
            );
            std::process::exit(emit(Err(error), &Ui::new(machine, false), "cli"));
        }
    };
    let ui = Ui::new(cli.json, cli.verbose);
    let name = command_name(&cli.command);
    // Privileged commands replace this process; never leave a spinner alive across exec.
    let offline = matches!(
        cli.command,
        Command::Capabilities
            | Command::Explain { .. }
            | Command::Schema { .. }
            | Command::Completions { .. }
            | Command::InternalCompletions { .. }
    );
    let progress = if !offline && !human_only(&cli.command) {
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
            Command::Execute(_) => "Waiting for review, Touch ID and execution...",
            _ => "Checking Fin...",
        }))
    } else {
        None
    };
    let client = UnixTransport::installed();
    let result = dispatch(cli, &client).await;
    drop(progress);
    std::process::exit(emit(result, &ui, name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serde_json::json;
    #[cfg(target_os = "macos")]
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn offline_discovery_is_available_without_a_broker() {
        let caps = capabilities();
        assert_eq!(caps["service_required"], false);
        assert!(!caps["commands"].as_array().unwrap().is_empty());
        assert!(find_definition("setup").is_some());
        assert!(schema("connections.add").is_some());
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
        assert_eq!(financial_catalog().len(), 30);
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
        let mut bytes = vec![b'x'; MAX_FRAME_BYTES + 2];
        bytes.push(b'\n');
        let error = read_response(&bytes[..]).await.unwrap_err();
        assert_eq!(error.code, "BROKER_PROTOCOL_ERROR");
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
    fn guided_add_is_optional_but_partial_metadata_is_not_silently_discarded() {
        let bare = Cli::try_parse_from(["fin", "connections", "add"]).unwrap();
        assert!(matches!(
            bare.command,
            Command::Connections {
                command: Some(ConnectionsCommand::Add(ConnectionCommandArgs {
                    connection: None
                }))
            }
        ));
        for arguments in [
            vec!["fin", "connections", "add", "--provider", "binance"],
            vec!["fin", "connections", "add", "--role", "main"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        let full = Cli::try_parse_from([
            "fin",
            "connections",
            "add",
            "--provider",
            "binance",
            "--name",
            "main",
            "--role",
            "main",
        ])
        .unwrap();
        assert!(matches!(
            full.command,
            Command::Connections {
                command: Some(ConnectionsCommand::Add(ConnectionCommandArgs {
                    connection: Some(_)
                }))
            }
        ));
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
            PrivilegedAction::Add(args),
            PrivilegedAction::Update(update),
            PrivilegedAction::Remove(remove),
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
}
