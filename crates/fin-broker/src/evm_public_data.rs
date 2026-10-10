//! Public candidates are discovery hints, never balance or completeness evidence.
//! Exact asset identities alone select market products; symbols from an indexer
//! or user input cannot establish a token's USD value.

use async_trait::async_trait;
use fin_domain::{FinError, Result};
use fin_ethereum::{TokenDiscovery, TokenDiscoveryResult};
use serde_json::Value;
use std::{collections::BTreeSet, time::Duration};

const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_CANDIDATES: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const INDEXER_WARNING: &str = "Public indexer candidates may omit unindexed or recently changed holdings; indexing freshness and exhaustive ERC-20 coverage are unproven. Balances require separate on-chain reads.";
#[cfg(test)]
const ARBITRUM_WETH: &str = "0x82af49447d8a07e3bd95bd0d56f35241523fbab1";
#[cfg(test)]
const ARBITRUM_USDC: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";

fn indexer_base(chain_id: u64) -> Result<&'static str> {
    match chain_id {
        1 => Ok("https://eth.blockscout.com"),
        42161 => Ok("https://arbitrum.blockscout.com"),
        8453 => Ok("https://base.blockscout.com"),
        4663 => Ok("https://robinhoodchain.blockscout.com"),
        _ => Err(FinError::new(
            "TOKEN_DISCOVERY_UNAVAILABLE",
            "discovery",
            "No reviewed public token indexer exists for this chain.",
            "Use an explicit token scope on a supported network.",
        )),
    }
}

