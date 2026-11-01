//! Optional real CLI transport for quote-expiry rehearsals.
use super::*;
use gofin_protocol::BrokerRequest;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

pub(super) async fn execute(
    runtime: &Arc<gofin_broker::BrokerRuntime>,
    request: BrokerRequest,
    directory: &std::path::Path,
) -> Value {
    let Ok(binary) = std::env::var("GOFIN_JUPITER_REFRESH_CLI") else {
        let response = runtime.handle(request).await;
        assert!(response.ok, "{:?}", response.error);
        return response.result.unwrap();
    };
    let BrokerRequest::Execute { request } = request else {
        panic!("expected execution")
    };
    let socket = directory.join("r.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let runtime = runtime.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(hello["identity"], "synthetic-jupiter-refresh");
        assert_eq!(hello["build_id"], "synthetic-jupiter-refresh-build");
        let reply = json!({"kind":"hello","identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic"});
        reader
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let envelope: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(envelope["cancel_on_disconnect"], true);
        let request = serde_json::from_value(envelope["request"].clone()).unwrap();
        let response = gofin_protocol::progress::observe_financial_request(
            reader.get_mut(),
            envelope["progress"].as_bool().unwrap_or(false),
            false,
            Box::pin(runtime.handle_client_request(request, false)),
        )
        .await;
        let reply = json!({"identity":hello["identity"],"build_id":hello["build_id"],"mode":"synthetic","response":response});
        reader
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
    });
    let socket_arg = socket.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .args([
                "--identity",
                "synthetic-jupiter-refresh",
                "--build-id",
                "synthetic-jupiter-refresh-build",
                "--json",
                "execute",
                &request.operation,
                "--id",
                &request.request_id,
                "--input",
                &request.input.to_string(),
            ])
            .env("GOFIN_DEV_IDENTITY", "synthetic-jupiter-refresh")
            .env("GOFIN_DEV_BUILD_ID", "synthetic-jupiter-refresh-build")
            .env("GOFIN_DEV_SOCKET", socket_arg)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    server.await.unwrap();
    let _ = std::fs::remove_file(socket);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{result}");
    result
}
