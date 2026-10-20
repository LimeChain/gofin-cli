//! Real Coinbase request construction and HTTP dispatch over a synthetic venue.
use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use gofin_exchange::coinbase::*;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Pending {
    route: CoinbaseRoute,
    body: Option<String>,
    guard: Option<DispatchGuard>,
}
pub struct Capture {
    pub path: String,
    pub body: String,
    pub mutating: bool,
}
struct Api {
    wire: CoinbaseTransport,
    pending: Arc<Mutex<Vec<Pending>>>,
}
#[async_trait]
impl CoinbaseApi for Api {
    async fn request(
        &self,
        route: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        self.pending.lock().unwrap().push(Pending {
            route: route.clone(),
            body: request.body.clone(),
            guard: guard.clone(),
        });
        self.wire.request(route, request, guard).await
    }
}
pub struct Fixture {
    pub api: Arc<dyn CoinbaseApi>,
    pub calls: Arc<Mutex<Vec<Capture>>>,
    pending: Arc<Mutex<Vec<Pending>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    pub fn assert_dispatched(&self) {
        assert!(
            self.pending.lock().unwrap().is_empty(),
            "Every typed request must reach the fixture exactly once."
        );
        assert!(!self.calls.lock().unwrap().is_empty());
    }
    pub async fn new(backend: Arc<dyn CoinbaseApi>) -> Self {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[0x29; 32]);
        let mut key = signing.to_bytes().to_vec();
        key.extend_from_slice(signing.verifying_key().as_bytes());
        let verify = signing.verifying_key();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let pending = Arc::new(Mutex::new(Vec::<Pending>::new()));
        let waiting = pending.clone();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        let server = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let waiting = waiting.clone();
                let captured = captured.clone();
                let backend = backend.clone();
                handlers.spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    let parts = line.split_whitespace().collect::<Vec<_>>();
                    let method = parts[0].to_owned();
                    let path = parts[1].to_owned();
                    let mut headers = std::collections::BTreeMap::new();
                    loop {
                        line.clear();
                        reader.read_line(&mut line).await.unwrap();
                        if line == "\r\n" { break; }
                        let (name, value) = line.split_once(':').unwrap();
                        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
                    }
                    let length = headers.get("content-length").map(|v| v.parse::<usize>().unwrap()).unwrap_or(0);
                    assert!(length < 64 * 1024);
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).await.unwrap();
                    let body = String::from_utf8(bytes).unwrap();
                    let expected = {
                        let mut queue = waiting.lock().unwrap();
                        let index = queue.iter().position(|p| p.route.method() == method && p.route.path().unwrap() == path)
                            .unwrap_or_else(|| panic!("Unexpected constructed HTTP route {method} {path}"));
                        queue.remove(index)
                    };
                    assert_eq!(body, expected.body.as_deref().unwrap_or(""), "The reviewed body must reach HTTP without reserialization.");
                    if let Some(authorization) = headers.get("authorization") {
                    let jwt = authorization.strip_prefix("Bearer ").unwrap();
                    let parts = jwt.split('.').collect::<Vec<_>>();
                    assert_eq!(parts.len(), 3);
                    let signature = ed25519_dalek::Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
                    verify.verify_strict(format!("{}.{}",parts[0],parts[1]).as_bytes(), &signature).unwrap();
                    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
                    assert_eq!(claims["uri"], format!("{method} api.coinbase.com{}", path.split('?').next().unwrap()));
                    assert_eq!(claims["sub"], "organizations/synthetic/apiKeys/loopback");
                    } else {
                        assert!(matches!(expected.route, CoinbaseRoute::Product { .. } | CoinbaseRoute::Products { .. } | CoinbaseRoute::ProductBook { .. }), "Private Coinbase routes require authentication.");
                        assert_eq!(method, "GET");
                        assert!(path.starts_with("/api/v3/brokerage/market/"));
                    }
                    captured.lock().unwrap().push(Capture { path, body: body.clone(), mutating: expected.route.mutating() });
                    // An accepted request followed by transport loss closes this
                    // socket; the real client must reconcile, never replay it.
                    let Ok(response) = backend.request(expected.route, CoinbaseRequest { body: expected.body }, expected.guard).await else { return };
                    let head = format!("HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.status, response.body.len());
                    reader.get_mut().write_all(head.as_bytes()).await.unwrap();
                    reader.get_mut().write_all(&response.body).await.unwrap();
                });
                while let Some(result) = handlers.try_join_next() {
                    result.unwrap();
                }
            }
        });
        let api = Arc::new(Api {
            wire: CoinbaseTransport::loopback(
                CoinbaseAuth::CdpEd25519 {
                    api_key: "organizations/synthetic/apiKeys/loopback".into(),
                    private_key_base64: STANDARD.encode(key),
                },
                base,
            )
            .unwrap(),
            pending: pending.clone(),
        });
        Self {
            api,
            calls,
            pending,
            server,
        }
    }
}
