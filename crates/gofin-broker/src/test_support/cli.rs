use gofin_protocol::BrokerRequest;
use serde_json::{Value, json};
use std::sync::Arc;

// Shared synthetic public CLI transport; never connects to an installed service.
pub struct Client {
    runtime: Arc<crate::BrokerRuntime>,
    socket: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Client {
    pub async fn new(runtime: Arc<crate::BrokerRuntime>, dir: &std::path::Path) -> Self {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let socket = dir.join("solana-funding.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        runtime.engine.recover_interrupted().unwrap();
        let r = runtime.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let runtime = r.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let hello: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(hello["identity"], "synthetic-solana-funding");
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
    pub async fn call(&self, args: Vec<String>, request: BrokerRequest, ok: bool) -> Value {
        if let Ok(binary) = std::env::var("GOFIN_SOLANA_FUNDING_CLI") {
            let socket = self.socket.clone();
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(binary)
                    .args([
                        "--identity",
                        "synthetic-solana-funding",
                        "--build-id",
                        "synthetic-solana-funding-build",
                    ])
                    .args(args)
                    .env("GOFIN_DEV_IDENTITY", "synthetic-solana-funding")
                    .env("GOFIN_DEV_BUILD_ID", "synthetic-solana-funding-build")
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
            assert_eq!(output.status.success(), ok, "{value}");
            value
        } else {
            let result = self.runtime.handle(request).await;
            assert_eq!(result.ok, ok, "{:?}", result.error);
            if ok {
                result.result.unwrap()
            } else {
                serde_json::to_value(result.error.unwrap()).unwrap()
            }
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}
pub fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}
