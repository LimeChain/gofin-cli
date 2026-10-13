use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, redirect::Policy};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::{MAINNET_GENESIS_HASH, failure};
use fin_domain::{FinError, Result};

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ATTEMPTS: usize = 3;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

#[async_trait]
pub trait SolanaRpc: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value>;
}

/// Fixed, reviewed mainnet-beta JSON-RPC transport. Redirects are disabled,
/// response bodies are bounded, and errors never include endpoint/body data.
pub struct HttpRpc {
    client: Client,
    verified: OnceCell<()>,
    next_id: AtomicU64,
    endpoint: String,
}

impl HttpRpc {
    pub fn mainnet_beta() -> Result<std::sync::Arc<Self>> {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| transport_error())?;
        Ok(std::sync::Arc::new(Self {
            client,
            verified: OnceCell::new(),
            next_id: AtomicU64::new(1),
            endpoint: RPC_URL.to_owned(),
        }))
    }

    #[cfg(test)]
    fn test_endpoint(endpoint: String) -> Self {
        Self {
            client: Client::builder()
                .redirect(Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("test HTTP client"),
            verified: OnceCell::new(),
            next_id: AtomicU64::new(1),
            endpoint,
        }
    }

    async fn verify_network(&self) -> Result<()> {
        self.verified
            .get_or_try_init(|| async {
                let genesis = self.post("getGenesisHash", json!([])).await?;
                verify_mainnet_genesis(&genesis)?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn post(&self, method: &str, params: Value) -> Result<Value> {
        if method.is_empty()
            || method.len() > 80
            || !method
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(FinError::validation(
                "The requested Solana RPC method is invalid.",
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let retryable_method = is_retryable_method(method);
        let attempt_timeout = attempt_timeout_for(method);
        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        for attempt in 1..=MAX_ATTEMPTS {
            let attempt_deadline =
                std::cmp::min(deadline, tokio::time::Instant::now() + attempt_timeout);
            let result = tokio::time::timeout_at(attempt_deadline, self.post_once(&body, id)).await;
            let failure = match result {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(failure)) => failure,
                Err(_) => RpcFailure {
                    error: rpc_error(
                        "The Solana mainnet-beta RPC request timed out.",
                        "transport_timeout",
                        None,
                        None,
                    ),
                    retryable: true,
                    retry_after: None,
                },
            };
            if !retryable_method || !failure.retryable || attempt == MAX_ATTEMPTS {
                return Err(failure.error);
            }
            let delay = failure.retry_after.unwrap_or_else(|| match attempt {
                1 => Duration::from_secs(1),
                _ => Duration::from_secs(2),
            });
            let now = tokio::time::Instant::now();
            if now >= deadline || delay >= deadline.saturating_duration_since(now) {
                let mut error = failure.error;
                if let Some(details) = error.details.as_deref_mut() {
                    details["retry_deadline_exceeded"] = json!(true);
                } else {
                    error.details = Some(Box::new(
                        json!({"stage":"retry_deadline","retry_deadline_exceeded":true}),
                    ));
                }
                return Err(error);
            }
            tokio::time::sleep(delay).await;
        }
        unreachable!("bounded RPC retry loop always returns")
    }

    async fn post_once(
        &self,
        request_body: &Value,
        id: u64,
    ) -> std::result::Result<Value, RpcFailure> {
        let response = self
            .client
            .post(&self.endpoint)
            .json(request_body)
            .send()
            .await
            .map_err(|error| RpcFailure {
                error: rpc_error(
                    "The Solana mainnet-beta RPC transport failed.",
                    "transport",
                    None,
                    None,
                ),
                retryable: error.is_connect() || error.is_timeout(),
                retry_after: None,
            })?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_seconds);
            return Err(RpcFailure {
                error: rpc_error(
                    "The Solana mainnet-beta RPC returned an HTTP error.",
                    "http_status",
                    Some(status.as_u16()),
                    None,
                ),
                retryable: matches!(status.as_u16(), 429 | 502 | 503 | 504),
                retry_after,
            });
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(RpcFailure::permanent(
                "The Solana mainnet-beta RPC response exceeded its size limit.",
                "response_too_large",
                Some(status.as_u16()),
                None,
            ));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| RpcFailure {
                error: rpc_error(
                    "The Solana mainnet-beta RPC response could not be read.",
                    "body_read",
                    Some(status.as_u16()),
                    None,
                ),
                retryable: error.is_timeout() || error.is_connect(),
                retry_after: None,
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(RpcFailure::permanent(
                    "The Solana mainnet-beta RPC response exceeded its size limit.",
                    "response_too_large",
                    Some(status.as_u16()),
                    None,
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let envelope: Value = serde_json::from_slice(&body).map_err(|_| {
            RpcFailure::permanent(
                "The Solana mainnet-beta RPC returned malformed JSON.",
                "json",
                Some(status.as_u16()),
                None,
            )
        })?;
        if envelope.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(RpcFailure::permanent(
                "The Solana mainnet-beta RPC response did not match its request.",
                "id_mismatch",
                Some(status.as_u16()),
                None,
            ));
        }
        if let Some(error) = envelope.get("error") {
            let code = error.get("code").and_then(Value::as_i64);
            return Err(RpcFailure::permanent(
                "The Solana mainnet-beta RPC returned an application error.",
                "rpc_error",
                Some(status.as_u16()),
                code,
            ));
        }
        envelope.get("result").cloned().ok_or_else(|| {
            RpcFailure::permanent(
                "The Solana mainnet-beta RPC response omitted its result.",
                "missing_result",
                Some(status.as_u16()),
                None,
            )
        })
    }
}

struct RpcFailure {
    error: FinError,
    retryable: bool,
    retry_after: Option<Duration>,
}

impl RpcFailure {
    fn permanent(
        message: &'static str,
        stage: &'static str,
        http_status: Option<u16>,
        rpc_code: Option<i64>,
    ) -> Self {
        Self {
            error: rpc_error(message, stage, http_status, rpc_code),
            retryable: false,
            retry_after: None,
        }
    }
}

fn rpc_error(
    message: &'static str,
    stage: &'static str,
    http_status: Option<u16>,
    rpc_code: Option<i64>,
) -> FinError {
    let mut details = json!({"stage": stage});
    if let Some(status) = http_status {
        details["http_status"] = json!(status);
    }
    if let Some(code) = rpc_code {
        details["rpc_code"] = json!(code);
    }
    failure(
        "SOLANA_RPC_UNAVAILABLE",
        "rpc",
        message,
        "Retry the read; do not repeat a financial submission whose outcome is uncertain.",
    )
    .with_details(details)
}

fn parse_retry_after_seconds(value: &str) -> Option<Duration> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().map(Duration::from_secs)
}

fn attempt_timeout_for(method: &str) -> Duration {
    if is_retryable_method(method) {
        ATTEMPT_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    }
}

fn is_retryable_method(method: &str) -> bool {
    matches!(
        method,
        "getAccountInfo"
            | "getBalance"
            | "getBlock"
            | "getBlockHeight"
            | "getBlocks"
            | "getBlockTime"
            | "getClusterNodes"
            | "getEpochInfo"
            | "getEpochSchedule"
            | "getFeeForMessage"
            | "getFirstAvailableBlock"
            | "getGenesisHash"
            | "getHealth"
            | "getIdentity"
            | "getInflationGovernor"
            | "getInflationRate"
            | "getLargestAccounts"
            | "getLatestBlockhash"
            | "getLeaderSchedule"
            | "getMinimumBalanceForRentExemption"
            | "getMultipleAccounts"
            | "getProgramAccounts"
            | "getRecentPerformanceSamples"
            | "getRecentPrioritizationFees"
            | "getSignaturesForAddress"
            | "getSignatureStatuses"
            | "getSlot"
            | "getSlotLeader"
            | "getStakeMinimumDelegation"
            | "getSupply"
            | "getTokenAccountBalance"
            | "getTokenAccountsByDelegate"
            | "getTokenAccountsByOwner"
            | "getTokenLargestAccounts"
            | "getTransaction"
            | "getVersion"
            | "getVoteAccounts"
            | "isBlockhashValid"
            | "minimumLedgerSlot"
            | "simulateTransaction"
    )
}

fn verify_mainnet_genesis(genesis: &Value) -> Result<()> {
    if genesis.as_str() != Some(MAINNET_GENESIS_HASH) {
        return Err(failure(
            "SOLANA_NETWORK_MISMATCH",
            "rpc",
            "The RPC endpoint is not Solana mainnet-beta.",
            "Use the reviewed mainnet-beta Solana RPC service.",
        ));
    }
    Ok(())
}

#[async_trait]
impl SolanaRpc for HttpRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.verify_network().await?;
        self.post(method, params).await
    }
}

fn transport_error() -> FinError {
    failure(
        "SOLANA_RPC_UNAVAILABLE",
        "rpc",
        "The Solana mainnet-beta RPC request failed or returned an invalid response.",
        "Retry the read; do not repeat a financial submission whose outcome is uncertain.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn rpc_response(id: u64, result: Value) -> String {
        json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
    }

    #[tokio::test]
    async fn transient_read_retry_reuses_the_same_id_and_params() {
        let (url, requests) = mock_read_retry().await;
        let rpc = HttpRpc::test_endpoint(url);
        let result = rpc
            .post(
                "getBalance",
                json!(["synthetic-address", {"commitment":"finalized"}]),
            )
            .await
            .unwrap();
        assert_eq!(result, json!({"value":42}));
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0]["method"], "getBalance");
        assert_eq!(
            requests[0]["params"],
            json!(["synthetic-address", {"commitment":"finalized"}])
        );
    }

    async fn mock_read_retry() -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_json(&mut stream).await;
                let id = request["id"].as_u64().unwrap();
                requests.push(request);
                let (status, body) = if attempt == 0 {
                    (503, "temporarily unavailable".to_owned())
                } else {
                    (200, rpc_response(id, json!({"value":42})))
                };
                write_http_response(&mut stream, status, None, &body).await;
            }
            requests
        });
        (format!("http://{address}/rpc"), task)
    }

    async fn read_http_json(stream: &mut tokio::net::TcpStream) -> Value {
        let mut bytes = Vec::new();
        let mut chunk = [0; 2048];
        loop {
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "client closed before request body completed");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let body_start = position + 4;
                let headers = String::from_utf8_lossy(&bytes[..position]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= body_start + content_length {
                    return serde_json::from_slice(&bytes[body_start..body_start + content_length])
                        .unwrap();
                }
            }
        }
    }

    async fn write_http_response(
        stream: &mut tokio::net::TcpStream,
        status: u16,
        retry_after: Option<&str>,
        body: &str,
    ) {
        let reason = match status {
            200 => "OK",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Bad Gateway",
        };
        let retry = retry_after
            .map(|value| format!("Retry-After: {value}\r\n"))
            .unwrap_or_default();
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{retry}Connection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    async fn mock_single_response(
        status: u16,
        retry_after: Option<&'static str>,
        body: String,
    ) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_json(&mut stream).await;
            write_http_response(&mut stream, status, retry_after, &body).await;
            vec![request]
        });
        (format!("http://{address}/rpc"), task)
    }

    #[tokio::test]
    async fn send_transaction_and_protocol_errors_are_never_retried() {
        let (url, server) = mock_single_response(503, None, "unavailable".into()).await;
        let rpc = HttpRpc::test_endpoint(url);
        let error = rpc
            .post("sendTransaction", json!(["synthetic-signed-wire"]))
            .await
            .unwrap_err();
        assert_eq!(error.code, "SOLANA_RPC_UNAVAILABLE");
        assert_eq!(error.details.as_deref().unwrap()["http_status"], 503);
        assert_eq!(server.await.unwrap().len(), 1);

        let (url, server) = mock_single_response(200, None, "not json".into()).await;
        let rpc = HttpRpc::test_endpoint(url);
        let error = rpc.post("getSlot", json!([])).await.unwrap_err();
        assert_eq!(error.details.as_deref().unwrap()["stage"], "json");
        assert_eq!(server.await.unwrap().len(), 1);

        let (url, server) = mock_single_response(
            200,
            None,
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32004,"message":"private-ish"}})
                .to_string(),
        )
        .await;
        let rpc = HttpRpc::test_endpoint(url);
        let error = rpc.post("getSlot", json!([])).await.unwrap_err();
        let details = error.details.as_deref().unwrap();
        assert_eq!(details["stage"], "rpc_error");
        assert_eq!(details["rpc_code"], -32004);
        assert!(!error.message.contains("private-ish"));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retry_after_beyond_deadline_fails_without_sleep_or_second_dispatch() {
        let (url, server) = mock_single_response(429, Some("21"), "rate limited".into()).await;
        let rpc = HttpRpc::test_endpoint(url);
        let started = tokio::time::Instant::now();
        let error = rpc.post("getSlot", json!([])).await.unwrap_err();
        let details = error.details.as_deref().unwrap();
        assert_eq!(details["stage"], "http_status");
        assert_eq!(details["http_status"], 429);
        assert_eq!(details["retry_deadline_exceeded"], true);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[test]
    fn only_retryable_methods_use_short_per_attempt_budget() {
        assert_eq!(attempt_timeout_for("getAccountInfo"), ATTEMPT_TIMEOUT);
        assert_eq!(attempt_timeout_for("simulateTransaction"), ATTEMPT_TIMEOUT);
        assert_eq!(attempt_timeout_for("sendTransaction"), REQUEST_TIMEOUT);
        assert_eq!(attempt_timeout_for("unknownMutation"), REQUEST_TIMEOUT);
    }

    #[test]
    fn mainnet_genesis_uses_the_full_canonical_hash() {
        assert_eq!(
            MAINNET_GENESIS_HASH,
            "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d"
        );
        assert!(verify_mainnet_genesis(&json!(MAINNET_GENESIS_HASH)).is_ok());
        assert_eq!(
            verify_mainnet_genesis(&json!("5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"))
                .unwrap_err()
                .code,
            "SOLANA_NETWORK_MISMATCH"
        );
        assert_eq!(
            verify_mainnet_genesis(&json!("4sGjMW1sUnHzSxGspuhpqLDx6wiyjNtZ"))
                .unwrap_err()
                .code,
            "SOLANA_NETWORK_MISMATCH"
        );
    }
}
