use async_trait::async_trait;
use clap::{Args, Parser, Subcommand, error::ErrorKind};
use fin_domain::{FinError, OperationRequest};
use fin_protocol::{BrokerRequest, BrokerResponse, MAX_FRAME_BYTES};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(target_os = "macos")]
use tokio::net::UnixStream;

#[cfg(target_os = "macos")]
const SOCKET_PATH: &str = "/Library/Fin/run/broker.sock";
#[cfg(target_os = "macos")]
const BROKER_PATH: &str = "/Library/Fin/runtime/fin-broker";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const BROKER_TIMEOUT: Duration = Duration::from_secs(8);
const FINANCIAL_WRITE_TIMEOUT: Duration = Duration::from_secs(300);
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser, Debug)]
#[command(
    name = "fin",
    version,
    about = "Discover and safely operate Fin's protected financial broker"
)]
struct Cli {
    /// Emit structured JSON (the default output format).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Capabilities,
    Explain {
        name: String,
    },
    Schema {
        name: String,
    },
    Read(OperationArgs),
    Preview(OperationArgs),
    Execute(ExecuteArgs),
    Status {
        id: String,
    },
    Operations,
    Doctor,
    Snapshot,
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    Setup,
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Connections {
        #[command(subcommand)]
        command: Option<ConnectionsCommand>,
    },
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
    #[arg(long)]
    run: Option<String>,
}

