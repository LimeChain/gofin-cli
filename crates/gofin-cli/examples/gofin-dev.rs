//! Development-only CLI. The production parser and dispatch path are shared
//! with `gofin`; this transport is available only in this example binary.
#![allow(dead_code)]

#[path = "../src/main.rs"]
mod production_cli;

use async_trait::async_trait;
use gofin_domain::GoFinError;
use gofin_protocol::{BrokerRequest, BrokerResponse, MAX_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES};
use production_cli::Transport;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DevelopmentMode {
    #[default]
    Synthetic,
    Live,
}

impl DevelopmentMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Synthetic => "synthetic",
            Self::Live => "live",
        }
    }
}

#[derive(Serialize)]
struct HelloRequest<'a> {
    kind: &'static str,
    identity: &'a str,
    build_id: &'a str,
    mode: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloResponse {
    kind: String,
    identity: String,
    build_id: String,
    mode: String,
}

#[derive(Serialize)]
struct DevRequest<'a> {
    identity: &'a str,
    build_id: &'a str,
    mode: &'a str,
    request: BrokerRequest,
    progress: bool,
    cancel_on_disconnect: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DevResponse {
    identity: String,
    build_id: String,
    mode: String,
    response: BrokerResponse,
}

struct DevelopmentTransport {
    socket: PathBuf,
    identity: String,
    build_id: String,
    mode: DevelopmentMode,
    financial_dispatch_started: Arc<AtomicBool>,
}

fn hello_matches(
    hello: &HelloResponse,
    identity: &str,
    build_id: &str,
    mode: DevelopmentMode,
) -> bool {
    hello.kind == "hello"
        && hello.identity == identity
        && hello.build_id == build_id
        && hello.mode == mode.as_str()
}

fn response_matches(
    response: &DevResponse,
    identity: &str,
    build_id: &str,
    mode: DevelopmentMode,
) -> bool {
    response.identity == identity
        && response.build_id == build_id
        && response.mode == mode.as_str()
        && response.response.validate()
}

#[async_trait]
impl Transport for DevelopmentTransport {
    fn status_recovery_command(&self, id: &str) -> Option<String> {
        Some(development_status_command(
            std::env::var("GOFIN_DEV_ENV_NAME").ok().as_deref(),
            id,
        ))
    }

    fn development_mode(&self) -> bool {
        true
    }

    fn live_development_mode(&self) -> bool {
        self.mode == DevelopmentMode::Live
    }

    async fn call(&self, request: BrokerRequest) -> Result<Value, GoFinError> {
        let stream =
            tokio::time::timeout(Duration::from_secs(3), UnixStream::connect(&self.socket))
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
        verify_peer(&stream)?;
        let mut reader = BufReader::new(stream);
        let response_timeout = production_cli::broker_response_timeout(&request);
        let cancel_on_disconnect = request.may_use_rpc_reads();
        let financial_dispatch = matches!(
            &request,
            BrokerRequest::Execute { .. }
                | BrokerRequest::ExecuteBoundary { .. }
                | BrokerRequest::ReplaceBoundaryCandidate { .. }
        );
        write_frame(
            reader.get_mut(),
            &HelloRequest {
                kind: "hello",
                identity: &self.identity,
                build_id: &self.build_id,
                mode: self.mode.as_str(),
            },
        )
        .await?;
        let response = read_bounded_line(&mut reader, 4096, Duration::from_secs(3))
            .await
            .map_err(|_| identity_error())?;
        let hello: HelloResponse =
            serde_json::from_slice(&response).map_err(|_| identity_error())?;
        if !hello_matches(&hello, &self.identity, &self.build_id, self.mode) {
            return Err(identity_error());
        }
        // Identity is checked on this peer and connection before the typed
        // operation frame is sent. The broker checks the same binding itself.
        if financial_dispatch {
            self.financial_dispatch_started
                .store(true, Ordering::SeqCst);
        }
        write_frame(
            reader.get_mut(),
            &DevRequest {
                identity: &self.identity,
                build_id: &self.build_id,
                mode: self.mode.as_str(),
                request,
                progress: gofin_domain::execution_progress::enabled(),
                cancel_on_disconnect,
            },
        )
        .await
        .map_err(|_| uncertain())?;
        let mut deadline = Some(tokio::time::Instant::now() + response_timeout);
        let response = async {
            for _ in 0..64 {
                let response =
                    read_bounded_line_until(&mut reader, MAX_RESPONSE_FRAME_BYTES, deadline)
                        .await
                        .map_err(|_| uncertain())?;
                if let Ok(frame) =
                    serde_json::from_slice::<gofin_protocol::progress::RpcReadFrame>(&response)
                {
                    if !frame.rpc_read || !cancel_on_disconnect {
                        return Err(identity_error());
                    }
                    deadline = None;
                    continue;
                }
                if let Ok(frame) =
                    serde_json::from_slice::<gofin_protocol::progress::ProgressFrame>(&response)
                {
                    gofin_domain::execution_progress::emit(frame.progress);
                    continue;
                }
                return serde_json::from_slice(&response).map_err(|_| identity_error());
            }
            Err(identity_error())
        };
        let response: DevResponse = response.await?;
        if !response_matches(&response, &self.identity, &self.build_id, self.mode) {
            return Err(identity_error());
        }
        if let Some(error) = response.response.error {
            return Err(error);
        }
        response.response.result.ok_or_else(identity_error)
    }
}

async fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<(), GoFinError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| identity_error())?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(identity_error());
    }
    tokio::time::timeout(Duration::from_secs(3), stream.write_all(&bytes))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
    timeout: Duration,
) -> Result<Vec<u8>, GoFinError> {
    read_bounded_line_until(reader, max, Some(tokio::time::Instant::now() + timeout)).await
}

async fn read_bounded_line_until<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
    deadline: Option<tokio::time::Instant>,
) -> Result<Vec<u8>, GoFinError> {
    let mut limited = reader.take((max + 1) as u64);
    let mut bytes = Vec::with_capacity(128);
    let read = limited.read_until(b'\n', &mut bytes);
    let read = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, read)
            .await
            .map_err(|_| unavailable())?,
        None => read.await,
    }
    .map_err(|_| unavailable())?;
    if read == 0 || bytes.len() > max || bytes.last() != Some(&b'\n') {
        return Err(identity_error());
    }
    bytes.pop();
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn verify_peer(stream: &UnixStream) -> Result<(), GoFinError> {
    use std::os::fd::AsRawFd;
    let mut uid = 0u32;
    let mut gid = 0u32;
    // SAFETY: getpeereid writes the peer's fixed-width uid and gid.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0
        || uid != unsafe { libc::geteuid() }
    {
        return Err(identity_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer(stream: &UnixStream) -> Result<(), GoFinError> {
    use std::{
        mem::{MaybeUninit, size_of},
        os::fd::AsRawFd,
    };
    let mut credential = MaybeUninit::<libc::ucred>::uninit();
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt fills a ucred value when the returned length matches.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credential.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0
        || length as usize != size_of::<libc::ucred>()
        || unsafe { credential.assume_init() }.uid != unsafe { libc::geteuid() }
    {
        return Err(identity_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn verify_peer(_stream: &UnixStream) -> Result<(), GoFinError> {
    Err(identity_error())
}

fn unavailable() -> GoFinError {
    GoFinError::new(
        "BROKER_UNAVAILABLE",
        "development",
        "The selected development broker is unavailable.",
        "Start the matching GoFin development environment and retry.",
    )
}

fn development_status_command(name: Option<&str>, id: &str) -> String {
    // Names supplied by the Make wrapper follow gofin-dev.py's validation. Check
    // again before displaying a copyable command; this is guidance, not authority.
    match name.filter(|name| {
        !name.is_empty()
            && name.len() <= 48
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    }) {
        Some(name) => format!("make dev-env-cli DEV_ENV={name} ARGS='status {id}'"),
        None => format!("Use the same development environment's CLI to run status {id}."),
    }
}

fn identity_error() -> GoFinError {
    GoFinError::new(
        "DEVELOPMENT_IDENTITY_MISMATCH",
        "development",
        "The broker identity does not match this development CLI.",
        "Start the broker built from this worktree and use its environment CLI.",
    )
}

fn startup_failure(arguments: &[OsString], error: GoFinError) -> ! {
    if arguments.iter().any(|argument| argument == "--json") {
        if arguments.iter().any(|argument| argument == "--stream") {
            let _ = production_cli::json_stream::JsonStream::stdout().finish(Err(error));
        } else {
            println!("{}", machine_error_json(error));
        }
    } else {
        eprintln!("✗ {}", error.message);
        eprintln!("{}", error.next_action);
    }
    std::process::exit(1)
}

fn uncertain() -> GoFinError {
    GoFinError::new(
        "OUTCOME_UNKNOWN",
        "transport",
        "The selected broker stopped responding after receiving the operation frame.",
        "Inspect the same environment's durable operation journal before retrying.",
    )
}

fn cancelled() -> GoFinError {
    GoFinError::new(
        "CANCELLED",
        "development",
        "The development request was cancelled before a financial operation was dispatched.",
        "Retry the same command when ready; no operation was dispatched.",
    )
}

fn cancellation_result(financial_dispatch_started: bool) -> GoFinError {
    if financial_dispatch_started {
        uncertain()
    } else {
        cancelled()
    }
}

fn machine_error_json(error: GoFinError) -> String {
    serde_json::json!({"ok":false,"error":error}).to_string()
}

fn parse_mode(value: &str) -> Option<DevelopmentMode> {
    match value {
        "synthetic" => Some(DevelopmentMode::Synthetic),
        "live" => Some(DevelopmentMode::Live),
        _ => None,
    }
}

#[tokio::main]
async fn main() {
    let mut input: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(flag) = input.first().and_then(|arg| arg.to_str()) else {
        startup_failure(&input, identity_error());
    };
    if flag != "--identity" || input.len() < 2 {
        startup_failure(&input, identity_error());
    }
    let identity = input.remove(1).to_string_lossy().into_owned();
    input.remove(0);
    if input.first().and_then(|arg| arg.to_str()) != Some("--build-id") || input.len() < 2 {
        startup_failure(&input, identity_error());
    }
    let build_id = input.remove(1).to_string_lossy().into_owned();
    input.remove(0);
    let mode = if input.first().and_then(|arg| arg.to_str()) == Some("--mode") {
        if input.len() < 2 {
            startup_failure(&input, identity_error());
        }
        let Some(mode) = parse_mode(&input[1].to_string_lossy()) else {
            startup_failure(&input, identity_error());
        };
        input.drain(..2);
        mode
    } else {
        DevelopmentMode::Synthetic
    };
    let compiled_build = option_env!("GOFIN_DEV_BUILD_ID");
    if identity.is_empty()
        || std::env::var("GOFIN_DEV_IDENTITY").ok().as_deref() != Some(&identity)
        || build_id.is_empty()
        || compiled_build != Some(build_id.as_str())
        || std::env::var("GOFIN_DEV_BUILD_ID").ok().as_deref() != Some(&build_id)
    {
        startup_failure(&input, identity_error());
    }
    let Some(socket) = std::env::var_os("GOFIN_DEV_SOCKET").map(PathBuf::from) else {
        startup_failure(&input, unavailable());
    };
    let mut arguments = vec![OsString::from("gofin")];
    arguments.extend(input);
    let financial_dispatch_started = Arc::new(AtomicBool::new(false));
    let transport = DevelopmentTransport {
        socket,
        identity,
        build_id,
        mode,
        financial_dispatch_started: financial_dispatch_started.clone(),
    };
    let cancellation_args = arguments.clone();
    let execution = production_cli::run_with_client(arguments, &transport);
    tokio::pin!(execution);
    tokio::select! {
        status = &mut execution => std::process::exit(status),
        signal = tokio::signal::ctrl_c() => {
            if signal.is_err() {
                startup_failure(&cancellation_args, unavailable());
            }
            let error = cancellation_result(financial_dispatch_started.load(Ordering::SeqCst));
            startup_failure(&cancellation_args, error);
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn a_silent_development_hello_still_has_a_bounded_identity_check() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("broker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (requested, request_received) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            read_bounded_line(&mut reader, 4096, Duration::from_secs(3))
                .await
                .unwrap();
            requested.send(()).unwrap();
            released.await.unwrap();
        });
        let transport = DevelopmentTransport {
            socket,
            identity: "synthetic".into(),
            build_id: "test-build".into(),
            mode: DevelopmentMode::Synthetic,
            financial_dispatch_started: Arc::new(AtomicBool::new(false)),
        };
        let read = tokio::spawn(async move { transport.call(BrokerRequest::Snapshot).await });
        request_received.await.unwrap();
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            read.await.unwrap().unwrap_err().code,
            "DEVELOPMENT_IDENTITY_MISMATCH"
        );
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn verified_development_read_waits_past_the_old_response_deadline() {
        use gofin_domain::execution_progress::{self, Stage};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("broker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (requested, request_received) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let hello: Value = serde_json::from_slice(
                &read_bounded_line(&mut reader, 4096, Duration::from_secs(3))
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(hello["kind"], "hello");
            assert_eq!(hello["identity"], "synthetic");
            assert_eq!(hello["build_id"], "test-build");
            assert_eq!(hello["mode"], "synthetic");
            reader.get_mut().write_all(
                b"{\"kind\":\"hello\",\"identity\":\"synthetic\",\"build_id\":\"test-build\",\"mode\":\"synthetic\"}\n",
            ).await.unwrap();
            let request: Value = serde_json::from_slice(
                &read_bounded_line(&mut reader, 4096, Duration::from_secs(3))
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(request["request"]["method"], "read");
            assert_eq!(request["request"]["operation"], "wallet.balances");
            assert_eq!(request["identity"], "synthetic");
            assert_eq!(request["cancel_on_disconnect"], true);
            assert_eq!(request["progress"], true);
            reader
                .get_mut()
                .write_all(b"{\"rpc_read\":true}\n{\"progress\":\"preparing\"}\n")
                .await
                .unwrap();
            requested.send(()).unwrap();
            released.await.unwrap();
            reader.get_mut().write_all(
                b"{\"identity\":\"synthetic\",\"build_id\":\"test-build\",\"mode\":\"synthetic\",\"response\":{\"ok\":true,\"result\":{\"amount\":\"2.5\"}}}\n",
            ).await.unwrap();
        });
        let financial_dispatch_started = Arc::new(AtomicBool::new(false));
        let transport = DevelopmentTransport {
            socket,
            identity: "synthetic".into(),
            build_id: "test-build".into(),
            mode: DevelopmentMode::Synthetic,
            financial_dispatch_started: financial_dispatch_started.clone(),
        };
        let (stages, mut progress) = execution_progress::channel();
        let read = tokio::spawn(async move {
            execution_progress::observe(
                stages,
                transport.call(BrokerRequest::Read {
                    operation: "wallet.balances".into(),
                    input: serde_json::json!({"wallet":"synthetic"}),
                }),
            )
            .await
        });
        request_received.await.unwrap();
        assert_eq!(progress.recv().await, Some(Stage::Preparing));
        tokio::time::advance(Duration::from_secs(181)).await;
        tokio::task::yield_now().await;
        assert!(
            !read.is_finished(),
            "verified read responses have no deadline"
        );
        release.send(()).unwrap();
        assert_eq!(
            read.await.unwrap().unwrap(),
            serde_json::json!({"amount":"2.5"})
        );
        assert!(!financial_dispatch_started.load(Ordering::SeqCst));
        assert_eq!(progress.recv().await, None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn coalesced_progress_lines_retain_every_frame() {
        let mut reader = tokio::io::BufReader::new(
            &b"{\"progress\":\"preparing\"}\n{\"progress\":\"submitting\"}\n{\"response\":true}\n"
                [..],
        );
        for expected in [
            "{\"progress\":\"preparing\"}",
            "{\"progress\":\"submitting\"}",
            "{\"response\":true}",
        ] {
            let line =
                super::read_bounded_line(&mut reader, 1024, std::time::Duration::from_secs(1))
                    .await
                    .unwrap();
            assert_eq!(line, expected.as_bytes());
        }
    }
    use super::*;

    #[test]
    fn status_guidance_stays_in_the_selected_development_environment() {
        assert_eq!(
            development_status_command(Some("learn"), "synthetic-operation-1"),
            "make dev-env-cli DEV_ENV=learn ARGS='status synthetic-operation-1'"
        );
        for name in [
            None,
            Some(""),
            Some("learn; touch unwanted"),
            Some("-other"),
        ] {
            let text = development_status_command(name, "synthetic-operation-1");
            assert!(text.contains("same development environment"));
            assert!(!text.contains("Next: gofin status"));
            assert!(!text.contains("touch unwanted"));
        }
    }

    #[test]
    fn cancellation_contract_distinguishes_pre_dispatch_from_uncertain_write() {
        let before = cancellation_result(false);
        assert_eq!(before.code, "CANCELLED");
        assert_eq!(before.stage, "development");
        let after = cancellation_result(true);
        assert_eq!(after.code, "OUTCOME_UNKNOWN");
        assert_eq!(after.stage, "transport");
        assert!(after.next_action.contains("durable operation journal"));
    }

    #[test]
    fn machine_startup_error_is_one_parseable_json_envelope() {
        let output = machine_error_json(identity_error());
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "DEVELOPMENT_IDENTITY_MISMATCH");
        assert!(!output.contains('\n'));
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn development_protocol_binds_mode_on_hello_request_and_response() {
        let hello_json = serde_json::to_value(HelloRequest {
            kind: "hello",
            identity: "fixture",
            build_id: "build",
            mode: "live",
        })
        .unwrap();
        assert_eq!(hello_json["mode"], "live");
        let request_json = serde_json::to_value(DevRequest {
            identity: "fixture",
            build_id: "build",
            mode: "live",
            request: BrokerRequest::Capabilities,
            progress: false,
            cancel_on_disconnect: false,
        })
        .unwrap();
        assert_eq!(request_json["mode"], "live");

        let response = DevResponse {
            identity: "fixture".into(),
            build_id: "build".into(),
            mode: "live".into(),
            response: BrokerResponse {
                ok: true,
                result: Some(serde_json::json!({"ok":true})),
                error: None,
            },
        };
        assert!(response_matches(
            &response,
            "fixture",
            "build",
            DevelopmentMode::Live
        ));
        assert!(!response_matches(
            &response,
            "fixture",
            "build",
            DevelopmentMode::Synthetic
        ));

        let hello = HelloResponse {
            kind: "hello".into(),
            identity: "fixture".into(),
            build_id: "build".into(),
            mode: "live".into(),
        };
        assert!(hello_matches(
            &hello,
            "fixture",
            "build",
            DevelopmentMode::Live
        ));
        assert!(!hello_matches(
            &hello,
            "fixture",
            "build",
            DevelopmentMode::Synthetic
        ));
    }

    #[test]
    fn development_transport_reports_selected_mode() {
        for (mode, live) in [
            (DevelopmentMode::Synthetic, false),
            (DevelopmentMode::Live, true),
        ] {
            let transport = DevelopmentTransport {
                socket: PathBuf::new(),
                identity: "identity".into(),
                build_id: "build".into(),
                mode,
                financial_dispatch_started: Arc::new(AtomicBool::new(false)),
            };
            assert!(Transport::development_mode(&transport));
            assert_eq!(Transport::live_development_mode(&transport), live);
        }
    }
}