fn normalized_address(value: &str) -> Option<String> {
    let digits = value.strip_prefix("0x")?;
    if digits.len() != 40
        || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        || digits.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn candidate_result(source: &str) -> TokenDiscoveryResult {
    TokenDiscoveryResult {
        candidates: Vec::new(),
        // Blockscout returns cached last-token balances, triggers asynchronous
        // refresh, and returns [] when the address is absent from its index.
        // https://github.com/blockscout/blockscout/blob/master/apps/block_scout_web/lib/block_scout_web/controllers/api/v2/address_controller.ex
        complete: false,
        source: format!("Blockscout public ERC-20 candidates: {source}"),
        warnings: vec![INDEXER_WARNING.into()],
    }
}

fn parse_candidates(bytes: &[u8], source: &str) -> TokenDiscoveryResult {
    let mut result = candidate_result(source);
    if bytes.len() > MAX_RESPONSE_BYTES {
        result.warnings.push(
            "Token discovery is unavailable: the public response exceeds the byte limit.".into(),
        );
        return result;
    }
    // The reviewed token-balances endpoint returns an array, not the paginated
    // /tokens endpoint. Never infer completeness from either [] or array length.
    // https://github.com/blockscout/blockscout/blob/master/apps/block_scout_web/lib/block_scout_web/views/api/v2/address_view.ex
    let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(bytes) else {
        result
            .warnings
            .push("Token discovery is unavailable: the public response is malformed or has an unsupported shape.".into());
        return result;
    };
    let mut addresses = BTreeSet::new();
    let mut malformed = false;
    let mut truncated = false;
    for item in items {
        let Some(token) = item.get("token").and_then(Value::as_object) else {
            malformed = true;
            continue;
        };
        match token.get("type").and_then(Value::as_str) {
            Some("ERC-20") => {}
            Some("ERC-721" | "ERC-1155" | "ERC-404") => continue,
            _ => {
                malformed = true;
                continue;
            }
        }
        if item.get("token_id").is_some_and(|value| !value.is_null()) {
            malformed = true;
            continue;
        }
        let Some(address) = token
            .get("address_hash")
            .and_then(Value::as_str)
            .and_then(normalized_address)
        else {
            malformed = true;
            continue;
        };
        if addresses.contains(&address) {
            continue;
        }
        if addresses.len() == MAX_CANDIDATES {
            truncated = true;
            continue;
        }
        // Exchange rates, symbols, decimals and quantities from the indexer are
        // deliberately unused; the EVM adapter verifies each candidate on chain.
        addresses.insert(address);
    }
    result.candidates = addresses.into_iter().collect();
    if malformed {
        result.warnings.push(
            "Token discovery is partial: malformed or unsupported token entries were omitted."
                .into(),
        );
    }
    if truncated {
        result.warnings.push(
            "Token discovery is partial: the candidate count exceeds the 64-token limit.".into(),
        );
    }
    result
}

#[derive(Clone, Copy)]
enum DiscoveryFailure {
    Unavailable,
    Oversized,
}

impl DiscoveryFailure {
    fn result(self, source: &str) -> TokenDiscoveryResult {
        let mut result = candidate_result(source);
        result.warnings.push(
            match self {
                Self::Unavailable => "Token discovery is unavailable: the public indexer failed, timed out, or rejected the request.",
                Self::Oversized => "Token discovery is unavailable: the public response exceeds the byte limit.",
            }
            .into(),
        );
        result
    }
}

pub struct PublicTokenDiscovery {
    client: reqwest::Client,
    #[cfg(test)]
    test_base_url: Option<String>,
    #[cfg(test)]
    test_timeout: Option<Duration>,
}

impl PublicTokenDiscovery {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("fin-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                FinError::new(
                    "TOKEN_DISCOVERY_UNAVAILABLE",
                    "configuration",
                    "Public token discovery transport could not be initialized.",
                    "Check broker networking, or use an explicit token scope.",
                )
            })?;
        Ok(Self {
            client,
            #[cfg(test)]
            test_base_url: None,
            #[cfg(test)]
            test_timeout: None,
        })
    }

    async fn fetch(&self, url: &str) -> std::result::Result<Vec<u8>, DiscoveryFailure> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| DiscoveryFailure::Unavailable)?;
        if !response.status().is_success() {
            return Err(DiscoveryFailure::Unavailable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(DiscoveryFailure::Oversized);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| DiscoveryFailure::Unavailable)?
        {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(DiscoveryFailure::Oversized);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[async_trait]
impl TokenDiscovery for PublicTokenDiscovery {
    async fn tokens(&self, chain_id: u64, address: &str) -> Result<TokenDiscoveryResult> {
        let source = indexer_base(chain_id)?;
        let address = normalized_address(address).ok_or_else(|| {
            FinError::validation("Public token discovery requires a nonzero exact EVM address.")
        })?;
        let base = source;
        #[cfg(test)]
        let base = self.test_base_url.as_deref().unwrap_or(base);
        let timeout = REQUEST_TIMEOUT;
        #[cfg(test)]
        let timeout = self.test_timeout.unwrap_or(timeout);
        let url = format!("{base}/api/v2/addresses/{address}/token-balances");
        // Covers connection, headers and the entire bounded body, including a
        // slow streaming response. No retries or alternate unreviewed origins.
        let outcome = tokio::time::timeout(timeout, self.fetch(&url)).await;
        Ok(match outcome {
            Ok(Ok(bytes)) => parse_candidates(&bytes, source),
            Ok(Err(failure)) => failure.result(source),
            Err(_) => DiscoveryFailure::Unavailable.result(source),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    const OWNER: &str = "0x1111111111111111111111111111111111111111";

    fn token(address: &str, kind: &str) -> Value {
        json!({"token":{"address_hash":address,"type":kind,"symbol":"untrusted","exchange_rate":"999999","decimals":"99"},"value":"999999","token_id":null})
    }

    fn parsed(value: &Value) -> TokenDiscoveryResult {
        parse_candidates(
            &serde_json::to_vec(value).unwrap(),
            "https://eth.blockscout.com",
        )
    }

    #[test]
    fn candidates_only_normalize_deduplicate_and_exclude_nfts() {
        let result = parsed(&json!([
            token(ARBITRUM_WETH, "ERC-20"),
            token(
                &format!("0x{}", ARBITRUM_WETH[2..].to_ascii_uppercase()),
                "ERC-20"
            ),
            token(OWNER, "ERC-721"),
            token(OWNER, "ERC-1155")
        ]));
        assert_eq!(result.candidates, vec![ARBITRUM_WETH]);
        assert!(!result.complete);
        assert_eq!(result.warnings, vec![INDEXER_WARNING]);
        assert!(!result.source.contains(OWNER));
    }

    #[test]
    fn malformed_entries_preserve_valid_candidates_and_safe_warnings() {
        let result = parsed(&json!([
            token(ARBITRUM_USDC, "ERC-20"),
            token("private-invalid-value", "ERC-20"),
            token("0x0000000000000000000000000000000000000000", "ERC-20"),
            {"token":{"address_hash":OWNER,"type":null}},
            {"token":{"address_hash":OWNER,"type":"ERC-20"},"token_id":"1"},
            {"private-response":"must-not-leak"}
        ]));
        assert_eq!(result.candidates, vec![ARBITRUM_USDC]);
        assert!(!result.complete);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("partial"))
        );
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(!rendered.contains("private-invalid-value"));
        assert!(!rendered.contains("must-not-leak"));
    }

    #[test]
    fn empty_indexer_result_never_claims_empty_wallet_or_complete_indexing() {
        let result = parsed(&json!([]));
        assert!(result.candidates.is_empty());
        assert!(!result.complete);
        assert!(result.warnings[0].contains("unindexed"));
        assert!(result.warnings[0].contains("freshness"));
    }

    #[test]
    fn count_and_byte_bounds_report_partial_or_unavailable() {
        let rows: Vec<_> = (1..=MAX_CANDIDATES + 3)
            .map(|number| token(&format!("0x{number:040x}"), "ERC-20"))
            .collect();
        let result = parsed(&json!(rows));
        assert_eq!(result.candidates.len(), MAX_CANDIDATES);
        assert!(!result.complete);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("64-token"))
        );
        let result = parse_candidates(&vec![b' '; MAX_RESPONSE_BYTES + 1], "fixture");
        assert!(result.candidates.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("byte limit"))
        );
    }

    #[test]
    fn malformed_truncated_or_paginated_shape_is_unavailable() {
        for bytes in [
            b"[{\"token\":".as_slice(),
            b"null",
            b"{\"items\":[],\"next_page_params\":{\"page\":2}}",
        ] {
            let result = parse_candidates(bytes, "fixture");
            assert!(result.candidates.is_empty());
            assert!(!result.complete);
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("unavailable"))
            );
        }
    }

    fn server(response: Vec<u8>, pause: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 2048];
            let _ = socket.read(&mut request);
            thread::sleep(pause);
            let _ = socket.write_all(&response);
        });
        url
    }

    async fn fixture_fetch(response: Vec<u8>) -> TokenDiscoveryResult {
        let mut provider = PublicTokenDiscovery::new().unwrap();
        provider.test_base_url = Some(server(response, Duration::ZERO));
        provider.tokens(1, OWNER).await.unwrap()
    }

    #[tokio::test]
    async fn actual_http_reader_handles_success_failure_redirect_and_short_body() {
        let body = serde_json::to_vec(&json!([token(ARBITRUM_WETH, "ERC-20")])).unwrap();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend(body);
        let result = fixture_fetch(response).await;
        assert_eq!(result.candidates, vec![ARBITRUM_WETH]);
        for response in [
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 302 Found\r\nLocation: https://unreviewed.invalid\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\n[]".to_vec(),
        ] {
            let result = fixture_fetch(response).await;
            assert!(result.candidates.is_empty());
            assert!(!result.complete);
            assert!(result.warnings.iter().any(|warning| warning.contains("unavailable")));
        }
    }

    #[tokio::test]
    async fn actual_http_reader_bounds_declared_and_streamed_body_sizes() {
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        chunked.extend(vec![b' '; MAX_RESPONSE_BYTES + 1]);
        chunked.extend(b"\r\n0\r\n\r\n");
        for response in [declared, chunked] {
            let result = fixture_fetch(response).await;
            assert!(result.candidates.is_empty());
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("byte limit"))
            );
        }
    }

    #[tokio::test]
    async fn actual_http_reader_has_total_timeout_and_safe_transport_failures() {
        let mut provider = PublicTokenDiscovery::new().unwrap();
        provider.test_base_url = Some(server(Vec::new(), Duration::from_millis(100)));
        provider.test_timeout = Some(Duration::from_millis(10));
        let result = provider.tokens(1, OWNER).await.unwrap();
        assert!(result.candidates.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("timed out"))
        );
        provider.test_base_url = Some("http://127.0.0.1:0".into());
        let result = provider.tokens(1, OWNER).await.unwrap();
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("unavailable"))
        );
    }

    #[tokio::test]
    async fn unsupported_chain_and_unsafe_address_fail_before_networking() {
        let provider = PublicTokenDiscovery::new().unwrap();
        assert!(provider.tokens(99, OWNER).await.is_err());
        for address in [
            "../secret",
            "0x0000000000000000000000000000000000000000",
            "0x1",
            "https://private.invalid",
        ] {
            assert!(provider.tokens(1, address).await.is_err());
        }
        assert_eq!(indexer_base(1).unwrap(), "https://eth.blockscout.com");
        assert_eq!(indexer_base(8453).unwrap(), "https://base.blockscout.com");
        assert_eq!(
            indexer_base(42161).unwrap(),
            "https://arbitrum.blockscout.com"
        );
        assert_eq!(
            indexer_base(4663).unwrap(),
            "https://robinhoodchain.blockscout.com"
        );
    }
}
