// Real protected transport for legacy address reads within the deposit rehearsal.
// Other venue calls retain the existing controlled fixture; no live endpoint is used.
use super::*;
use gofin_exchange::{
    ccxt::{CcxtError, CredentialProvider, Credentials},
    kraken::{KrakenClient, NonceAllocator},
};
use std::sync::atomic::AtomicU64;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Nonces(AtomicU64);
impl NonceAllocator for Nonces {
    fn allocate(&self, _: &str, clock_ms: u64) -> std::result::Result<u64, CcxtError> {
        let prior = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |prior| {
                Some(prior.saturating_add(1).max(clock_ms))
            })
            .unwrap();
        Ok(prior.saturating_add(1).max(clock_ms))
    }
}
struct Keys;
impl CredentialProvider for Keys {
    fn credentials(&self, reference: &str) -> std::result::Result<Credentials, CcxtError> {
        assert_eq!(reference, "synthetic-credential-only");
        Ok(Credentials {
            api_key: zeroize::Zeroizing::new("synthetic-loopback-only-key".into()),
            secret: zeroize::Zeroizing::new("c3ludGhldGljLWxvb3BiYWNrLW9ubHk=".into()),
        })
    }
}
struct Transport {
    venue: Arc<Venue>,
    wire: gofin_kraken::KrakenCcxtTransport,
}
#[async_trait]
impl KrakenTransport for Transport {
    async fn request(
        &self,
        endpoint: E,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if matches!(
            endpoint,
            E::KrakenPrivatePostDepositMethods | E::KrakenPrivatePostDepositAddresses
        ) {
            self.wire
                .request(endpoint, account, parameters, guard)
                .await
        } else {
            self.venue
                .request(endpoint, account, parameters, guard)
                .await
        }
    }
}
pub(super) struct Server {
    task: tokio::task::JoinHandle<()>,
    pub calls: Arc<Mutex<Vec<(E, u64)>>>,
}
impl Server {
    pub(super) fn assert_legacy_calls(&self, venue: &Venue) {
        let calls = self.calls.lock().unwrap();
        assert!(
            calls.len() >= 2,
            "The workflow must exercise both real HTTP legacy reads."
        );
        assert!(calls.windows(2).all(|pair| pair[0].1 < pair[1].1));
        let expected = venue
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(endpoint, _)| {
                matches!(
                    endpoint,
                    E::KrakenPrivatePostDepositMethods | E::KrakenPrivatePostDepositAddresses
                )
                .then_some(*endpoint)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls
                .iter()
                .map(|(endpoint, _)| *endpoint)
                .collect::<Vec<_>>(),
            expected
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub(super) async fn attach(adapter: &mut KrakenEvmDepositAdapter, venue: Arc<Venue>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let backend = venue.clone();
    let task = tokio::spawn(async move {
        // Scoped handlers are cancelled if the fixture ends before a response.
        let mut handlers = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let backend = backend.clone();
            let captured = captured.clone();
            handlers.spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request = line.split_whitespace().collect::<Vec<_>>();
                assert_eq!(request[0], "POST");
                let endpoint = match request[1] {
                    "/0/private/DepositMethods" => E::KrakenPrivatePostDepositMethods,
                    "/0/private/DepositAddresses" => E::KrakenPrivatePostDepositAddresses,
                    other => panic!("Unexpected loopback legacy endpoint: {other}"),
                };
                let mut headers = BTreeMap::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.split_once(':').unwrap();
                    headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
                }
                assert_eq!(headers["api-key"], "synthetic-loopback-only-key");
                assert!(!headers["api-sign"].is_empty());
                let length: usize = headers["content-length"].parse().unwrap();
                assert!(length < 4096);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let body = String::from_utf8(body).unwrap();
                let url = reqwest::Url::parse(&format!("http://fixture/?{body}")).unwrap();
                let mut parameters = serde_json::Map::new();
                for (key, value) in url.query_pairs() {
                    assert!(parameters.insert(key.into_owned(), json!(value)).is_none());
                }
                let nonce = parameters.remove("nonce").unwrap().as_str().unwrap().parse().unwrap();
                assert_eq!(parameters["asset"], "USDC");
                assert_eq!(parameters["aclass"], "currency");
                assert!(!parameters.contains_key("new"));
                assert!(!parameters.contains_key("account"));
                if endpoint == E::KrakenPrivatePostDepositAddresses {
                    assert_eq!(parameters["method"], "USDC - Ethereum (Unified)");
                }
                captured.lock().unwrap().push((endpoint, nonce));
                let response = backend.request(endpoint, None, Value::Object(parameters), None).await.unwrap();
                let KrakenResponse::Json(response) = response else { panic!("Expected JSON fixture") };
                let body = serde_json::to_vec(&response).unwrap();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                reader.get_mut().write_all(head.as_bytes()).await.unwrap();
                reader.get_mut().write_all(&body).await.unwrap();
            });
            while let Some(result) = handlers.try_join_next() {
                result.unwrap();
            }
        }
    });
    let wire = gofin_kraken::KrakenCcxtTransport::new(
        KrakenClient::loopback(Arc::new(Nonces(AtomicU64::new(1))), base).unwrap(),
        Arc::new(Keys),
    );
    adapter.kraken = Arc::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![KrakenAccount {
                name: "fixture-spot".into(),
                resource: ResourceId("kraken:fixture-spot".into()),
                credential_ref: "synthetic-credential-only".into(),
                physical_identity: "synthetic-uid-only".into(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: true,
                product: KrakenProduct::Spot,
            }],
            Arc::new(Transport { venue, wire }),
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    );
    Server { task, calls }
}
