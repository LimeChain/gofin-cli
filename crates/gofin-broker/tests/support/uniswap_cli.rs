// The exact public local socket protocol used by fin-dev; the runtime and
// venue are synthetic and isolated to the temporary test database.
struct Client {
    runtime: Arc<gofin_broker::BrokerRuntime>,
    socket: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}
impl Client {
    async fn new(runtime: Arc<gofin_broker::BrokerRuntime>, dir: &std::path::Path) -> Self {
        Self::with_reporting_barrier(runtime, dir, None).await
    }
    async fn with_reporting_barrier(runtime: Arc<gofin_broker::BrokerRuntime>, dir: &std::path::Path, barrier: Option<Arc<(AtomicBool, AtomicBool)>>) -> Self {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::UnixListener,
        };
        let socket = dir.join("uniswap-api.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        runtime.engine.recover_interrupted().unwrap();
        let handle = runtime.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let runtime = handle.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let hello: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(hello["identity"], CLIENT_IDENTITY);
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
                    let financial = matches!(request, BrokerRequest::Execute { .. }
                        | BrokerRequest::ContinueOperation { .. } | BrokerRequest::ReauthorizeOperation { .. });
                    let response = if financial {
                        gofin_protocol::progress::observe_financial_request(
                            reader.get_mut(), input["progress"].as_bool().unwrap_or(false), false,
                            Box::pin(runtime.handle(request)),
                        ).await
                    } else {
                        gofin_protocol::progress::observe(reader.get_mut(),
                            input["progress"].as_bool().unwrap_or(false), runtime.handle(request)).await
                    };
                    if response.result.as_ref().is_some_and(|value| value["status"] == "completed")
                        && let Some(barrier) = barrier {
                        barrier.0.store(true, Ordering::SeqCst);
                        while !barrier.1.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }
                    let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
                    let _ = reader
                        .get_mut()
                        .write_all(format!("{reply}\n").as_bytes())
                        .await;
                });
            }
        });
        Self {
            runtime,
            socket,
            server,
        }
    }
    async fn call(&self, args: Vec<String>, request: BrokerRequest, ok: bool) -> Value {
        if let Ok(binary) = std::env::var("GOFIN_UNISWAP_CLI") {
            let socket = self.socket.clone();
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(binary)
                    .args([
                        "--identity",
                        CLIENT_IDENTITY,
                        "--build-id",
                        "synthetic-uniswap-build",
                    ])
                    .args(args)
                    .env("GOFIN_DEV_IDENTITY", CLIENT_IDENTITY)
                    .env("GOFIN_DEV_BUILD_ID", "synthetic-uniswap-build")
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
            if ok {
                value
            } else {
                assert_eq!(value["ok"], false, "{value}");
                assert!(value["error"].is_object(), "{value}");
                value["error"].clone()
            }
        } else {
            // Keep the unused direct-call fallback out of every public-CLI
            // call's stack frame; the broker's async router is substantial.
            let response = Box::pin(self.runtime.handle(request)).await;
            assert_eq!(response.ok, ok, "{:?}", response.error);
            if ok {
                response.result.unwrap()
            } else {
                serde_json::to_value(response.error.unwrap()).unwrap()
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
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}
