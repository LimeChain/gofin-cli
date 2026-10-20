//! Loopback HTTP around the existing stateful RPC fixture, using the real pool.
use async_trait::async_trait;
use gofin_evm::{EvmRpc, HttpRpc, RpcReadRequirements};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

pub struct Pool {
    pub rpc: Arc<dyn EvmRpc>,
    pub armed: Arc<AtomicBool>,
    pub rejected: Arc<AtomicUsize>,
    pub fallback: Arc<AtomicUsize>,
    servers: Vec<JoinHandle<()>>,
}

struct CodeReads {
    direct: Arc<dyn EvmRpc>,
    http: Arc<HttpRpc>,
    armed: Arc<AtomicBool>,
}
#[async_trait]
impl EvmRpc for CodeReads {
    fn diagnostics(&self) -> Value {
        self.http.diagnostics()
    }
    async fn request(&self, method: &str, params: Value) -> gofin_domain::Result<Value> {
        if method == "eth_getCode" && self.armed.load(Ordering::SeqCst) {
            self.http.request(method, params).await
        } else {
            self.direct.request(method, params).await
        }
    }
    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> gofin_domain::Result<Value> {
        if method == "eth_getCode" && self.armed.load(Ordering::SeqCst) {
            self.http
                .request_checked(method, params, requirements)
                .await
        } else {
            self.direct
                .request_checked(method, params, requirements)
                .await
        }
    }
}

impl Pool {
    pub async fn new(source: Arc<dyn EvmRpc>) -> Self {
        let armed = Arc::new(AtomicBool::new(false));
        let rejected = Arc::new(AtomicUsize::new(0));
        let fallback = Arc::new(AtomicUsize::new(0));
        let mut urls = Vec::new();
        let mut servers = Vec::new();
        for primary in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            urls.push(format!("http://{}", listener.local_addr().unwrap()));
            let source = source.clone();
            let armed = armed.clone();
            let rejected = rejected.clone();
            let fallback = fallback.clone();
            servers.push(tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            let source = source.clone();
                            let armed = armed.clone();
                            let rejected = rejected.clone();
                            let fallback = fallback.clone();
                            connections.spawn(async move {
                                let mut reader = BufReader::new(stream);
                                let mut size = 0;
                                loop {
                                    let mut line = String::new();
                                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                                        return;
                                    }
                                    if line == "\r\n" { break; }
                                    if let Some((name, value)) = line.split_once(':')
                                        && name.eq_ignore_ascii_case("content-length")
                                    {
                                        size = value.trim().parse::<usize>().unwrap();
                                    }
                                }
                                assert!(size > 0 && size < 1_000_000);
                                let mut body = vec![0; size];
                                if reader.read_exact(&mut body).await.is_err() { return; }
                                let request: Value = serde_json::from_slice(&body).unwrap();
                                let method = request["method"].as_str().unwrap();
                                let code_failure = method == "eth_getCode" && armed.load(Ordering::SeqCst);
                                let response = if primary && code_failure {
                                    rejected.fetch_add(1, Ordering::SeqCst);
                                    json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"controlled code backend unavailable"}})
                                } else {
                                    if code_failure {
                                        // Ensure the primary rejection arrives while this valid
                                        // already-dispatched read is still pending.
                                        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                                    }
                                    let value = source.request(method, request["params"].clone()).await.unwrap();
                                    if code_failure {
                                        fallback.fetch_add(1, Ordering::SeqCst);
                                    }
                                    json!({"jsonrpc":"2.0","id":request["id"],"result":value})
                                };
                                let body = response.to_string();
                                let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                // Losing read candidates are deliberately cancelled by HttpRpc.
                                let _ = reader.get_mut().write_all(reply.as_bytes()).await;
                            });
                        }
                        result = connections.join_next(), if !connections.is_empty() => {
                            result.unwrap().unwrap();
                        }
                    }
                }
            }));
        }
        Self {
            // Only the code-read transport is under test. The existing synthetic
            // Aave fixture deliberately does not model immutable historic hashes
            // for every changing balance, so keep its other reads direct.
            rpc: Arc::new(CodeReads {
                direct: source,
                http: Arc::new(HttpRpc::isolated_localhost_pool(urls, 1).unwrap()),
                armed: armed.clone(),
            }),
            armed,
            rejected,
            fallback,
            servers,
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Dropping each server also drops its JoinSet, cancelling scoped reads.
        for server in &self.servers {
            server.abort();
        }
    }
}