#[derive(Subcommand, Debug)]
enum RunCommand {
    Begin {
        #[arg(long)]
        input: String,
    },
    Status {
        id: String,
    },
    Replan {
        id: String,
        #[arg(long)]
        input: String,
    },
    Finish {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonCommand {
    Start,
    Status,
    Stop,
}

#[derive(Subcommand, Debug)]
enum ConnectionsCommand {
    List,
    Add(fin_discovery::AddConnectionArgs),
    Update(fin_discovery::AddConnectionArgs),
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

fn protocol_error() -> FinError {
    FinError::new(
        "BROKER_PROTOCOL_ERROR",
        "transport",
        "The protected broker returned an invalid response.",
        "Check the installed broker service and retry with the same request ID.",
    )
}
fn broker_unavailable() -> FinError {
    FinError::new(
        "BROKER_UNAVAILABLE",
        "transport",
        "The protected broker is unavailable.",
        "Start the installed Fin service and retry the same request ID.",
    )
}
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
    let _json = cli.json;
    match cli.command {
        Command::Capabilities => Ok(capabilities()),
        Command::Explain { name } => find_definition(&name).ok_or_else(|| {
            FinError::new(
                "UNKNOWN_DEFINITION",
                "discovery",
                "No bundled definition has that name.",
                "Inspect fin capabilities --json.",
            )
        }),
        Command::Schema { name } => schema(&name).ok_or_else(|| {
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
                    run_id: args.run,
                })
                .await
        }
        Command::Status { id } => client.call(BrokerRequest::Status { id }).await,
        Command::Operations => client.call(BrokerRequest::Operations).await,
        Command::Doctor => client.call(BrokerRequest::Doctor).await,
        Command::Snapshot => client.call(BrokerRequest::Snapshot).await,
        Command::Run { command } => match command {
            RunCommand::Begin { input } => {
                client
                    .call(BrokerRequest::RunBegin {
                        request: serde_json::from_value(parse_input(&input)?)
                            .map_err(|_| generic_input_error())?,
                    })
                    .await
            }
            RunCommand::Status { id } => client.call(BrokerRequest::RunStatus { id }).await,
            RunCommand::Replan { id, input } => {
                let request: fin_discovery::RunReplanInput =
                    serde_json::from_value(parse_input(&input)?)
                        .map_err(|_| generic_input_error())?;
                client
                    .call(BrokerRequest::RunReplan {
                        id,
                        remaining: request.remaining,
                    })
                    .await
            }
            RunCommand::Finish { id } => client.call(BrokerRequest::RunFinish { id }).await,
        },
        Command::Daemon { command } => match command {
            DaemonCommand::Start => client.call(BrokerRequest::DaemonStart).await,
            DaemonCommand::Status => client.call(BrokerRequest::Doctor).await,
            DaemonCommand::Stop => client.call(BrokerRequest::DaemonStop).await,
        },
        Command::Setup => launch_human_privileged(PrivilegedAction::Setup),
        Command::Connections { command } => match command {
            None | Some(ConnectionsCommand::List) => client.call(BrokerRequest::Connections).await,
            Some(ConnectionsCommand::Add(args)) => {
                fin_discovery::validate_add(&args)?;
                launch_human_privileged(PrivilegedAction::Add(args))
            }
            Some(ConnectionsCommand::Update(args)) => {
                fin_discovery::validate_add(&args)?;
                launch_human_privileged(PrivilegedAction::Update(args))
            }
            Some(ConnectionsCommand::Remove(args)) => {
                fin_discovery::validate_name(&args.name)?;
                launch_human_privileged(PrivilegedAction::Remove(args))
            }
        },
    }
}

enum PrivilegedAction {
    Setup,
    Add(fin_discovery::AddConnectionArgs),
    Update(fin_discovery::AddConnectionArgs),
    Remove(fin_discovery::NamedConnectionArgs),
}

fn launch_human_privileged(action: PrivilegedAction) -> Result<Value, FinError> {
    launch_human_privileged_with_terminal(action, stdin_is_terminal())
}

fn launch_human_privileged_with_terminal(
    action: PrivilegedAction,
    is_terminal: bool,
) -> Result<Value, FinError> {
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
        let _ = action;
        Err(FinError::new(
            "PROTECTED_PROVIDER_UNAVAILABLE",
            "enrollment",
            "Protected installation and credential enrollment are implemented only for macOS.",
            "Use the reviewed macOS Fin installation.",
        ))
    }
    #[cfg(target_os = "macos")]
    {
        use std::{os::unix::process::CommandExt, process::Command};
        let mut command = Command::new("/usr/bin/sudo");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("-k")
            .arg("--");
        match action {
            PrivilegedAction::Setup => {
                let script = std::env::current_exe()
                    .ok()
                    .and_then(|path| path.parent().map(|parent| parent.join("install.sh")))
                    .filter(|path| path.is_absolute())
                    .ok_or_else(|| FinError::new(
                        "INSTALLER_MISSING",
                        "setup",
                        "The bundled Fin installer could not be located beside this executable.",
                        "Run setup from the reviewed installation bundle.",
                    ))?;
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
            PrivilegedAction::Remove(args) => {
                command
                    .arg(BROKER_PATH)
                    .args(["enrollment", "remove", "--request"])
                    .arg(serde_json::to_string(&args).map_err(|_| generic_input_error())?);
            }
        }
        let error = command.exec();
        Err(FinError::new(
            "PRIVILEGED_EXEC_FAILED",
            "enrollment",
            "The fixed protected Fin command could not be started.",
            "Check the reviewed installation and run the command again in Terminal.",
        )
        .into_with_detail(error))
    }
}

trait ErrorWithoutEcho {
    fn into_with_detail(self, error: std::io::Error) -> FinError;
}
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

fn emit<T: Serialize>(result: Result<T, FinError>) -> i32 {
    match result {
        Ok(value) => {
            let output = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
            println!("{output}");
            0
        }
        Err(error) => {
            let output = serde_json::to_string(&json!({"ok":false,"error":error}))
                .unwrap_or_else(|_| "{}".into());
            println!("{output}");
            1
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
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
            std::process::exit(emit::<Value>(Err(error)));
        }
    };
    let client = UnixTransport::installed();
    std::process::exit(emit(dispatch(cli, &client).await));
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
    fn generated_help_and_catalog_do_not_need_broker() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("capabilities"));
        assert!(help.contains("connections"));
        assert_eq!(financial_catalog().len(), 29);
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
            launch_human_privileged_with_terminal(PrivilegedAction::Setup, false).unwrap_err();
        assert_eq!(error.code, "HUMAN_TERMINAL_REQUIRED");
    }
}
