//! fin-dev socket client used by the isolated Coinbase withdrawal rehearsal.
use gofin_domain::OperationRequest;
use gofin_protocol::BrokerRequest;
use serde_json::{Value, json};
use std::sync::Arc;

pub const IDENTITY: &str = "synthetic-coinbase-withdrawal";

pub struct Client {
    pub runtime: Arc<gofin_broker::BrokerRuntime>,
    socket: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Client {
    pub async fn new(runtime: Arc<gofin_broker::BrokerRuntime>, dir: &std::path::Path) -> Self {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let socket = dir.join("coinbase-withdrawal.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        runtime.engine.recover_interrupted().unwrap();
        let handle = runtime.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let runtime = handle.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let hello: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(hello["identity"], IDENTITY);
                    let reply = json!({"kind":"hello","identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic"});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    let input: Value = serde_json::from_str(&line).unwrap();
                    let request: BrokerRequest =
                        serde_json::from_value(input["request"].clone()).unwrap();
                    let response = gofin_protocol::progress::observe(
                        reader.get_mut(),
                        input["progress"].as_bool().unwrap_or(false),
                        runtime.handle(request),
                    )
                    .await;
                    let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
                    reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                });
            }
        });
        Self {
            runtime,
            socket,
            server,
        }
    }

    pub async fn execute(&self, input: Value, success: bool) -> Value {
        let request_id = input["requestId"].as_str().unwrap().to_owned();
        let operation = OperationRequest {
            request_id: request_id.clone(),
            operation: "coinbase.withdraw".into(),
            input: input.clone(),
            run_id: None,
        };
        self.call(
            vec![
                "execute".into(),
                "coinbase.withdraw".into(),
                "--input".into(),
                input.to_string(),
                "--id".into(),
                request_id,
                "--json".into(),
            ],
            BrokerRequest::Execute { request: operation },
            success,
        )
        .await
    }

    pub async fn call(&self, args: Vec<String>, request: BrokerRequest, success: bool) -> Value {
        if let Ok(binary) = std::env::var("GOFIN_COINBASE_PLAN_CLI") {
            let socket = self.socket.clone();
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(binary)
                    .args([
                        "--identity",
                        IDENTITY,
                        "--build-id",
                        "synthetic-coinbase-build",
                    ])
                    .args(args)
                    .env("GOFIN_DEV_IDENTITY", IDENTITY)
                    .env("GOFIN_DEV_BUILD_ID", "synthetic-coinbase-build")
                    .env("GOFIN_DEV_SOCKET", socket)
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(
                output.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(output.status.success(), success, "{value}");
            value
        } else {
            let response = self.runtime.handle(request).await;
            assert_eq!(response.ok, success, "{:?}", response.error);
            if success {
                response.result.unwrap()
            } else {
                serde_json::to_value(response.error.unwrap()).unwrap()
            }
        }
    }

    pub async fn status(&self, id: &str) -> Value {
        self.call(
            vec!["status".into(), id.into(), "--json".into()],
            BrokerRequest::Status { id: id.into() },
            true,
        )
        .await
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}
