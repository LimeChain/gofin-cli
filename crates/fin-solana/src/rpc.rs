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
        }))
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
        let response = self
            .client
            .post(RPC_URL)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| transport_error())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(transport_error());
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| transport_error())?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(transport_error());
            }
            body.extend_from_slice(&chunk);
        }
        let envelope: Value = serde_json::from_slice(&body).map_err(|_| transport_error())?;
        if envelope.get("id").and_then(Value::as_u64) != Some(id) || envelope.get("error").is_some()
        {
            return Err(transport_error());
        }
        envelope.get("result").cloned().ok_or_else(transport_error)
    }
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
