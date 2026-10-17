//! Strict read-side transport for the Papertrade primary API.
//!
//! This module deliberately does not infer settlement from API queue state.
//! Writes are handled by the broker runtime after durable intent journaling,
//! and finality comes from canonical chain evidence.

use async_trait::async_trait;
use gofin_domain::{GoFinError, Result};
use gofin_papertrade::{
    BoundIntent, CoreDepositNonceRecoveryBinding, CoreDepositNonceRecoveryProgress,
    CoreDepositNonceRecoveryProof, CoreDepositNonceRecoveryStatus, FundingBinding,
    FundingContinuationBinding, FundingIntent, FundingProgress, IntentRequest, PapertradeAccount,
    PapertradeRuntime, PapertradeTypedIntent, ReadRequest, Receipt,
};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BATCH_EXECUTOR: &str = gofin_papertrade::BATCH_EXECUTOR;
const CALL_EXECUTED_TOPIC: &str =
    "0x8a23ef1120f3e42e722a0c0a599a66d55ab2650ecc39716f57e625684e349cad";
const CALL_REJECTED_TOPIC: &str =
    "0x6002fcbae88fcbb8133c69c91df90bbc0b4464af94df2a56f8bb843f1cc5a193";
const CALL_SKIPPED_TOPIC: &str =
    "0xaaf3cbad9087f9956efae61847c185478efbe673d0f8591fdaf1eb97a897aa7c";
const LIQUIDATED_TOPIC: &str = "0x0c859bc5db091ba3b9dc10c00f8dfc78b1f0b7e2566d107253ae67c069ae5de8";
const SESSION_DEADLINE_SAFETY_SECONDS: u64 = 30;
const SIGNED_INTENT_TTL_SECONDS: u64 = 5 * 60;
const CANONICAL_LOG_PAGE_DELAY: Duration = Duration::from_millis(1_200);
const CANONICAL_LOG_RATE_RETRIES: usize = 3;
const CANONICAL_LOG_MAX_RATE_RETRY_DELAY: Duration = Duration::from_secs(60);
// HyperEVM documents eth_getLogs requests in 50-block windows. Keep each
// observer below the published 100-RPC/minute budget even when the HTTP
// transport races two configured read endpoints.
const CANONICAL_LOG_PAGE_BLOCKS: u64 = 50;
const EIP1967_IMPLEMENTATION_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

/// Read-only canonical observer for the Papertrade relayer's batch executor.
/// It discovers transactions from the durable intent ID event topic, so an
/// accepted API response without a tx hash can still be reconciled safely.
pub(crate) struct PapertradeChainObserver {
    rpc: std::sync::Arc<dyn gofin_evm::EvmRpc>,
    runtime: std::sync::Arc<gofin_evm::EvmRuntime>,
    confirmations: u64,
    require_finalized: bool,
    page_delay: Duration,
}

impl PapertradeChainObserver {
    pub(crate) fn new(
        rpc: std::sync::Arc<dyn gofin_evm::EvmRpc>,
        runtime: std::sync::Arc<gofin_evm::EvmRuntime>,
        confirmations: u64,
        require_finalized: bool,
    ) -> Result<Self> {
        if confirmations == 0 {
            return Err(GoFinError::validation(
                "Papertrade canonical observation requires a confirmation rule.",
            ));
        }
        Ok(Self {
            rpc,
            runtime,
            confirmations,
            require_finalized,
            page_delay: CANONICAL_LOG_PAGE_DELAY,
        })
    }

    pub(crate) async fn by_intent(
        &self,
        intent_id: &str,
        from_block: u64,
    ) -> Result<Option<gofin_papertrade::CanonicalTransaction>> {
        if intent_id.len() != 66
            || !intent_id.starts_with("0x")
            || !intent_id[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(GoFinError::validation("Papertrade intent ID is invalid."));
        }
        let head = self.rpc.request("eth_blockNumber", json!([])).await?;
        let head = rpc_hex_u64(&head)?;
        if from_block > head {
            return Ok(None);
        }
        let mut cursor = from_block;
        let mut candidates = std::collections::BTreeSet::new();
        while cursor <= head {
            let end = cursor
                .saturating_add(CANONICAL_LOG_PAGE_BLOCKS - 1)
                .min(head);
            let logs = self
                .get_logs(json!({
                    "address": BATCH_EXECUTOR,
                    "fromBlock": format!("0x{cursor:x}"),
                    "toBlock": format!("0x{end:x}"),
                    "topics": [[CALL_EXECUTED_TOPIC, CALL_REJECTED_TOPIC, CALL_SKIPPED_TOPIC], null, null, intent_id.to_ascii_lowercase()]
                }))
                .await?;
            let rows = logs.as_array().ok_or_else(|| {
                invalid_response("HyperEVM returned malformed Papertrade intent logs.")
            })?;
            for row in rows {
                if row
                    .get("address")
                    .and_then(Value::as_str)
                    .is_none_or(|address| !address.eq_ignore_ascii_case(BATCH_EXECUTOR))
                {
                    return Err(invalid_response("BatchExecutor log address did not match."));
                }
                let topics = row
                    .get("topics")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid_response("BatchExecutor intent log omitted topics."))?;
                if topics.len() != 4
                    || topics[0].as_str().is_none_or(|topic| {
                        ![CALL_EXECUTED_TOPIC, CALL_REJECTED_TOPIC, CALL_SKIPPED_TOPIC]
                            .iter()
                            .any(|expected| topic.eq_ignore_ascii_case(expected))
                    })
                    || topics[3]
                        .as_str()
                        .is_none_or(|topic| !topic.eq_ignore_ascii_case(intent_id))
                {
                    return Err(invalid_response(
                        "BatchExecutor intent log did not bind the exact intent ID.",
                    ));
                }
                let hash = row
                    .get("transactionHash")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        invalid_response("BatchExecutor intent log omitted its transaction hash.")
                    })?;
                candidates.insert(hash.to_ascii_lowercase());
            }
            if end == head {
                break;
            }
            tokio::time::sleep(self.page_delay).await;
            cursor = end.saturating_add(1);
        }
        if candidates.len() > 1 {
            return Err(GoFinError::new(
                "PAPERTRADE_INTENT_AMBIGUOUS",
                "reconciliation",
                "More than one BatchExecutor event matches this Papertrade intent ID.",
                "Keep the original operation unresolved and inspect canonical chain evidence.",
            ));
        }
        let Some(hash) = candidates.into_iter().next() else {
            return Ok(None);
        };
        self.by_hash(&hash).await
    }

    async fn has_intent_event_through(
        &self,
        intent_id: &str,
        from_block: u64,
        through_block: u64,
    ) -> Result<bool> {
        if intent_id.len() != 66
            || !intent_id.starts_with("0x")
            || !intent_id[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(GoFinError::validation("Papertrade intent ID is invalid."));
        }
        if from_block > through_block {
            return Ok(false);
        }
        let mut cursor = from_block;
        while cursor <= through_block {
            let end = cursor
                .saturating_add(CANONICAL_LOG_PAGE_BLOCKS - 1)
                .min(through_block);
            let logs = self
                .get_logs(json!({
                    "address": BATCH_EXECUTOR,
                    "fromBlock": format!("0x{cursor:x}"),
                    "toBlock": format!("0x{end:x}"),
                    "topics": [[CALL_EXECUTED_TOPIC, CALL_REJECTED_TOPIC, CALL_SKIPPED_TOPIC], null, null, intent_id.to_ascii_lowercase()]
                }))
                .await?;
            let rows = logs.as_array().ok_or_else(|| {
                invalid_response("HyperEVM returned malformed Papertrade intent logs.")
            })?;
            if let Some(row) = rows.first() {
                let topics = row
                    .get("topics")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid_response("BatchExecutor intent log omitted topics."))?;
                if row
                    .get("address")
                    .and_then(Value::as_str)
                    .is_none_or(|address| !address.eq_ignore_ascii_case(BATCH_EXECUTOR))
                    || topics.len() != 4
                    || topics[0].as_str().is_none_or(|topic| {
                        ![CALL_EXECUTED_TOPIC, CALL_REJECTED_TOPIC, CALL_SKIPPED_TOPIC]
                            .iter()
                            .any(|expected| topic.eq_ignore_ascii_case(expected))
                    })
                    || topics[3]
                        .as_str()
                        .is_none_or(|topic| !topic.eq_ignore_ascii_case(intent_id))
                {
                    return Err(invalid_response(
                        "BatchExecutor log did not bind the exact Papertrade intent ID.",
                    ));
                }
                return Ok(true);
            }
            if end == through_block {
                break;
            }
            tokio::time::sleep(self.page_delay).await;
            cursor = end.saturating_add(1);
        }
        Ok(false)
    }

    async fn get_logs(&self, filter: Value) -> Result<Value> {
        for attempt in 0..CANONICAL_LOG_RATE_RETRIES {
            match self
                .rpc
                .request("eth_getLogs", json!([filter.clone()]))
                .await
            {
                Ok(logs) => return Ok(logs),
                Err(error) => {
                    let details = error.details.as_deref();
                    let is_rate_limit = error.code == "RPC_RATE_LIMITED"
                        || details
                            .and_then(|value| value.get("json_rpc_code"))
                            .and_then(Value::as_i64)
                            == Some(-32005);
                    if !is_rate_limit || attempt + 1 == CANONICAL_LOG_RATE_RETRIES {
                        return Err(error);
                    }
                    let delay_ms = details
                        .and_then(|value| value.get("retry_after_ms"))
                        .and_then(Value::as_u64)
                        .unwrap_or(30_000)
                        .clamp(100, CANONICAL_LOG_MAX_RATE_RETRY_DELAY.as_millis() as u64);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
        Err(unavailable(
            "Canonical Papertrade log lookup did not complete.",
        ))
    }

    pub(crate) async fn by_hash(
        &self,
        transaction_hash: &str,
    ) -> Result<Option<gofin_papertrade::CanonicalTransaction>> {
        let Some(canonical) = self
            .runtime
            .observe_receipt(transaction_hash, self.confirmations, self.require_finalized)
            .await?
        else {
            return Ok(None);
        };
        let tx = self
            .rpc
            .request("eth_getTransactionByHash", json!([transaction_hash]))
            .await?;
        if tx.is_null() {
            return Ok(None);
        }
        let receipt = &canonical.receipt;
        let tx_hash = tx
            .get("hash")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_response("Canonical transaction omitted hash."))?;
        let to = tx
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_response("Canonical transaction omitted target."))?;
        let from = tx
            .get("from")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_response("Canonical transaction omitted sender."))?;
        let input = tx
            .get("input")
            .or_else(|| tx.get("data"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_response("Canonical transaction omitted calldata."))?;
        let logs = receipt
            .get("logs")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_response("Canonical receipt omitted logs."))?;
        let mut normalized_logs = Vec::with_capacity(logs.len());
        for log in logs {
            normalized_logs.push(gofin_papertrade::CanonicalLog {
                address: required_string(log, "address")?.to_ascii_lowercase(),
                topics: log
                    .get("topics")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid_response("Canonical receipt log omitted topics."))?
                    .iter()
                    .map(|topic| {
                        topic.as_str().map(str::to_ascii_lowercase).ok_or_else(|| {
                            invalid_response("Canonical receipt topic is malformed.")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                data: required_string(log, "data")?.to_ascii_lowercase(),
                log_index: required_string(log, "logIndex")?.to_ascii_lowercase(),
            });
        }
        let block_number = canonical.block_number;
        let gas_used = rpc_hex_quantity(required_string(receipt, "gasUsed")?)?;
        let effective_gas_price = rpc_hex_quantity(required_string(receipt, "effectiveGasPrice")?)?;
        let (exchange_implementation, exchange_code_hash) = self
            .implementation_identity(gofin_papertrade::EXCHANGE, block_number)
            .await?;
        let (staking_implementation, staking_code_hash) = self
            .implementation_identity(gofin_papertrade::PAPER_STAKING, block_number)
            .await?;
        let (batch_implementation, batch_code_hash) = self
            .implementation_identity(BATCH_EXECUTOR, block_number)
            .await?;
        Ok(Some(gofin_papertrade::CanonicalTransaction {
            transaction_hash: tx_hash.to_ascii_lowercase(),
            transaction_from: Some(from.to_ascii_lowercase()),
            transaction_to: to.to_ascii_lowercase(),
            transaction_input: input.to_ascii_lowercase(),
            block_hash: canonical.block_hash.to_string().to_ascii_lowercase(),
            block_number,
            canonical: true,
            success: canonical.success,
            gas_used: Some(gas_used),
            effective_gas_price: Some(effective_gas_price),
            logs: normalized_logs,
            exchange_implementation: Some(exchange_implementation),
            exchange_code_hash: Some(exchange_code_hash),
            staking_implementation: Some(staking_implementation),
            staking_code_hash: Some(staking_code_hash),
            batch_implementation: Some(batch_implementation),
            batch_code_hash: Some(batch_code_hash),
        }))
    }

    pub(crate) async fn by_liquidation(
        &self,
        owner: &str,
        position_id: &str,
        since_block: Option<u64>,
    ) -> Result<Option<gofin_papertrade::CanonicalTransaction>> {
        if owner.len() != 42
            || !owner.starts_with("0x")
            || !owner[2..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(GoFinError::validation(
                "Liquidation owner address is malformed.",
            ));
        }
        let id = BigUint::parse_bytes(position_id.as_bytes(), 10)
            .ok_or_else(|| GoFinError::validation("Liquidation position ID is malformed."))?;
        if id.bits() > 256 {
            return Err(GoFinError::validation(
                "Liquidation position ID exceeds uint256.",
            ));
        }
        let bytes = id.to_bytes_be();
        let mut padded = [0u8; 32];
        padded[32 - bytes.len()..].copy_from_slice(&bytes);
        let topic_id = format!("0x{}", hex::encode(padded));
        let mut owner_topic = [0u8; 32];
        hex::decode_to_slice(&owner[2..], &mut owner_topic[12..])
            .map_err(|_| GoFinError::validation("Liquidation owner address is malformed."))?;
        let owner_topic = format!("0x{}", hex::encode(owner_topic));
        let head = self.rpc.request("eth_blockNumber", json!([])).await?;
        let head = rpc_hex_u64(&head)?;
        // External positions without a known open proof are searched only in
        // an explicit bounded recent window. Absence never proves non-liquidation.
        let mut cursor = since_block.unwrap_or_else(|| head.saturating_sub(9_999));
        if cursor > head {
            return Ok(None);
        }
        let mut candidates = std::collections::BTreeSet::new();
        while cursor <= head {
            let end = cursor.saturating_add(199).min(head);
            let logs=self.rpc.request("eth_getLogs",json!([{"address":gofin_papertrade::EXCHANGE,"fromBlock":format!("0x{cursor:x}"),"toBlock":format!("0x{end:x}"),"topics":[LIQUIDATED_TOPIC,topic_id,owner_topic]}])).await?;
            let rows = logs
                .as_array()
                .ok_or_else(|| invalid_response("HyperEVM returned malformed liquidation logs."))?;
            for row in rows {
                let topics = row
                    .get("topics")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid_response("Liquidation log omitted topics."))?;
                if topics.len() != 3
                    || topics[0]
                        .as_str()
                        .is_none_or(|v| !v.eq_ignore_ascii_case(LIQUIDATED_TOPIC))
                    || topics[1]
                        .as_str()
                        .is_none_or(|v| !v.eq_ignore_ascii_case(&topic_id))
                    || topics[2]
                        .as_str()
                        .is_none_or(|v| !v.eq_ignore_ascii_case(&owner_topic))
                    || row
                        .get("address")
                        .and_then(Value::as_str)
                        .is_none_or(|v| !v.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE))
                {
                    return Err(invalid_response(
                        "Liquidation log differs from the exact owner and position filter.",
                    ));
                }
                let hash = required_string(row, "transactionHash")?;
                candidates.insert(hash.to_ascii_lowercase());
            }
            if end == head {
                break;
            }
            cursor = end.saturating_add(1);
        }
        if candidates.len() > 1 {
            return Err(GoFinError::new(
                "PAPERTRADE_LIQUIDATION_AMBIGUOUS",
                "read",
                "Multiple canonical liquidation candidates matched the same position.",
                "Inspect the exact position history before retrying.",
            ));
        }
        let Some(hash) = candidates.into_iter().next() else {
            return Ok(None);
        };
        self.by_hash(&hash).await
    }

    async fn implementation_identity(
        &self,
        proxy: &str,
        block_number: u64,
    ) -> Result<(String, String)> {
        let block_tag = format!("0x{block_number:x}");
        let storage = self
            .rpc
            .request(
                "eth_getStorageAt",
                json!([proxy, EIP1967_IMPLEMENTATION_SLOT, block_tag]),
            )
            .await?;
        let storage = storage
            .as_str()
            .ok_or_else(|| invalid_response("Papertrade implementation storage is malformed."))?;
        let implementation = storage
            .strip_prefix("0x")
            .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| invalid_response("Papertrade implementation slot is malformed."))?;
        let implementation = format!("0x{}", &implementation[24..]).to_ascii_lowercase();
        let code = self
            .rpc
            .request("eth_getCode", json!([implementation, block_tag]))
            .await?;
        let code = code
            .as_str()
            .and_then(|code| hex::decode(code.strip_prefix("0x")?).ok())
            .filter(|code| !code.is_empty())
            .ok_or_else(|| invalid_response("Papertrade implementation code is malformed."))?;
        let code_hash = format!("0x{:x}", gofin_evm::alloy::primitives::keccak256(code));
        Ok((implementation, code_hash))
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response("Canonical receipt has a malformed log field."))
}

fn rpc_hex_u64(value: &Value) -> Result<u64> {
    value
        .as_str()
        .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
        .ok_or_else(|| invalid_response("HyperEVM returned a malformed block number."))
}

fn rpc_hex_quantity(value: &str) -> Result<String> {
    let digits = value
        .strip_prefix("0x")
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| invalid_response("HyperEVM returned a malformed receipt quantity."))?;
    BigUint::parse_bytes(digits.as_bytes(), 16)
        .map(|quantity| quantity.to_string())
        .ok_or_else(|| invalid_response("HyperEVM returned a malformed receipt quantity."))
}

const API: &str = "https://exchange.papertrade.xyz";
const API_ORIGIN: &str = "https://exchange.papertrade.xyz";
const USER_AGENT: &str = "GoFin/0.1 (Papertrade integration)";
const READ_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_READ_BODY_BYTES: usize = 2 * 1024 * 1024;

async fn read_bounded_body(mut response: reqwest::Response, label: &str) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_READ_BODY_BYTES as u64)
    {
        return Err(invalid_response(&format!(
            "{label} exceeded its size bound."
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| unavailable(&format!("{label} body failed.")))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_READ_BODY_BYTES {
            return Err(invalid_response(&format!(
                "{label} exceeded its size bound."
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Clone)]
pub(crate) struct PapertradeHttp {
    client: reqwest::Client,
    base: String,
    build_id: std::sync::Arc<tokio::sync::Mutex<Option<(String, std::time::Instant)>>>,
}

impl PapertradeHttp {
    pub(crate) fn new() -> Result<Self> {
        Self::with_base(API)
    }

    fn with_base(base: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| unavailable("Could not initialize the Papertrade read transport."))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            build_id: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_with_base(base: &str) -> Result<Self> {
        Self::with_base(base)
    }

    pub(crate) async fn build_id(&self) -> Result<String> {
        let mut cached = self.build_id.lock().await;
        if let Some((value, observed)) = cached.as_ref()
            && observed.elapsed() < Duration::from_secs(60)
        {
            return Ok(value.clone());
        }
        let value = tokio::time::timeout(READ_TIMEOUT, async {
            let html_url = format!("{}/trade/btc", self.base);
            let html = self.fetch_bounded_text(&html_url).await?;
            let asset = parse_build_asset(&html)?;
            let script_url = format!("{}{}", self.base, asset);
            let script = self.fetch_bounded_text(&script_url).await?;
            parse_build_id(&script)
        })
        .await
        .map_err(|_| unavailable("Papertrade build version lookup timed out."))??;
        *cached = Some((value.clone(), std::time::Instant::now()));
        Ok(value)
    }

    pub(crate) async fn funding_records(&self, owner: &str) -> Result<Value> {
        let owner = checked_owner(owner)?;
        self.get_json(&format!("/query/accounts/{owner}/funding"))
            .await
    }

    /// Ask the official funding-check endpoint for the owner's proxy. The
    /// caller must independently compare this response with the factory's
    /// chain-derived address before treating registration as complete.
    pub(crate) async fn check_deposit_proxy(&self, owner: &str) -> Result<String> {
        let owner = checked_owner(owner)?;
        let url = format!("{}/funding/deposits/check", self.base);
        tokio::time::timeout(READ_TIMEOUT, async {
            let build = self.build_id().await?;
            let response = self
                .client
                .post(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ORIGIN, API_ORIGIN)
                .header("X-Papertrade-Build", build)
                .json(&json!({"user": owner}))
                .send()
                .await
                .map_err(|_| unavailable("Papertrade proxy registration check failed."))?;
            if !response.status().is_success() {
                return Err(unavailable(
                    "Papertrade did not acknowledge the proxy registration.",
                ));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_READ_BODY_BYTES as u64)
            {
                return Err(invalid_response(
                    "Papertrade proxy registration response exceeded its size bound.",
                ));
            }
            let bytes =
                read_bounded_body(response, "Papertrade proxy registration response").await?;
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                invalid_response("Papertrade proxy registration response is malformed.")
            })?;
            if value
                .as_object()
                .is_none_or(|object| object.len() != 1 || !object.contains_key("proxy"))
            {
                return Err(invalid_response(
                    "Papertrade proxy registration response has unexpected fields.",
                ));
            }
            let proxy = value.get("proxy").and_then(Value::as_str).ok_or_else(|| {
                invalid_response("Papertrade proxy registration omitted its proxy.")
            })?;
            Ok(format!("{:#x}", gofin_evm::address(proxy)?))
        })
        .await
        .map_err(|_| unavailable("Papertrade proxy registration check timed out."))?
    }

    async fn fetch_bounded_text(&self, url: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "text/html,application/javascript")
            .send()
            .await
            .map_err(|_| unavailable("Papertrade build asset request failed."))?;
        if !response.status().is_success() {
            return Err(unavailable("Papertrade build asset was unavailable."));
        }
        let bytes = read_bounded_body(response, "Papertrade build asset").await?;
        String::from_utf8(bytes)
            .map_err(|_| invalid_response("Papertrade build asset was not UTF-8."))
    }

    pub(crate) async fn read(
        &self,
        request: ReadRequest,
        rpc: &dyn gofin_evm::EvmRpc,
    ) -> Result<Value> {
        match request {
            ReadRequest::TradingState => self.get_json("/state/trading").await,
            ReadRequest::ProtocolSummary => self.get_json("/query/protocol/summary").await,
            ReadRequest::AccountState { owner } => self.wallet_snapshot(&owner).await,
            ReadRequest::Positions { owner } => {
                let snapshot = self.wallet_snapshot(&owner).await?;
                let positions = snapshot.get("openPositions").cloned().ok_or_else(|| {
                    invalid_response("Papertrade wallet snapshot omitted openPositions.")
                })?;
                normalize_positions(&snapshot, &owner, &positions)
            }
            ReadRequest::History { owner, since_ms } => {
                let owner = checked_owner(&owner)?;
                let url = format!("/query/accounts/{owner}/trades?filter=all&limit=75");
                let mut history = self.get_json(&url).await?;
                if let Some(since) = since_ms {
                    filter_since(&mut history, since)?;
                }
                Ok(history)
            }
            ReadRequest::StakingState { owner } => self.staking_state(&owner, rpc).await,
        }
    }

    /// Submit the exact already-journaled typed intent. A successful response
    /// proves relayer acceptance only; settlement is discovered separately
    /// through canonical BatchExecutor logs.
    pub(crate) async fn submit_typed(
        &self,
        bound: &BoundIntent,
        signer: &gofin_evm::LocalEnvelopeSigner,
    ) -> Result<Receipt> {
        let action = bound.action.as_deref().ok_or_else(|| {
            GoFinError::validation("Cancellation uses its dedicated Papertrade endpoint.")
        })?;
        if action != bound.typed.action().unwrap_or_default() {
            return Err(GoFinError::validation(
                "Papertrade action does not match its typed intent.",
            ));
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        bound.typed.validate(now)?;
        let typed = to_evm_intent(&bound.typed);
        let (expected_signer, expected_id) = signer.papertrade_intent_identity(&typed, now)?;
        if !expected_signer.eq_ignore_ascii_case(&bound.signer)
            || !expected_id.eq_ignore_ascii_case(&bound.intent_id)
        {
            return Err(GoFinError::validation(
                "Papertrade typed intent no longer matches its frozen signer identity.",
            ));
        }
        let signature = signer.sign_papertrade_intent(&typed, now)?;
        let mut signed = typed_api_fields(&bound.typed)?;
        let object = signed.as_object_mut().ok_or_else(|| {
            GoFinError::validation("Papertrade typed intent encoding is not an object.")
        })?;
        object.insert("v".into(), json!(signature.v));
        object.insert("r".into(), json!(signature.r));
        object.insert("s".into(), json!(signature.s));
        let mut request = json!({"action":action,"signed":signed});
        if matches!(bound.typed, PapertradeTypedIntent::WithdrawToCore { .. }) {
            // Withdrawal route metadata is part of the official relayer
            // request envelope, though it is not part of the EIP-712 message.
            // Stage one is always the Papertrade→HyperCore credit; onward
            // routing is a separate, explicitly approved continuation.
            request["withdrawal"] = json!({"route":"hypercore"});
        }
        let response = self.post_json("/intents", request).await?;
        validate_submit_response(response, action, &bound.intent_id)
    }

    pub(crate) async fn submit_cancel(
        &self,
        bound: &BoundIntent,
        signer: &gofin_evm::LocalEnvelopeSigner,
        target_action: &str,
    ) -> Result<Receipt> {
        let PapertradeTypedIntent::CancelIntent { intent_id, .. } = &bound.typed else {
            return Err(GoFinError::validation(
                "Expected a typed Papertrade cancellation.",
            ));
        };
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        bound.typed.validate(now)?;
        let typed = to_evm_intent(&bound.typed);
        let (expected_signer, expected_id) = signer.papertrade_intent_identity(&typed, now)?;
        if !expected_signer.eq_ignore_ascii_case(&bound.signer)
            || !expected_id.eq_ignore_ascii_case(&bound.intent_id)
        {
            return Err(GoFinError::validation(
                "Papertrade cancellation no longer matches its frozen signer identity.",
            ));
        }
        let signature = signer.sign_papertrade_intent(&typed, now)?;
        let mut signed = typed_api_fields(&bound.typed)?;
        let object = signed.as_object_mut().ok_or_else(|| {
            GoFinError::validation("Papertrade cancellation encoding is not an object.")
        })?;
        object.remove("intentId");
        object.insert("v".into(), json!(signature.v));
        object.insert("r".into(), json!(signature.r));
        object.insert("s".into(), json!(signature.s));
        let response = self
            .post_json(&format!("/intents/{}/cancel", intent_id), signed)
            .await?;
        validate_cancel_response(response, intent_id, target_action, &bound.intent_id)
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        tokio::time::timeout(READ_TIMEOUT, async {
            let build = self.build_id().await?;
            let response = self
                .client
                .post(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::ORIGIN, API_ORIGIN)
                .header("X-Papertrade-Build", build)
                .json(&body)
                .send()
                .await
                .map_err(|_| unavailable("Papertrade intent submission failed."))?;
            if !response.status().is_success() {
                let status = response.status();
                let bytes = read_bounded_body(response, "Papertrade error response").await?;
                let code = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|value| bounded_exchange_error_code(&value));
                let detail = code.as_ref().map_or_else(
                    || format!("HTTP {}", status.as_u16()),
                    |code| format!("HTTP {} ({code})", status.as_u16()),
                );
                return Err(GoFinError::new(
                    "PAPERTRADE_HTTP_REJECTED",
                    "submission",
                    format!("Papertrade intent endpoint rejected the request: {detail}."),
                    "Reconcile the original intent identity before attempting another submission.",
                )
                .with_details(json!({
                    "http_status": status.as_u16(),
                    "exchange_code": code,
                })));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_READ_BODY_BYTES as u64)
            {
                return Err(invalid_response(
                    "Papertrade intent response exceeded its size bound.",
                ));
            }
            let bytes = read_bounded_body(response, "Papertrade intent response").await?;
            serde_json::from_slice(&bytes)
                .map_err(|_| invalid_response("Papertrade intent endpoint returned invalid JSON."))
        })
        .await
        .map_err(|_| unavailable("Papertrade intent submission timed out."))?
    }

    pub(crate) async fn staking_state(
        &self,
        owner: &str,
        rpc: &dyn gofin_evm::EvmRpc,
    ) -> Result<Value> {
        let snapshot = self.wallet_snapshot(owner).await?;
        let trading = self.get_json("/state/trading").await?;
        let block = snapshot
            .get("indexedThroughBlock")
            .and_then(parse_block_number)
            .ok_or_else(|| {
                invalid_response("Papertrade wallet snapshot omitted its indexed block.")
            })?;
        let latest = rpc.request("eth_blockNumber", json!([])).await?;
        let latest_block = latest
            .as_str()
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| invalid_response("HyperEVM RPC returned an invalid latest block."))?;
        let block_tag = format!("0x{latest_block:x}");
        let response = rpc
            .request(
                "eth_call",
                json!([{"to": gofin_papertrade::PAPER_STAKING, "data": "0xcab64bcd"}, block_tag]),
            )
            .await?;
        let reward_reserve = decode_u256_result(response.as_str().ok_or_else(|| {
            invalid_response("PaperStaking.rewardReserve returned a non-string result.")
        })?)?;
        let trading_block = trading
            .get("indexedThroughBlock")
            .and_then(parse_block_number)
            .ok_or_else(|| {
                invalid_response("Papertrade trading state omitted its indexed block.")
            })?;
        let mut normalized = normalize_staking(&snapshot, &trading, &reward_reserve)?;
        normalized["observation"] = json!({
            "walletIndexedThroughBlock": block.to_string(),
            "tradingIndexedThroughBlock": trading_block.to_string(),
            "contractObservedAtBlock": latest_block.to_string(),
            "walletIndexLagBlocks": latest_block.saturating_sub(block).to_string(),
            "tradingIndexLagBlocks": latest_block.saturating_sub(trading_block).to_string(),
            "coherent": latest_block == block && latest_block == trading_block
        });
        Ok(normalized)
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        tokio::time::timeout(READ_TIMEOUT, async {
            let build = self.build_id().await?;
            let response = self
                .client
                .get(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .header("X-Papertrade-Build", build)
                .send()
                .await
                .map_err(|_| unavailable("Papertrade read request failed."))?;
            if !response.status().is_success() {
                return Err(unavailable("Papertrade read endpoint returned an error."));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_READ_BODY_BYTES as u64)
            {
                return Err(invalid_response(
                    "Papertrade read response exceeded its size bound.",
                ));
            }
            let bytes = read_bounded_body(response, "Papertrade read response").await?;
            serde_json::from_slice(&bytes)
                .map_err(|_| invalid_response("Papertrade read endpoint returned invalid JSON."))
        })
        .await
        .map_err(|_| unavailable("Papertrade read request timed out."))?
    }

    async fn wallet_snapshot(&self, owner: &str) -> Result<Value> {
        let owner = checked_owner(owner)?;
        let url = format!("{}/state/user/live?wallet={owner}", self.base);
        tokio::time::timeout(READ_TIMEOUT, async {
            let build = self.build_id().await?;
            let response = self
                .client
                .get(url)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header("X-Papertrade-Build", build)
                .send()
                .await
                .map_err(|_| unavailable("Papertrade account snapshot request failed."))?;
            if !response.status().is_success() {
                return Err(unavailable(
                    "Papertrade account snapshot endpoint returned an error.",
                ));
            }
            let mut response = response;
            let mut pending = Vec::<u8>::new();
            loop {
                let chunk = response
                    .chunk()
                    .await
                    .map_err(|_| unavailable("Papertrade account snapshot stream failed."))?;
                let Some(chunk) = chunk else {
                    return Err(invalid_response(
                        "Papertrade account snapshot ended before its wallet event.",
                    ));
                };
                if pending.len().saturating_add(chunk.len()) > MAX_READ_BODY_BYTES {
                    return Err(invalid_response(
                        "Papertrade account stream exceeded its size bound.",
                    ));
                }
                pending.extend_from_slice(&chunk);
                while let Some(boundary) = pending.windows(2).position(|pair| pair == b"\n\n") {
                    let event = pending.drain(..boundary + 2).collect::<Vec<_>>();
                    if let Some(snapshot) = parse_wallet_event(&event, &owner)? {
                        return Ok(snapshot);
                    }
                }
                if pending.len() > MAX_READ_BODY_BYTES {
                    return Err(invalid_response(
                        "Papertrade account event exceeded its size bound.",
                    ));
                }
            }
        })
        .await
        .map_err(|_| unavailable("Papertrade account snapshot timed out."))?
    }
}

fn bounded_exchange_error_code(value: &Value) -> Option<String> {
    let error = value.get("error");
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| {
            error
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
        })?;
    (!code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then(|| code.to_owned())
}

fn parse_build_asset(html: &str) -> Result<&str> {
    html.split_whitespace()
        .flat_map(|part| part.split(['"', '\'', '(', ')', '=']))
        .find(|part| {
            part.starts_with("/assets/build-")
                && part.ends_with(".js")
                && part[14..part.len() - 3]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
        .ok_or_else(|| invalid_response("Papertrade page omitted its build asset."))
}

fn parse_build_id(script: &str) -> Result<String> {
    let marker = "var e=`";
    let start = script
        .find(marker)
        .ok_or_else(|| invalid_response("Papertrade build asset omitted its build identifier."))?
        + marker.len();
    let tail = &script[start..];
    let end = tail
        .find('`')
        .ok_or_else(|| invalid_response("Papertrade build identifier is malformed."))?;
    let value = &tail[..end];
    // The app's generated bundle can reorder its named exports. Bind the
    // build identifier through the exported name instead of assuming it is
    // the first export (`export{e as n}`). This remains a narrow parser for
    // the known declaration/export syntax; it never evaluates JavaScript.
    let exports_build_id = script
        .match_indices("export{")
        .filter_map(|(start, _)| {
            let body_start = start + "export{".len();
            let body_end = script[body_start..].find('}')? + body_start;
            script[body_start..body_end]
                .split(',')
                .map(str::trim)
                .any(|binding| binding == "e as n")
                .then_some(())
        })
        .next()
        .is_some();
    if value.len() != 12 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) || !exports_build_id
    {
        return Err(invalid_response(
            "Papertrade build identifier failed validation.",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_submit_response(response: Value, action: &str, intent_id: &str) -> Result<Receipt> {
    if response.get("action").and_then(Value::as_str) != Some(action)
        || response
            .get("intentId")
            .and_then(Value::as_str)
            .is_none_or(|actual| !actual.eq_ignore_ascii_case(intent_id))
        || response.get("queueRevision").is_none()
    {
        return Err(invalid_response(
            "Papertrade response did not confirm the exact submitted intent.",
        ));
    }
    Ok(Receipt {
        intent_id: intent_id.to_ascii_lowercase(),
        action: action.to_owned(),
        status: "submitted".into(),
        transaction_hash: None,
    })
}

fn validate_cancel_response(
    response: Value,
    target_intent_id: &str,
    target_action: &str,
    signed_intent_id: &str,
) -> Result<Receipt> {
    let response_id = response.get("intentId").and_then(Value::as_str);
    let action = response.get("action").and_then(Value::as_str);
    let state = response.get("state").and_then(Value::as_str);
    if !response_id.is_some_and(|value| value.eq_ignore_ascii_case(target_intent_id))
        || action != Some(target_action)
        || !matches!(
            action,
            Some(
                "registerSessionKey"
                    | "openPosition"
                    | "closePositions"
                    | "withdrawToCore"
                    | "paperStake"
                    | "paperUnstake"
                    | "paperClaim"
            )
        )
        || !matches!(
            state,
            Some("cancelled" | "succeeded" | "failed" | "expired" | "pending" | "unknown")
        )
    {
        return Err(invalid_response(
            "Papertrade cancellation response did not identify the exact target and state.",
        ));
    }
    Ok(Receipt {
        // The adapter's durable submission is bound to the signed cancellation
        // intent; terminal proof separately binds the target order ID.
        intent_id: signed_intent_id.to_ascii_lowercase(),
        action: target_action.to_owned(),
        status: state.expect("validated above").to_owned(),
        transaction_hash: None,
    })
}

fn typed_api_fields(value: &PapertradeTypedIntent) -> Result<Value> {
    let mut fields = serde_json::to_value(value)
        .map_err(|_| GoFinError::validation("Papertrade typed intent could not be encoded."))?;
    let object = fields.as_object_mut().ok_or_else(|| {
        GoFinError::validation("Papertrade typed intent encoding is not an object.")
    })?;
    object.remove("primary_type");
    match value {
        PapertradeTypedIntent::RegisterSessionKey { .. } => {
            for field in ["expiry", "nonce", "deadline"] {
                normalize_api_uint(object, field)?;
            }
            for field in ["user", "key", "feeAddress"] {
                normalize_api_address(object, field)?;
            }
        }
        PapertradeTypedIntent::OpenPosition { .. } => {
            for field in ["instrumentId", "size", "leverage", "nonce", "deadline"] {
                normalize_api_uint(object, field)?;
            }
            normalize_api_address(object, "user")?;
        }
        PapertradeTypedIntent::Close { .. } => {
            normalize_api_uint(object, "nonce")?;
            normalize_api_uint(object, "deadline")?;
            normalize_api_uint_array(object, "positionIds")?;
            normalize_api_address(object, "user")?;
        }
        PapertradeTypedIntent::WithdrawToCore { .. }
        | PapertradeTypedIntent::Stake { .. }
        | PapertradeTypedIntent::Unstake { .. } => {
            for field in ["amount", "nonce", "deadline"] {
                normalize_api_uint(object, field)?;
            }
            normalize_api_address(object, "user")?;
        }
        PapertradeTypedIntent::Claim { .. } => {
            normalize_api_uint(object, "nonce")?;
            normalize_api_uint(object, "deadline")?;
            normalize_api_address(object, "user")?;
        }
        PapertradeTypedIntent::CancelIntent { .. } => {
            normalize_api_uint(object, "deadline")?;
            normalize_api_address(object, "user")?;
        }
    }
    Ok(fields)
}

fn normalize_api_uint(object: &mut serde_json::Map<String, Value>, field: &str) -> Result<()> {
    let value = object.get(field).cloned().ok_or_else(|| {
        GoFinError::validation("Papertrade typed message omitted an integer field.")
    })?;
    let decimal = match value {
        Value::String(value) => {
            BigUint::parse_bytes(value.as_bytes(), 10).map(|number| number.to_str_radix(10))
        }
        Value::Number(value) => value
            .as_u64()
            .map(|number| number.to_string())
            .and_then(|value| BigUint::parse_bytes(value.as_bytes(), 10))
            .map(|number| number.to_str_radix(10)),
        _ => None,
    }
    .ok_or_else(|| GoFinError::validation("Papertrade typed integer field is malformed."))?;
    object.insert(field.to_owned(), Value::String(decimal));
    Ok(())
}

fn normalize_api_uint_array(
    object: &mut serde_json::Map<String, Value>,
    field: &str,
) -> Result<()> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| GoFinError::validation("Papertrade typed integer array is malformed."))?;
    let normalized = values
        .iter()
        .map(|value| match value {
            Value::String(value) => BigUint::parse_bytes(value.as_bytes(), 10),
            Value::Number(value) => value
                .as_u64()
                .and_then(|number| BigUint::parse_bytes(number.to_string().as_bytes(), 10)),
            _ => None,
        })
        .map(|value| {
            value
                .map(|number| Value::String(number.to_str_radix(10)))
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Papertrade typed integer array contains a malformed value.",
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    object.insert(field.to_owned(), Value::Array(normalized));
    Ok(())
}

fn normalize_api_address(object: &mut serde_json::Map<String, Value>, field: &str) -> Result<()> {
    let address = object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| value.len() == 42 && value.starts_with("0x"))
        .ok_or_else(|| GoFinError::validation("Papertrade typed address field is malformed."))?;
    object.insert(
        field.to_owned(),
        Value::String(address.to_ascii_lowercase()),
    );
    Ok(())
}

fn to_evm_intent(
    value: &PapertradeTypedIntent,
) -> gofin_evm::papertrade_signing::PapertradeTypedIntent {
    use gofin_evm::papertrade_signing::PapertradeTypedIntent as Typed;
    match value {
        PapertradeTypedIntent::RegisterSessionKey {
            user,
            key,
            expiry,
            fee_address,
            nonce,
            deadline,
        } => Typed::RegisterSessionKey {
            user: user.clone(),
            key: key.clone(),
            expiry: *expiry,
            fee_address: fee_address.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::OpenPosition {
            user,
            instrument_id,
            is_long,
            size,
            leverage,
            use_debt,
            nonce,
            deadline,
        } => Typed::OpenPosition {
            user: user.clone(),
            instrument_id: *instrument_id,
            is_long: *is_long,
            size: *size,
            leverage: *leverage,
            use_debt: *use_debt,
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Close {
            user,
            position_ids,
            nonce,
            deadline,
        } => Typed::Close {
            user: user.clone(),
            position_ids: position_ids.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::WithdrawToCore {
            user,
            amount,
            nonce,
            deadline,
        } => Typed::WithdrawToCore {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Stake {
            user,
            amount,
            nonce,
            deadline,
        } => Typed::Stake {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Unstake {
            user,
            amount,
            nonce,
            deadline,
        } => Typed::Unstake {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Claim {
            user,
            nonce,
            deadline,
        } => Typed::Claim {
            user: user.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::CancelIntent {
            user,
            intent_id,
            deadline,
        } => Typed::CancelIntent {
            user: user.clone(),
            intent_id: intent_id.clone(),
            deadline: deadline.clone(),
        },
    }
}

fn parse_wallet_event(event: &[u8], expected_owner: &str) -> Result<Option<Value>> {
    let text = std::str::from_utf8(event)
        .map_err(|_| invalid_response("Papertrade account stream is not UTF-8."))?;
    let mut name = None;
    let mut data = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            name = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    if name != Some("wallet") {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&data)
        .map_err(|_| invalid_response("Papertrade wallet event contains invalid JSON."))?;
    let actual = value
        .get("wallet")
        .or_else(|| value.get("address"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response("Papertrade wallet event omitted its owner."))?;
    if !actual.eq_ignore_ascii_case(expected_owner) {
        return Err(invalid_response(
            "Papertrade wallet stream returned a different owner's event.",
        ));
    }
    Ok(Some(value))
}

fn normalize_staking(snapshot: &Value, trading: &Value, reward_reserve: &str) -> Result<Value> {
    let paper = snapshot
        .get("paper")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response("Papertrade wallet event omitted raw staking balances."))?;
    let balance = raw_decimal(paper.get("balance"), "paper.balance")?;
    let staked = raw_decimal(paper.get("staked"), "paper.staked")?;
    let pending = raw_decimal(paper.get("pendingBase"), "paper.pendingBase")?;
    let reward_debt = raw_decimal(paper.get("rewardDebt"), "paper.rewardDebt")?;
    let acc = raw_decimal(trading.get("accRewardPerShareRaw"), "accRewardPerShareRaw")?;
    let staked_n = parse_uint(&staked)?;
    let pending_n = parse_uint(&pending)?;
    let reward_debt_n = parse_uint(&reward_debt)?;
    let acc_n = parse_uint(&acc)?;
    let reserve_n = parse_uint(reward_reserve)?;
    let gross = (staked_n * acc_n) / BigUint::from(10u8).pow(27);
    let computed = pending_n + gross;
    if computed < reward_debt_n {
        return Err(invalid_response(
            "Papertrade staking reward fields are internally inconsistent.",
        ));
    }
    let claimable = computed - reward_debt_n;
    Ok(json!({
        "paper": {
            "balance": balance,
            "staked": staked,
            "pendingBase": pending,
            "rewardDebt": reward_debt
        },
        "accRewardPerShareRaw": acc,
        "claimableRewardsRaw": claimable.to_string(),
        "rewardReserveRaw": reserve_n.to_string()
    }))
}

fn normalize_positions(snapshot: &Value, owner: &str, positions: &Value) -> Result<Value> {
    let rows = positions
        .as_array()
        .ok_or_else(|| invalid_response("Papertrade openPositions is not an array."))?;
    let mut normalized = Vec::with_capacity(rows.len());
    for row in rows {
        let fields = row
            .as_array()
            .filter(|fields| fields.len() == 10)
            .ok_or_else(|| invalid_response("Papertrade position tuple has the wrong shape."))?;
        let position_id = exact_uint(&fields[0], "positionId")?;
        let instrument_id = exact_small_uint(&fields[1], "instrumentId")?;
        let is_long = fields[2]
            .as_bool()
            .ok_or_else(|| invalid_response("Papertrade isLong field is malformed."))?;
        let use_debt = fields[3]
            .as_bool()
            .ok_or_else(|| invalid_response("Papertrade useDebt field is malformed."))?;
        let entry_price = exact_uint(&fields[4], "entryPriceRaw")?;
        let margin = exact_uint(&fields[5], "marginRaw")?;
        let leverage = exact_small_uint(&fields[6], "leverage")?;
        let bust_price = exact_uint(&fields[7], "bustPriceRaw")?;
        let opened_at_ms = exact_small_uint(&fields[8], "openedAtMs")?;
        let intent_id = match &fields[9] {
            Value::Null => Value::Null,
            Value::String(value)
                if value.len() == 66
                    && value.starts_with("0x")
                    && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                Value::String(value.to_ascii_lowercase())
            }
            _ => {
                return Err(invalid_response(
                    "Papertrade position intentId is malformed.",
                ));
            }
        };
        normalized.push(json!({
            "positionId": position_id,
            "instrumentId": instrument_id,
            "isLong": is_long,
            "useDebt": use_debt,
            "entryPriceRaw": entry_price,
            "marginRaw": margin,
            "leverage": leverage,
            "bustPriceRaw": bust_price,
            "openedAtMs": opened_at_ms,
            "intentId": intent_id
        }));
    }
    let block = snapshot
        .get("indexedThroughBlock")
        .and_then(parse_block_number)
        .ok_or_else(|| invalid_response("Papertrade wallet snapshot omitted its indexed block."))?;
    Ok(json!({
        "owner": checked_owner(owner)?,
        "indexedThroughBlock": block.to_string(),
        "positions": normalized
    }))
}

fn exact_uint(value: &Value, field: &str) -> Result<String> {
    let raw = value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
        .ok_or_else(|| invalid_response(&format!("Papertrade {field} is not an exact uint.")))?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_response(&format!(
            "Papertrade {field} is not a decimal uint."
        )));
    }
    Ok(raw)
}

fn exact_small_uint(value: &Value, field: &str) -> Result<u64> {
    exact_uint(value, field)?
        .parse::<u64>()
        .map_err(|_| invalid_response(&format!("Papertrade {field} exceeds uint64.")))
}

fn action_disabled(trading: &Value, action: &str) -> Result<bool> {
    let intake = trading
        .get("intakeControls")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("Trading state omitted intake controls."))?;
    let disabled = intake
        .get(2)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("Trading intake controls omitted disabled actions."))?;
    if disabled.iter().any(|entry| entry.as_str().is_none()) {
        return Err(invalid_response(
            "Trading intake disabled-action list is malformed.",
        ));
    }
    Ok(disabled.iter().any(|entry| entry.as_str() == Some(action)))
}

fn parse_block_number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let value = value.as_str()?;
        value
            .strip_prefix("0x")
            .and_then(|raw| u64::from_str_radix(raw, 16).ok())
            .or_else(|| value.parse().ok())
    })
}

fn decode_u256_result(value: &str) -> Result<String> {
    let raw = value
        .strip_prefix("0x")
        .ok_or_else(|| invalid_response("PaperStaking.rewardReserve returned malformed hex."))?;
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_response(
            "PaperStaking.rewardReserve returned a noncanonical uint256.",
        ));
    }
    let parsed = BigUint::parse_bytes(raw.as_bytes(), 16).ok_or_else(|| {
        invalid_response("PaperStaking.rewardReserve returned an invalid uint256.")
    })?;
    Ok(parsed.to_string())
}

fn filter_since(history: &mut Value, since_ms: u64) -> Result<()> {
    let items = if let Some(items) = history.as_array_mut() {
        items
    } else if let Some(items) = history.get_mut("trades").and_then(Value::as_array_mut) {
        items
    } else if let Some(items) = history.get_mut("items").and_then(Value::as_array_mut) {
        items
    } else {
        return Err(invalid_response(
            "Papertrade trade history has an unrecognized response shape.",
        ));
    };
    items.retain(|item| {
        item.get("timestamp")
            .or_else(|| item.get("createdAt"))
            .and_then(Value::as_u64)
            .is_some_and(|timestamp| timestamp >= since_ms)
    });
    Ok(())
}

fn checked_owner(owner: &str) -> Result<String> {
    if owner.len() != 42
        || !owner.starts_with("0x")
        || !owner[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(GoFinError::validation(
            "Papertrade owner address is invalid.",
        ));
    }
    Ok(owner.to_ascii_lowercase())
}

fn raw_decimal(value: Option<&Value>, field: &str) -> Result<String> {
    let raw = value
        .and_then(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .or_else(|| value.as_u64().map(|number| number.to_string()))
        })
        .ok_or_else(|| invalid_response(&format!("Papertrade response omitted {field}.")))?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_response(&format!(
            "Papertrade response has invalid {field}."
        )));
    }
    Ok(raw)
}

fn parse_uint(value: &str) -> Result<BigUint> {
    BigUint::parse_bytes(value.as_bytes(), 10)
        .ok_or_else(|| invalid_response("Papertrade staking amount is not an integer."))
}

fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "PAPERTRADE_UNAVAILABLE",
        "transport",
        message,
        "Retry the read after checking Papertrade service connectivity.",
    )
}

fn invalid_response(message: &str) -> GoFinError {
    GoFinError::new(
        "PAPERTRADE_INVALID_RESPONSE",
        "observation",
        message,
        "Refresh the official Papertrade API response before continuing.",
    )
}

/// Concrete protected runtime used by the Papertrade adapter. It owns the
/// credential resolution, durable nonce allocation, typed signing and
/// canonical EVM observation boundary; neither secrets nor signatures leave it.
pub(crate) struct BrokerPapertradeRuntime {
    account: PapertradeAccount,
    wallet_credential: gofin_credentials::CredentialRef,
    credentials: Arc<dyn crate::BrokerCredentialProvider>,
    evm: Arc<gofin_evm::EvmRuntime>,
    observer: PapertradeChainObserver,
    http: PapertradeHttp,
    valuator: Arc<dyn crate::AssetValuator>,
    execution_state: Mutex<Option<Arc<dyn gofin_domain::execution_state::ExecutionState>>>,
    funding_host: Arc<crate::papertrade_funding_host::PapertradeFundingHostAdapter>,
}

impl BrokerPapertradeRuntime {
    #[allow(clippy::too_many_arguments)] // Composition binds independently protected capabilities.
    pub(crate) fn new(
        account: PapertradeAccount,
        wallet_credential: gofin_credentials::CredentialRef,
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        evm: Arc<gofin_evm::EvmRuntime>,
        confirmations: u64,
        require_finalized: bool,
        valuator: Arc<dyn crate::AssetValuator>,
        funding_host: Arc<crate::papertrade_funding_host::PapertradeFundingHostAdapter>,
    ) -> Result<Self> {
        Self::new_with_http(
            account,
            wallet_credential,
            credentials,
            evm,
            confirmations,
            require_finalized,
            valuator,
            funding_host,
            PapertradeHttp::new()?,
        )
    }

    #[allow(clippy::too_many_arguments)] // The HTTP override is limited to controlled fixtures.
    pub(crate) fn new_with_http(
        account: PapertradeAccount,
        wallet_credential: gofin_credentials::CredentialRef,
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        evm: Arc<gofin_evm::EvmRuntime>,
        confirmations: u64,
        require_finalized: bool,
        valuator: Arc<dyn crate::AssetValuator>,
        funding_host: Arc<crate::papertrade_funding_host::PapertradeFundingHostAdapter>,
        http: PapertradeHttp,
    ) -> Result<Self> {
        account.validate()?;
        if evm.chain_id != gofin_papertrade::CHAIN_ID {
            return Err(GoFinError::validation(
                "Papertrade requires its protected HyperEVM chain-999 runtime.",
            ));
        }
        let observer = PapertradeChainObserver::new(
            evm.rpc.clone(),
            evm.clone(),
            confirmations,
            require_finalized,
        )?;
        Ok(Self {
            account,
            wallet_credential,
            credentials,
            evm,
            observer,
            http,
            valuator,
            execution_state: Mutex::new(None),
            funding_host,
        })
    }

    pub(crate) fn attach_funding_route_runtime(self: &Arc<Self>) -> Result<()> {
        self.funding_host.attach_runtime(Arc::downgrade(self))
    }

    pub(crate) async fn funding_latest_block(&self) -> Result<u64> {
        let value = self.evm.rpc.request("eth_blockNumber", json!([])).await?;
        value
            .as_str()
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| invalid_response("HyperEVM returned an invalid block number."))
    }

    async fn ensure_withdrawable_cash(&self, amount_atoms: u64) -> Result<()> {
        let snapshot = self
            .http
            .read(
                ReadRequest::AccountState {
                    owner: self.account.owner.clone(),
                },
                self.evm.rpc.as_ref(),
            )
            .await?;
        let raw = snapshot
            .get("funds")
            .and_then(|funds| funds.get("availableBalance"))
            .and_then(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| value.as_u64().map(|number| number.to_string()))
            })
            .ok_or_else(|| {
                invalid_response("Papertrade wallet snapshot omitted exact availableBalance.")
            })?;
        let available = raw.parse::<u128>().map_err(|_| {
            invalid_response("Papertrade availableBalance is not an exact unsigned integer.")
        })?;
        let required = u128::from(amount_atoms)
            .checked_mul(1_000_000_000_000)
            .ok_or_else(|| GoFinError::validation("Core withdrawal amount overflowed."))?;
        if required > available {
            return Err(GoFinError::new(
                "PAPERTRADE_INSUFFICIENT_AVAILABLE_CASH",
                "preflight",
                "Available Papertrade USDC cash no longer covers the requested withdrawal.",
                "Reduce the withdrawal to the current available balance and prepare it again.",
            ));
        }
        Ok(())
    }

    pub(crate) async fn canonical_block_timestamp_ms(
        &self,
        block_number: u64,
        expected_block_hash: &str,
    ) -> Result<Option<u64>> {
        let tag = format!("0x{block_number:x}");
        let block = self
            .evm
            .rpc
            .request("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        if block
            .get("hash")
            .and_then(Value::as_str)
            .is_none_or(|hash| !hash.eq_ignore_ascii_case(expected_block_hash))
        {
            return Ok(None);
        }
        let timestamp = block
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok());
        Ok(timestamp.and_then(|seconds| seconds.checked_mul(1_000)))
    }

    /// Prepare the owner-only Papertrade `WithdrawToCore` typed intent used by
    /// the funding lifecycle. The withdrawal amount is the venue's six-decimal
    /// USDC atom count (unlike internal trading cash, which is 18 decimals).
    /// A stable operation id reuses the original frozen signer, nonce and
    /// deadline across preview/recovery; it never allocates a replacement on
    /// an uncertain submission.
    pub(crate) async fn prepare_funding_withdraw_to_core(
        &self,
        operation_id: &str,
        amount_atoms: u64,
    ) -> Result<BoundIntent> {
        if operation_id.is_empty()
            || operation_id.len() > 160
            || !operation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
            || amount_atoms == 0
        {
            return Err(GoFinError::validation(
                "Papertrade Core withdrawal identity or amount is invalid.",
            ));
        }
        self.ensure_withdrawable_cash(amount_atoms).await?;
        let record_key = format!("papertrade-funding-withdrawal-{}", operation_id);
        if let Some(value) = self.credentials.papertrade_runtime_record(&record_key)? {
            let bound: BoundIntent = serde_json::from_value(value)
                .map_err(|_| invalid_response("Protected Core withdrawal binding is malformed."))?;
            if !matches!(
                &bound.typed,
                PapertradeTypedIntent::WithdrawToCore { user, amount, .. }
                    if user.eq_ignore_ascii_case(&self.account.owner)
                        && amount == &amount_atoms.to_string()
            ) || bound.action.as_deref() != Some("withdrawToCore")
                || bound.cancel_target_action.is_some()
                || bound.registration_id.is_some()
            {
                return Err(GoFinError::validation(
                    "Papertrade operation id is already bound to different withdrawal terms.",
                ));
            }
            bound
                .typed
                .validate(chrono::Utc::now().timestamp().max(0) as u64)?;
            self.credentials.put_papertrade_runtime_record(
                &format!("papertrade-funding-withdrawal-intent-{}", bound.intent_id),
                &serde_json::to_value(&bound)
                    .map_err(|_| unavailable("Core withdrawal identity could not be persisted."))?,
            )?;
            return Ok(bound);
        }

        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let deadline = funding_withdrawal_deadline(now)?;
        let nonce = self
            .allocate_nonce(
                &self.account.owner,
                gofin_papertrade::PapertradeDomainTarget::Exchange,
            )
            .await?;
        let typed = PapertradeTypedIntent::WithdrawToCore {
            user: self.account.owner.clone(),
            amount: amount_atoms.to_string(),
            nonce,
            deadline,
        };
        let signer = self.owner_signer(&self.account.owner)?;
        let bound = self.make_bound(typed, signer.as_ref(), now, None)?;
        self.credentials.put_papertrade_runtime_record(
            &record_key,
            &serde_json::to_value(&bound)
                .map_err(|_| unavailable("Core withdrawal identity could not be persisted."))?,
        )?;
        self.credentials.put_papertrade_runtime_record(
            &format!("papertrade-funding-withdrawal-intent-{}", bound.intent_id),
            &serde_json::to_value(&bound)
                .map_err(|_| unavailable("Core withdrawal identity could not be persisted."))?,
        )?;
        Ok(bound)
    }

    /// Dispatch only the exact protected owner-signed withdrawal binding
    /// prepared above. The outer plan has already journaled its immutable
    /// funding identity before this can be called.
    pub(crate) async fn dispatch_funding_withdraw_to_core(
        &self,
        operation_id: &str,
        bound: &BoundIntent,
    ) -> Result<Receipt> {
        let key = format!("papertrade-funding-withdrawal-intent-{}", operation_id);
        let stored = self
            .credentials
            .papertrade_runtime_record(&key)?
            .ok_or_else(|| unavailable("Protected Core withdrawal binding is unavailable."))?;
        let stored: BoundIntent = serde_json::from_value(stored)
            .map_err(|_| invalid_response("Protected Core withdrawal binding is malformed."))?;
        if stored != *bound
            || !matches!(bound.typed, PapertradeTypedIntent::WithdrawToCore { .. })
            || !bound.signer.eq_ignore_ascii_case(&self.account.owner)
        {
            return Err(GoFinError::validation(
                "Core withdrawal dispatch differs from its protected owner-signed binding.",
            ));
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        bound.typed.validate(now)?;
        let PapertradeTypedIntent::WithdrawToCore { nonce, amount, .. } = &bound.typed else {
            unreachable!("variant checked above")
        };
        let amount_atoms = amount
            .parse::<u64>()
            .map_err(|_| invalid_response("Core withdrawal amount is malformed."))?;
        self.ensure_withdrawable_cash(amount_atoms).await?;
        let nonce = nonce
            .parse::<u64>()
            .map_err(|_| invalid_response("Core withdrawal nonce is malformed."))?;
        if self
            .nonce_used_at(
                gofin_papertrade::SESSION_KEY_MANAGER,
                &self.account.owner,
                nonce,
            )
            .await?
        {
            return Err(GoFinError::validation(
                "Core withdrawal nonce has already been consumed; the original submission must be reconciled.",
            ));
        }
        self.validate_mutable_evidence(&bound.typed).await?;
        let signer = self.owner_signer(&self.account.owner)?;
        let (address, intent_id) =
            signer.papertrade_intent_identity(&to_evm_intent(&bound.typed), now)?;
        if !address.eq_ignore_ascii_case(&bound.signer)
            || !intent_id.eq_ignore_ascii_case(&bound.intent_id)
        {
            return Err(GoFinError::validation(
                "Core withdrawal signature identity changed after preparation.",
            ));
        }
        self.http.submit_typed(bound, signer.as_ref()).await
    }

    pub(crate) async fn dispatch_funding_intent(
        &self,
        bound: &BoundIntent,
        outer_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Receipt> {
        journal.assert_candidate(validated.prepared())?;
        journal.assert_authorized()?;
        validated.assert_fresh()?;
        if !matches!(bound.typed, PapertradeTypedIntent::WithdrawToCore { .. })
            || outer_submission.kind != "papertrade_funding"
        {
            return Err(GoFinError::validation(
                "Funding intent dispatch is not bound to a Papertrade Core withdrawal.",
            ));
        }
        let stored: BoundIntent = serde_json::from_value(
            self.credentials
                .papertrade_runtime_record(&format!(
                    "papertrade-funding-withdrawal-intent-{}",
                    bound.intent_id
                ))?
                .ok_or_else(|| unavailable("Protected Core withdrawal binding is unavailable."))?,
        )
        .map_err(|_| invalid_response("Protected Core withdrawal binding is malformed."))?;
        if stored != *bound {
            return Err(GoFinError::validation(
                "Core withdrawal binding differs from its intent-indexed protected record.",
            ));
        }
        let receipt = self
            .dispatch_funding_withdraw_to_core(&bound.intent_id, bound)
            .await?;
        // The adapter owns the outer submission's single immutable dispatch
        // evidence record. Persisting a second route-specific value here
        // conflicts with that record and loses a valid relayer acceptance.
        Ok(receipt)
    }

    fn execution_state(&self) -> Result<Arc<dyn gofin_domain::execution_state::ExecutionState>> {
        self.execution_state
            .lock()
            .map_err(|_| unavailable("Papertrade execution state lock is poisoned."))?
            .clone()
            .ok_or_else(|| unavailable("Papertrade durable execution state is not attached."))
    }

    fn owner_signer(&self, owner: &str) -> Result<Arc<gofin_evm::LocalEnvelopeSigner>> {
        if !owner.eq_ignore_ascii_case(&self.account.owner) {
            return Err(GoFinError::validation(
                "Papertrade request owner differs from the enrolled wallet.",
            ));
        }
        self.credentials
            .papertrade_wallet_signer(&self.wallet_credential, owner)
    }

    async fn active_session_signer(
        &self,
        owner: &str,
        now: u64,
    ) -> Result<(String, Arc<gofin_evm::LocalEnvelopeSigner>, String, u64)> {
        let mut sessions = self.credentials.papertrade_sessions(owner)?;
        sessions.retain(|session| {
            session.revoked_at.is_none() && session.registered_at > 0 && session.expires_at > now
        });
        sessions.sort_by_key(|session| (session.registered_at, session.registration_id.clone()));
        let session = sessions.pop().ok_or_else(|| {
            GoFinError::new(
                "PAPERTRADE_SESSION_REQUIRED",
                "credentials",
                "No active Papertrade session key is registered for this wallet.",
                "Register a bounded Papertrade session through the protected configuration workflow.",
            )
        })?;
        self.session_signer(owner, &session.registration_id, now)
            .await
    }

    async fn session_signer(
        &self,
        owner: &str,
        registration_id: &str,
        now: u64,
    ) -> Result<(String, Arc<gofin_evm::LocalEnvelopeSigner>, String, u64)> {
        let session = self
            .credentials
            .papertrade_sessions(owner)?
            .into_iter()
            .find(|session| {
                session.registration_id == registration_id
                    && session.owner_address.eq_ignore_ascii_case(owner)
                    && session.revoked_at.is_none()
                    && session.registered_at > 0
                    && session.expires_at > now
            })
            .ok_or_else(|| {
                GoFinError::validation("Bound Papertrade session is not active or has expired.")
            })?;
        let (_, address, signer) = self.credentials.papertrade_session_signer(
            registration_id,
            owner,
            session.expires_at,
        )?;
        if !address.starts_with("0x") {
            return Err(unavailable(
                "Papertrade session signer identity is malformed.",
            ));
        }
        let (chain_expiry, fee_address) = self.read_session_key(owner, &address).await?;
        if chain_expiry != session.expires_at
            || fee_address != "0x0000000000000000000000000000000000000000"
        {
            return Err(GoFinError::new(
                "PAPERTRADE_SESSION_NOT_REGISTERED",
                "credentials",
                "The locally registered Papertrade session expiry or fee address differs from its on-chain registration.",
                "Reconcile its canonical registration or register a new bounded session.",
            ));
        }
        Ok((
            registration_id.to_owned(),
            signer,
            address,
            session.expires_at,
        ))
    }

    async fn read_session_key(&self, owner: &str, key: &str) -> Result<(u64, String)> {
        let owner_bytes = hex::decode(owner.strip_prefix("0x").unwrap_or_default())
            .map_err(|_| GoFinError::validation("Papertrade owner address is malformed."))?;
        let key_bytes = hex::decode(key.strip_prefix("0x").unwrap_or_default())
            .map_err(|_| GoFinError::validation("Papertrade session address is malformed."))?;
        if owner_bytes.len() != 20 || key_bytes.len() != 20 {
            return Err(GoFinError::validation(
                "Papertrade session address is malformed.",
            ));
        }
        let mut calldata = hex::decode("96ade1f9").expect("fixed selector");
        calldata.extend_from_slice(&[0; 12]);
        calldata.extend_from_slice(&owner_bytes);
        calldata.extend_from_slice(&[0; 12]);
        calldata.extend_from_slice(&key_bytes);
        let response = self
            .evm
            .rpc
            .request(
                "eth_call",
                json!([{
            "to": gofin_papertrade::SESSION_KEY_MANAGER,
            "data": format!("0x{}", hex::encode(calldata)),
        }, "latest"]),
            )
            .await?;
        let bytes = response
            .as_str()
            .and_then(|v| v.strip_prefix("0x"))
            .and_then(|v| hex::decode(v).ok())
            .ok_or_else(|| {
                invalid_response("SessionKeyManager sessionKeys response is malformed.")
            })?;
        if bytes.len() != 64
            || bytes[..24].iter().any(|v| *v != 0)
            || bytes[32..44].iter().any(|v| *v != 0)
        {
            return Err(invalid_response(
                "SessionKeyManager sessionKeys returned malformed expiry or fee fields.",
            ));
        }
        let expiry = u64::from_be_bytes(
            bytes[24..32]
                .try_into()
                .map_err(|_| invalid_response("Session expiry is malformed."))?,
        );
        Ok((expiry, format!("0x{}", hex::encode(&bytes[44..64]))))
    }

    async fn allocate_nonce(
        &self,
        owner: &str,
        target: gofin_papertrade::PapertradeDomainTarget,
    ) -> Result<String> {
        use gofin_papertrade::PapertradeDomainTarget as D;
        let (contract, kind) = match target {
            D::Exchange => (gofin_papertrade::EXCHANGE, "session"),
            D::PaperStaking => (gofin_papertrade::PAPER_STAKING, "paper"),
            D::Cancellation => (gofin_papertrade::EXCHANGE, "session"),
        };
        let scope = format!(
            "papertrade:999:{contract}:{kind}:{}",
            owner.to_ascii_lowercase()
        );
        let lane_record = format!(
            "papertrade-nonce-lane-{}",
            hex::encode(gofin_evm::alloy::primitives::keccak256(scope.as_bytes()))
        );
        let seed = match self.credentials.papertrade_runtime_record(&lane_record)? {
            Some(value) => {
                if value.get("version").and_then(Value::as_u64) != Some(1) {
                    return Err(invalid_response(
                        "Protected Papertrade nonce lane version is unsupported.",
                    ));
                }
                value
                    .get("seed")
                    .and_then(Value::as_u64)
                    .filter(|seed| *seed > 0 && *seed <= 0xffff_ff00 && seed % 256 == 0)
                    .ok_or_else(|| {
                        invalid_response("Protected Papertrade nonce lane seed is malformed.")
                    })?
            }
            None => {
                let prefix =
                    u32::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[..4].try_into().unwrap())
                        & 0x00ff_ffff;
                let seed = u64::from(prefix.max(1)) << 8;
                self.credentials.put_papertrade_runtime_record(
                    &lane_record,
                    &json!({"version":1,"seed":seed}),
                )?;
                seed
            }
        };
        // The store persists and increments this lane before typed signing;
        // its first value has the frontend's random 24-bit-prefix shape.
        for _ in 0..4 {
            let nonce = self.execution_state()?.allocate_nonce(&scope, seed)?;
            let nonce_contract = match target {
                D::PaperStaking => gofin_papertrade::PAPER_STAKING,
                D::Exchange | D::Cancellation => gofin_papertrade::SESSION_KEY_MANAGER,
            };
            if !self.nonce_used_at(nonce_contract, owner, nonce).await? {
                return Ok(nonce.to_string());
            }
        }
        Err(GoFinError::new(
            "PAPERTRADE_NONCE_CONFLICT",
            "preview",
            "Allocated Papertrade nonces are already marked used on chain.",
            "Refresh the protected nonce lane and prepare a new operation.",
        ))
    }

    async fn nonce_used_at(&self, contract: &str, owner: &str, nonce: u64) -> Result<bool> {
        self.nonce_used_at_block(contract, owner, nonce, "latest")
            .await
    }

    async fn nonce_used_at_block(
        &self,
        contract: &str,
        owner: &str,
        nonce: u64,
        block_tag: &str,
    ) -> Result<bool> {
        let owner = hex::decode(owner.strip_prefix("0x").unwrap_or_default())
            .map_err(|_| GoFinError::validation("Papertrade owner address is malformed."))?;
        if owner.len() != 20 {
            return Err(GoFinError::validation(
                "Papertrade owner address is malformed.",
            ));
        }
        let mut calldata = hex::decode("cab7e8eb").expect("fixed selector");
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(&owner);
        calldata.extend_from_slice(&[0u8; 24]);
        calldata.extend_from_slice(&nonce.to_be_bytes());
        let response = self
            .evm
            .rpc
            .request(
                "eth_call",
                json!([{
            "to": contract, "data": format!("0x{}", hex::encode(calldata))
        }, block_tag]),
            )
            .await?;
        let bytes = response
            .as_str()
            .and_then(|s| s.strip_prefix("0x"))
            .and_then(|s| hex::decode(s).ok())
            .ok_or_else(|| invalid_response("Papertrade nonce response is malformed."))?;
        if bytes.len() != 32 || bytes[..31].iter().any(|byte| *byte != 0) || bytes[31] > 1 {
            return Err(invalid_response("Papertrade nonce response is not a bool."));
        }
        Ok(bytes[31] == 1)
    }

    async fn session_registration_expiry_proof(
        &self,
        binding: &gofin_papertrade::SessionRegistrationBinding,
    ) -> Result<Option<gofin_papertrade::SessionRegistrationExpiryProof>> {
        if !binding.owner.eq_ignore_ascii_case(&self.account.owner)
            || binding.owner.len() != 42
            || !binding.owner.starts_with("0x")
            || !binding.owner[2..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(GoFinError::validation(
                "Session registration expiry owner is malformed.",
            ));
        }
        let nonce = binding
            .nonce
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Session registration nonce is malformed."))?;
        let Some(protected_intent) = self.session_registration_intent(binding).await? else {
            // The public binding intentionally omits the generated session
            // address. Without the protected typed registration we cannot
            // query the exact key whose registration is being reconciled.
            return Ok(None);
        };
        let PapertradeTypedIntent::RegisterSessionKey {
            user,
            key: session_key,
            expiry,
            nonce: intent_nonce,
            deadline,
            ..
        } = &protected_intent.typed
        else {
            return Err(GoFinError::validation(
                "Protected session registration has the wrong typed intent.",
            ));
        };
        if !user.eq_ignore_ascii_case(&binding.owner)
            || !protected_intent.signer.eq_ignore_ascii_case(&binding.owner)
            || protected_intent.intent_id != binding.intent_id
            || intent_nonce != &binding.nonce
            || *deadline != binding.deadline
            || *expiry != binding.expiry
            || session_key.len() != 42
            || !session_key.starts_with("0x")
            || !session_key[2..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(GoFinError::validation(
                "Protected registration does not match the exact owner, key, nonce and deadline.",
            ));
        }

        // Query a block whose finality policy is at least as strong as the
        // observer's transaction policy, then pin every contract read to its
        // exact number. Never mix a latest nonce read with an older header.
        let (number, header_tag) = if self.observer.require_finalized {
            let block = self
                .evm
                .rpc
                .request("eth_getBlockByNumber", json!(["finalized", false]))
                .await?;
            let number = block
                .get("number")
                .ok_or_else(|| invalid_response("Finalized HyperEVM header omitted its number."))?;
            (rpc_hex_u64(number)?, block)
        } else {
            let head = self.evm.rpc.request("eth_blockNumber", json!([])).await?;
            let head = rpc_hex_u64(&head)?;
            let number = head.saturating_sub(self.observer.confirmations.saturating_sub(1));
            let block = self
                .evm
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([format!("0x{number:x}"), false]),
                )
                .await?;
            (number, block)
        };
        let hash = header_tag
            .get("hash")
            .and_then(Value::as_str)
            .filter(|hash| {
                hash.len() == 66
                    && hash.starts_with("0x")
                    && hash[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| invalid_response("Canonical HyperEVM header hash is malformed."))?
            .to_ascii_lowercase();
        let timestamp = header_tag
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| invalid_response("Canonical HyperEVM header timestamp is malformed."))?;
        if number == 0 || timestamp <= binding.deadline {
            return Ok(None);
        }
        let block_tag = format!("0x{number:x}");
        // Re-read by number and compare hash so a reorg during proof assembly
        // cannot combine state from a different block.
        let canonical = self
            .evm
            .rpc
            .request("eth_getBlockByNumber", json!([block_tag, false]))
            .await?;
        if canonical
            .get("hash")
            .and_then(Value::as_str)
            .is_none_or(|candidate| !candidate.eq_ignore_ascii_case(&hash))
        {
            return Ok(None);
        }

        let (implementation, code_hash) = self
            .observer
            .implementation_identity(gofin_papertrade::SESSION_KEY_MANAGER, number)
            .await?;
        if !implementation
            .eq_ignore_ascii_case(gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION)
            || !code_hash.eq_ignore_ascii_case(gofin_papertrade::SESSION_KEY_MANAGER_CODE_HASH)
        {
            return Ok(None);
        }
        let nonce_unused = !self
            .nonce_used_at_block(
                gofin_papertrade::SESSION_KEY_MANAGER,
                &binding.owner,
                nonce,
                &block_tag,
            )
            .await?;
        // sessionKeys(address,address) returns packed (uint64 expiry,address
        // feeAddress), each ABI word is checked independently. A registered
        // session contradicts the claimed never-accepted registration.
        let owner = hex::decode(binding.owner.strip_prefix("0x").unwrap_or_default())
            .map_err(|_| GoFinError::validation("Session registration owner is malformed."))?;
        let key = hex::decode(session_key.strip_prefix("0x").unwrap_or_default())
            .map_err(|_| GoFinError::validation("Session registration key is malformed."))?;
        if owner.len() != 20 || key.len() != 20 {
            return Err(GoFinError::validation(
                "Session registration address is malformed.",
            ));
        }
        let mut calldata = hex::decode("96ade1f9").expect("fixed selector");
        calldata.extend_from_slice(&[0; 12]);
        calldata.extend_from_slice(&owner);
        calldata.extend_from_slice(&[0; 12]);
        calldata.extend_from_slice(&key);
        let raw = self
            .evm
            .rpc
            .request(
                "eth_call",
                json!([{
                    "to": gofin_papertrade::SESSION_KEY_MANAGER,
                    "data": format!("0x{}", hex::encode(calldata))
                }, block_tag]),
            )
            .await?;
        let words = raw
            .as_str()
            .and_then(|raw| raw.strip_prefix("0x"))
            .and_then(|raw| hex::decode(raw).ok())
            .filter(|bytes| bytes.len() == 64)
            .ok_or_else(|| {
                invalid_response("SessionKeyManager sessionKeys result is malformed.")
            })?;
        let session_absent = words.iter().all(|byte| *byte == 0);
        if !nonce_unused || !session_absent {
            return Ok(None);
        }
        let canonical_after_reads = self
            .evm
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        if canonical_after_reads
            .get("hash")
            .and_then(Value::as_str)
            .is_none_or(|candidate| !candidate.eq_ignore_ascii_case(&hash))
        {
            return Ok(None);
        }
        let proof = gofin_papertrade::SessionRegistrationExpiryProof {
            chain_id: self.evm.chain_id,
            owner: binding.owner.to_ascii_lowercase(),
            registration_id: binding.registration_id.clone(),
            intent_id: binding.intent_id.to_ascii_lowercase(),
            nonce: binding.nonce.clone(),
            deadline: binding.deadline,
            canonical_block_number: number,
            canonical_block_hash: hash,
            canonical_block_timestamp: timestamp,
            session_key_manager_proxy: gofin_papertrade::SESSION_KEY_MANAGER.into(),
            session_key_manager_implementation: implementation,
            session_key_manager_code_hash: code_hash,
            nonce_unused,
            session_absent,
        };
        proof.validate_against(binding)?;
        Ok(Some(proof))
    }

    async fn core_withdrawal_expiry_proof(
        &self,
        bound: &BoundIntent,
    ) -> Result<Option<gofin_papertrade::CoreWithdrawalExpiryProof>> {
        let PapertradeTypedIntent::WithdrawToCore {
            user,
            nonce,
            deadline,
            ..
        } = &bound.typed
        else {
            return Err(GoFinError::validation(
                "Core withdrawal expiry proof requires a WithdrawToCore intent.",
            ));
        };
        if !user.eq_ignore_ascii_case(&self.account.owner)
            || !bound.signer.eq_ignore_ascii_case(&self.account.owner)
            || bound.action.as_deref() != Some("withdrawToCore")
        {
            return Err(GoFinError::validation(
                "Core withdrawal expiry proof differs from the enrolled owner identity.",
            ));
        }
        let nonce = nonce
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Core withdrawal nonce is malformed."))?;
        let target_timestamp = deadline.saturating_sub(gofin_papertrade::MAX_INTENT_SECONDS);

        // Expiry is proven at the earliest finalized/confirmed block after
        // the signed deadline. That block is enough: the exact intent could
        // no longer execute after its deadline, and it bounds log history to
        // the signed lifetime even after long downtime.
        let Some((number, header)) = self.canonical_block_after_deadline(*deadline).await? else {
            return Ok(None);
        };
        let hash = header
            .get("hash")
            .and_then(Value::as_str)
            .filter(|hash| {
                hash.len() == 66
                    && hash.starts_with("0x")
                    && hash[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| invalid_response("Canonical HyperEVM header hash is malformed."))?
            .to_ascii_lowercase();
        let timestamp = header
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| invalid_response("Canonical HyperEVM header timestamp is malformed."))?;
        if number == 0 || timestamp <= *deadline {
            return Ok(None);
        }
        let block_tag = format!("0x{number:x}");

        let (exchange_implementation, exchange_code_hash) = self
            .observer
            .implementation_identity(gofin_papertrade::EXCHANGE, number)
            .await?;
        if !exchange_implementation.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE_IMPLEMENTATION)
            || !exchange_code_hash.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE_CODE_HASH)
        {
            return Ok(None);
        }
        let (manager_implementation, manager_code_hash) = self
            .observer
            .implementation_identity(gofin_papertrade::SESSION_KEY_MANAGER, number)
            .await?;
        if !manager_implementation
            .eq_ignore_ascii_case(gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION)
            || !manager_code_hash
                .eq_ignore_ascii_case(gofin_papertrade::SESSION_KEY_MANAGER_CODE_HASH)
        {
            return Ok(None);
        }

        let nonce_unused = !self
            .nonce_used_at_block(
                gofin_papertrade::SESSION_KEY_MANAGER,
                user,
                nonce,
                &block_tag,
            )
            .await?;
        if !nonce_unused {
            return Ok(None);
        }

        // Every version of this broker allocated owner intent deadlines no
        // more than MAX_INTENT_SECONDS after preparation. Locate the block at
        // or immediately before that earliest possible preparation time. The
        // binary search is logarithmic in chain height; log scanning is then
        // bounded to this signed-intent lifetime and uses the observer's
        // 200-block pages. Absence from balances is never consulted.
        let from_block = self
            .block_at_or_before_timestamp(number, target_timestamp)
            .await?;
        if number.saturating_sub(from_block) > 10_000
            || self
                .observer
                .has_intent_event_through(&bound.intent_id, from_block, number)
                .await?
        {
            return Ok(None);
        }
        let canonical = self
            .evm
            .rpc
            .request("eth_getBlockByNumber", json!([block_tag, false]))
            .await?;
        if canonical
            .get("hash")
            .and_then(Value::as_str)
            .is_none_or(|candidate| !candidate.eq_ignore_ascii_case(&hash))
        {
            return Ok(None);
        }
        let proof = gofin_papertrade::CoreWithdrawalExpiryProof {
            chain_id: self.evm.chain_id,
            owner: user.to_ascii_lowercase(),
            intent_id: bound.intent_id.to_ascii_lowercase(),
            nonce: match &bound.typed {
                PapertradeTypedIntent::WithdrawToCore { nonce, .. } => nonce.clone(),
                _ => unreachable!("variant checked above"),
            },
            deadline: *deadline,
            canonical_block_number: number,
            canonical_block_hash: hash,
            canonical_block_timestamp: timestamp,
            exchange_proxy: gofin_papertrade::EXCHANGE.into(),
            exchange_implementation,
            exchange_code_hash,
            session_key_manager_proxy: gofin_papertrade::SESSION_KEY_MANAGER.into(),
            session_key_manager_implementation: manager_implementation,
            session_key_manager_code_hash: manager_code_hash,
            nonce_unused,
            intent_absent: true,
        };
        proof.validate_against(bound)?;
        Ok(Some(proof))
    }

    async fn intent_expiry_proof(
        &self,
        bound: &BoundIntent,
    ) -> Result<Option<gofin_papertrade::PapertradeIntentExpiryProof>> {
        use PapertradeTypedIntent as I;
        let (
            owner,
            nonce,
            deadline,
            target,
            target_impl,
            target_hash,
            nonce_contract,
            nonce_impl,
            nonce_hash,
        ) = match &bound.typed {
            I::OpenPosition {
                user,
                nonce,
                deadline,
                ..
            }
            | I::Close {
                user,
                nonce,
                deadline,
                ..
            } => (
                user,
                nonce,
                *deadline,
                gofin_papertrade::EXCHANGE,
                gofin_papertrade::EXCHANGE_IMPLEMENTATION,
                gofin_papertrade::EXCHANGE_CODE_HASH,
                gofin_papertrade::SESSION_KEY_MANAGER,
                gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
                gofin_papertrade::SESSION_KEY_MANAGER_CODE_HASH,
            ),
            I::Stake {
                user,
                nonce,
                deadline,
                ..
            }
            | I::Unstake {
                user,
                nonce,
                deadline,
                ..
            }
            | I::Claim {
                user,
                nonce,
                deadline,
            } => (
                user,
                nonce,
                *deadline,
                gofin_papertrade::PAPER_STAKING,
                gofin_papertrade::PAPER_STAKING_IMPLEMENTATION,
                gofin_papertrade::PAPER_STAKING_CODE_HASH,
                gofin_papertrade::PAPER_STAKING,
                gofin_papertrade::PAPER_STAKING_IMPLEMENTATION,
                gofin_papertrade::PAPER_STAKING_CODE_HASH,
            ),
            I::CancelIntent { .. } | I::WithdrawToCore { .. } | I::RegisterSessionKey { .. } => {
                return Ok(None);
            }
        };
        if !owner.eq_ignore_ascii_case(&self.account.owner)
            || bound.intent_id.len() != 66
            || !bound.intent_id.starts_with("0x")
            || !bound.intent_id[2..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(GoFinError::validation(
                "Papertrade intent expiry identity differs from the enrolled owner.",
            ));
        }
        let nonce = nonce
            .parse::<u64>()
            .map_err(|_| GoFinError::validation("Papertrade intent expiry nonce is malformed."))?;
        let target_timestamp = deadline.saturating_sub(gofin_papertrade::MAX_INTENT_SECONDS);
        let Some((number, header)) = self.canonical_block_after_deadline(deadline).await? else {
            return Ok(None);
        };
        let hash = header
            .get("hash")
            .and_then(Value::as_str)
            .filter(|hash| {
                hash.len() == 66
                    && hash.starts_with("0x")
                    && hash[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or_else(|| invalid_response("Canonical HyperEVM header hash is malformed."))?
            .to_ascii_lowercase();
        let timestamp = header
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| invalid_response("Canonical HyperEVM timestamp is malformed."))?;
        if number == 0 || timestamp <= deadline {
            return Ok(None);
        }
        let block_tag = format!("0x{number:x}");
        let (target_implementation, target_code_hash) = self
            .observer
            .implementation_identity(target, number)
            .await?;
        if !target_implementation.eq_ignore_ascii_case(target_impl)
            || !target_code_hash.eq_ignore_ascii_case(target_hash)
        {
            return Ok(None);
        }
        let (nonce_implementation, nonce_code_hash) = if nonce_contract == target {
            (target_implementation.clone(), target_code_hash.clone())
        } else {
            self.observer
                .implementation_identity(nonce_contract, number)
                .await?
        };
        if !nonce_implementation.eq_ignore_ascii_case(nonce_impl)
            || !nonce_code_hash.eq_ignore_ascii_case(nonce_hash)
        {
            return Ok(None);
        }
        let nonce_unused = !self
            .nonce_used_at_block(nonce_contract, owner, nonce, &block_tag)
            .await?;
        if !nonce_unused {
            return Ok(None);
        }
        let from_block = self
            .block_at_or_before_timestamp(number, target_timestamp)
            .await?;
        if number.saturating_sub(from_block) > 10_000
            || self
                .observer
                .has_intent_event_through(&bound.intent_id, from_block, number)
                .await?
        {
            return Ok(None);
        }
        let (batch_implementation, batch_code_hash) = self
            .observer
            .implementation_identity(gofin_papertrade::BATCH_EXECUTOR, number)
            .await?;
        if !batch_implementation
            .eq_ignore_ascii_case(gofin_papertrade::BATCH_EXECUTOR_IMPLEMENTATION)
            || !batch_code_hash.eq_ignore_ascii_case(gofin_papertrade::BATCH_EXECUTOR_CODE_HASH)
        {
            return Ok(None);
        }
        let canonical = self
            .evm
            .rpc
            .request("eth_getBlockByNumber", json!([block_tag, false]))
            .await?;
        if canonical
            .get("hash")
            .and_then(Value::as_str)
            .is_none_or(|candidate| !candidate.eq_ignore_ascii_case(&hash))
        {
            return Ok(None);
        }
        let proof = gofin_papertrade::PapertradeIntentExpiryProof {
            chain_id: self.evm.chain_id,
            owner: owner.to_ascii_lowercase(),
            intent_id: bound.intent_id.to_ascii_lowercase(),
            nonce: nonce.to_string(),
            deadline,
            canonical_block_number: number,
            canonical_block_hash: hash,
            canonical_block_timestamp: timestamp,
            target_proxy: target.into(),
            target_implementation,
            target_code_hash,
            nonce_proxy: nonce_contract.into(),
            nonce_implementation,
            nonce_code_hash,
            batch_executor_proxy: gofin_papertrade::BATCH_EXECUTOR.into(),
            batch_executor_implementation: batch_implementation,
            batch_executor_code_hash: batch_code_hash,
            nonce_unused,
            intent_absent: true,
        };
        proof.validate_against(bound)?;
        Ok(Some(proof))
    }

    async fn canonical_block_after_deadline(&self, deadline: u64) -> Result<Option<(u64, Value)>> {
        let upper_block = if self.observer.require_finalized {
            let finalized = self
                .evm
                .rpc
                .request("eth_getBlockByNumber", json!(["finalized", false]))
                .await?;
            rpc_hex_u64(finalized.get("number").ok_or_else(|| {
                invalid_response("Finalized HyperEVM header omitted its number.")
            })?)?
        } else {
            let head = self.evm.rpc.request("eth_blockNumber", json!([])).await?;
            rpc_hex_u64(&head)?.saturating_sub(self.observer.confirmations.saturating_sub(1))
        };
        let Some(number) = self.block_after_timestamp(upper_block, deadline).await? else {
            return Ok(None);
        };
        let header = self
            .evm
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        let timestamp = header
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| invalid_response("Canonical HyperEVM timestamp is malformed."))?;
        if timestamp <= deadline {
            return Ok(None);
        }
        Ok(Some((number, header)))
    }

    async fn block_after_timestamp(&self, upper_block: u64, timestamp: u64) -> Result<Option<u64>> {
        let upper = self
            .evm
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("0x{upper_block:x}"), false]),
            )
            .await?;
        let upper_timestamp = upper
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| invalid_response("Confirmed HyperEVM timestamp is malformed."))?;
        if upper_timestamp <= timestamp {
            return Ok(None);
        }

        // Block timestamps are monotonic. Find the first block strictly after
        // the deadline so a later reconciliation does not rescan an ever
        // growing history window.
        let mut low = 0u64;
        let mut high = upper_block;
        while low < high {
            let mid = low + (high - low) / 2;
            let block = self
                .evm
                .rpc
                .request("eth_getBlockByNumber", json!([format!("0x{mid:x}"), false]))
                .await?;
            let observed = block
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
                .ok_or_else(|| {
                    invalid_response("HyperEVM historical block timestamp is malformed.")
                })?;
            if observed > timestamp {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        Ok(Some(low))
    }

    async fn block_at_or_before_timestamp(&self, upper_block: u64, timestamp: u64) -> Result<u64> {
        let mut low = 0u64;
        let mut high = upper_block;
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            let block = self
                .evm
                .rpc
                .request("eth_getBlockByNumber", json!([format!("0x{mid:x}"), false]))
                .await?;
            let observed = block
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
                .ok_or_else(|| {
                    invalid_response("HyperEVM historical block timestamp is malformed.")
                })?;
            if observed <= timestamp {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        Ok(low)
    }

    fn persist_bound_intent(&self, operation_id: &str, bound: &BoundIntent) -> Result<()> {
        let key = format!("papertrade-intent-{}", operation_id);
        self.credentials.put_papertrade_runtime_record(
            &key,
            &serde_json::to_value(bound)
                .map_err(|_| unavailable("Papertrade intent identity could not be persisted."))?,
        )
    }

    fn load_bound_intent(&self, operation_id: &str) -> Result<Option<BoundIntent>> {
        self.credentials
            .papertrade_runtime_record(&format!("papertrade-intent-{operation_id}"))?
            .map(|value| {
                serde_json::from_value(value).map_err(|_| {
                    invalid_response("Protected Papertrade intent record is malformed.")
                })
            })
            .transpose()
    }

    fn make_bound(
        &self,
        typed: PapertradeTypedIntent,
        signer: &gofin_evm::LocalEnvelopeSigner,
        now: u64,
        registration_id: Option<String>,
    ) -> Result<BoundIntent> {
        typed.validate(now)?;
        let (address, intent_id) =
            signer.papertrade_intent_identity(&to_evm_intent(&typed), now)?;
        let bound = BoundIntent {
            action: typed.action().map(str::to_owned),
            cancel_target_action: None,
            typed,
            intent_id,
            signer: address,
            registration_id,
        };
        Ok(bound)
    }

    async fn validate_mutable_evidence(&self, typed: &PapertradeTypedIntent) -> Result<()> {
        match typed {
            PapertradeTypedIntent::OpenPosition {
                user,
                instrument_id,
                size,
                leverage,
                use_debt,
                ..
            } => {
                if *use_debt || !user.eq_ignore_ascii_case(&self.account.owner) {
                    return Err(GoFinError::validation(
                        "Papertrade opens must use this owner and cash-only mode.",
                    ));
                }
                let trading = self
                    .http
                    .read(ReadRequest::TradingState, self.evm.rpc.as_ref())
                    .await?;
                if trading.get("tradingPaused").and_then(Value::as_bool) != Some(false) {
                    return Err(GoFinError::new(
                        "PAPERTRADE_MARKET_CLOSED",
                        "revalidation",
                        "Papertrade trading is paused or its state is unavailable.",
                        "Wait for the official trading state to report open intake.",
                    ));
                }
                let min_margin = parse_uint(&exact_uint(&trading, "minimumTradeSizeRaw")?)?;
                let min_notional = parse_uint(&exact_uint(&trading, "minimumOpenNotionalRaw")?)?;
                let margin = BigUint::from(*size);
                if margin < min_margin || &margin * BigUint::from(*leverage) < min_notional {
                    return Err(GoFinError::validation(
                        "Papertrade margin or notional is below the current protocol minimum.",
                    ));
                }
                let instruments = trading
                    .get("instruments")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        invalid_response("Trading state omitted its instrument list.")
                    })?;
                let instrument = instruments
                    .iter()
                    .find(|row| {
                        row.as_array()
                            .and_then(|a| a.first())
                            .and_then(|v| v.as_u64())
                            == Some(u64::from(*instrument_id))
                    })
                    .ok_or_else(|| {
                        invalid_response("Selected Papertrade instrument is not present.")
                    })?;
                let fields = instrument
                    .as_array()
                    .ok_or_else(|| invalid_response("Papertrade instrument tuple is malformed."))?;
                if fields.len() != 23 {
                    return Err(invalid_response(
                        "Papertrade instrument tuple has an unreviewed shape.",
                    ));
                }
                let active = fields[4].as_bool() == Some(true);
                let paused = fields[5].as_bool() != Some(false);
                let terminal = fields[6].as_bool();
                let retired = fields[7].as_bool();
                let close_only = fields[8].as_bool();
                if terminal.is_none()
                    || retired.is_none()
                    || close_only.is_none()
                    || terminal != Some(paused)
                    || close_only != Some(paused)
                    || retired != Some(paused || !active)
                {
                    return Err(invalid_response(
                        "Papertrade instrument lifecycle flags are inconsistent or malformed.",
                    ));
                }
                let max_leverage = fields[14]
                    .as_u64()
                    .ok_or_else(|| invalid_response("Instrument leverage cap is malformed."))?;
                let max_notional = parse_uint(&exact_uint(&fields[15], "maxPositionNotional")?)?;
                if !active
                    || paused
                    || u64::from(*leverage) > max_leverage
                    || &margin * BigUint::from(*leverage) > max_notional
                {
                    return Err(GoFinError::new(
                        "PAPERTRADE_MARKET_LIMIT",
                        "revalidation",
                        "Selected Papertrade instrument is paused or outside its current leverage/notional bounds.",
                        "Refresh market limits and prepare a new bounded trade.",
                    ));
                }
                if action_disabled(&trading, "openPosition")? {
                    return Err(GoFinError::new(
                        "PAPERTRADE_ACTION_DISABLED",
                        "revalidation",
                        "Papertrade has disabled new position intake.",
                        "Wait for openPosition intake to be enabled.",
                    ));
                }
                let snapshot = self
                    .http
                    .read(
                        ReadRequest::AccountState {
                            owner: user.to_ascii_lowercase(),
                        },
                        self.evm.rpc.as_ref(),
                    )
                    .await?;
                let balance = parse_uint(&raw_decimal(snapshot.get("balance"), "balance")?)?;
                let queued = parse_uint(&raw_decimal(snapshot.get("queued"), "queued")?)?;
                let locked = parse_uint(&raw_decimal(snapshot.get("locked"), "locked")?)?;
                let reserved = queued + locked;
                let available = if balance >= reserved {
                    balance - reserved
                } else {
                    return Err(invalid_response(
                        "Papertrade available cash is internally inconsistent.",
                    ));
                };
                if available < margin {
                    return Err(GoFinError::new(
                        "PAPERTRADE_CASH_INSUFFICIENT",
                        "revalidation",
                        "Available internal USDC cash no longer covers the approved margin.",
                        "Deposit or reduce the trade margin before preparing a new operation.",
                    ));
                }
            }
            PapertradeTypedIntent::Stake { user, amount, .. } => {
                if !user.eq_ignore_ascii_case(&self.account.owner) {
                    return Err(GoFinError::validation(
                        "Papertrade stake owner does not match the protected account.",
                    ));
                }
                let state = self
                    .http
                    .read(
                        ReadRequest::StakingState {
                            owner: user.clone(),
                        },
                        self.evm.rpc.as_ref(),
                    )
                    .await?;
                let available = parse_uint(&raw_decimal(
                    state.pointer("/paper/balance"),
                    "paper.balance",
                )?)?;
                let trading = self
                    .http
                    .read(ReadRequest::TradingState, self.evm.rpc.as_ref())
                    .await?;
                let intake = trading
                    .get("intakeControls")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        invalid_response("Trading intake state omitted minimum PAPER stake.")
                    })?;
                let minimum = intake.get(1).ok_or_else(|| {
                    invalid_response("Trading intake state omitted minimum PAPER stake.")
                })?;
                let amount = parse_uint(amount)?;
                if action_disabled(&trading, "paperStake")? {
                    return Err(GoFinError::new(
                        "PAPERTRADE_ACTION_DISABLED",
                        "revalidation",
                        "Papertrade has disabled staking intake.",
                        "Wait for paperStake intake to be enabled.",
                    ));
                }
                if amount < parse_uint(&exact_uint(minimum, "minimumPaperStakeRaw")?)? {
                    return Err(GoFinError::validation(
                        "PAPER stake is below the current protocol minimum.",
                    ));
                }
                if available < amount {
                    return Err(GoFinError::validation(
                        "PAPER wallet balance no longer covers the stake.",
                    ));
                }
            }
            PapertradeTypedIntent::Unstake { user, amount, .. } => {
                if !user.eq_ignore_ascii_case(&self.account.owner) {
                    return Err(GoFinError::validation(
                        "Papertrade unstake owner does not match the protected account.",
                    ));
                }
                let trading = self
                    .http
                    .read(ReadRequest::TradingState, self.evm.rpc.as_ref())
                    .await?;
                if action_disabled(&trading, "paperUnstake")? {
                    return Err(GoFinError::new(
                        "PAPERTRADE_ACTION_DISABLED",
                        "revalidation",
                        "Papertrade has disabled unstaking intake.",
                        "Wait for paperUnstake intake to be enabled.",
                    ));
                }
                let state = self
                    .http
                    .read(
                        ReadRequest::StakingState {
                            owner: user.clone(),
                        },
                        self.evm.rpc.as_ref(),
                    )
                    .await?;
                let staked = parse_uint(&raw_decimal(
                    state.pointer("/paper/staked"),
                    "paper.staked",
                )?)?;
                if staked < parse_uint(amount)? {
                    return Err(GoFinError::validation(
                        "Staked PAPER no longer covers the unstake.",
                    ));
                }
            }
            PapertradeTypedIntent::Claim { user, .. } => {
                if !user.eq_ignore_ascii_case(&self.account.owner) {
                    return Err(GoFinError::validation(
                        "Papertrade claim owner does not match the protected account.",
                    ));
                }
                let trading = self
                    .http
                    .read(ReadRequest::TradingState, self.evm.rpc.as_ref())
                    .await?;
                if action_disabled(&trading, "paperClaim")? {
                    return Err(GoFinError::new(
                        "PAPERTRADE_ACTION_DISABLED",
                        "revalidation",
                        "Papertrade has disabled reward claims.",
                        "Wait for paperClaim intake to be enabled.",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
impl PapertradeRuntime for BrokerPapertradeRuntime {
    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<()> {
        self.funding_host.attach_execution_state(state.clone())?;
        *self
            .execution_state
            .lock()
            .map_err(|_| unavailable("Papertrade execution state lock is poisoned."))? =
            Some(state);
        Ok(())
    }

    async fn read(&self, request: ReadRequest) -> Result<Value> {
        self.http.read(request, self.evm.rpc.as_ref()).await
    }

    async fn valuation(&self, request: &IntentRequest) -> Result<Option<gofin_domain::Valuation>> {
        use gofin_papertrade::{IntentRequest as I, paper_asset, trading_usdc_asset};
        if matches!(request, I::Cancel { .. }) {
            let now = chrono::Utc::now().to_rfc3339();
            return Ok(Some(gofin_domain::Valuation {
                movement_usd_micros: gofin_domain::Quantity::from_u64(0),
                maximum_fee_usd_micros: gofin_domain::Quantity::from_u64(0),
                additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
                observed_at: now,
                source: "papertrade:relayed-cancellation-no-owner-fee:v1".into(),
                prices: vec![],
            }));
        }
        if matches!(request, I::Stake { .. } | I::Unstake { .. }) {
            let asset = paper_asset();
            let price = gofin_domain::AssetPrice {
                source: gofin_domain::unpriced::papertrade_principal_source(&asset)
                    .ok_or_else(|| {
                        GoFinError::validation("PAPER principal identity is not reviewed.")
                    })?
                    .to_owned(),
                asset: asset.clone(),
                unit_atoms: gofin_domain::Quantity::new("1000000000000000000")?,
                usd_micros_per_unit: gofin_domain::Quantity::from_u64(0),
                observed_at: chrono::Utc::now().to_rfc3339(),
            };
            return Ok(Some(gofin_domain::Valuation {
                movement_usd_micros: gofin_domain::Quantity::from_u64(0),
                maximum_fee_usd_micros: gofin_domain::Quantity::from_u64(0),
                additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
                observed_at: price.observed_at.clone(),
                source: price.source.clone(),
                prices: vec![price],
            }));
        }
        let amount_wad = match request {
            I::Open { margin_wad, .. } => *margin_wad,
            I::Close {
                owner,
                position_ids,
            } => {
                let snapshot = self
                    .http
                    .read(
                        gofin_papertrade::ReadRequest::Positions {
                            owner: owner.to_ascii_lowercase(),
                        },
                        self.evm.rpc.as_ref(),
                    )
                    .await?;
                close_margin_wad(&snapshot, owner, position_ids)?
            }
            I::Stake { .. } | I::Unstake { .. } => unreachable!(),
            I::Claim { .. } => {
                // Reward credits are estimates only; the canonical contract
                // return/event determines the settled amount.
                0
            }
            _ => return Ok(None),
        };
        let asset = underlying_hyperevm_usdc();
        let mut price = self.valuator.asset_price(&asset).await?;
        price.asset = trading_usdc_asset();
        price.unit_atoms = gofin_domain::Quantity::new("1000000000000000000")?;
        let movement = price.usd_micros_per_unit.mul_div_ceil(
            &gofin_domain::Quantity::new(amount_wad.to_string())?,
            &price.unit_atoms,
        )?;
        Ok(Some(gofin_domain::Valuation {
            movement_usd_micros: movement,
            maximum_fee_usd_micros: gofin_domain::Quantity::from_u64(0),
            additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
            observed_at: price.observed_at.clone(),
            source: price.source.clone(),
            prices: vec![price],
        }))
    }

    async fn session_revocation_valuation(
        &self,
        revocation: &gofin_papertrade::BoundSessionRevoke,
    ) -> Result<Option<gofin_domain::Valuation>> {
        revocation.validate()?;
        let asset = gofin_domain::AssetId {
            network: "ethereum:999".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let price = self.valuator.asset_price(&asset).await?;
        if price.asset != asset || price.unit_atoms.is_zero() || price.usd_micros_per_unit.is_zero()
        {
            return Err(GoFinError::validation(
                "The configured price source did not return an exact HyperEVM native-asset price.",
            ));
        }
        let maximum_cost = revocation.transaction.maximum_cost()?.to_string();
        let maximum_fee = gofin_domain::Quantity::new(maximum_cost)?
            .mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)?;
        Ok(Some(gofin_domain::Valuation {
            movement_usd_micros: gofin_domain::Quantity::from_u64(0),
            maximum_fee_usd_micros: maximum_fee,
            additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
            observed_at: price.observed_at.clone(),
            source: price.source.clone(),
            prices: vec![price],
        }))
    }

    async fn prepare_intent(
        &self,
        request: IntentRequest,
        operation_id: &str,
    ) -> Result<BoundIntent> {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        request.validate(now)?;
        if let Some(existing) = self.load_bound_intent(operation_id)? {
            if existing.matches_request(&request) {
                existing.typed.validate(now)?;
                return Ok(existing);
            }
            return Err(GoFinError::validation(
                "Papertrade operation ID is already bound to a different immutable request.",
            ));
        }
        if !request.owner().eq_ignore_ascii_case(&self.account.owner) {
            return Err(GoFinError::validation(
                "Papertrade request owner differs from enrolled account.",
            ));
        }
        let cancel_target_action = if let IntentRequest::Cancel { owner, intent_id } = &request {
            let snapshot = self
                .http
                .read(
                    ReadRequest::AccountState {
                        owner: owner.to_ascii_lowercase(),
                    },
                    self.evm.rpc.as_ref(),
                )
                .await?;
            Some(lookup_cancel_target_action(&snapshot, intent_id)?)
        } else {
            None
        };
        let (typed, signer, registration_id) = match &request {
            IntentRequest::Open {
                owner,
                instrument_id,
                is_long,
                margin_wad,
                leverage,
            } => {
                let nonce = self
                    .allocate_nonce(owner, gofin_papertrade::PapertradeDomainTarget::Exchange)
                    .await?;
                let (registration_id, signer, _, expiry) =
                    self.active_session_signer(owner, now).await?;
                let deadline = session_intent_deadline(now, expiry)?;
                (
                    PapertradeTypedIntent::OpenPosition {
                        user: owner.clone(),
                        instrument_id: *instrument_id,
                        is_long: *is_long,
                        size: *margin_wad,
                        leverage: *leverage,
                        use_debt: false,
                        nonce,
                        deadline,
                    },
                    signer,
                    Some(registration_id),
                )
            }
            IntentRequest::Close {
                owner,
                position_ids,
            } => {
                let nonce = self
                    .allocate_nonce(owner, gofin_papertrade::PapertradeDomainTarget::Exchange)
                    .await?;
                let (registration_id, signer, _, expiry) =
                    self.active_session_signer(owner, now).await?;
                let deadline = session_intent_deadline(now, expiry)?;
                (
                    PapertradeTypedIntent::Close {
                        user: owner.clone(),
                        position_ids: position_ids.clone(),
                        nonce,
                        deadline,
                    },
                    signer,
                    Some(registration_id),
                )
            }
            IntentRequest::Cancel { owner, intent_id } => {
                let (registration_id, signer, _, expiry) =
                    self.active_session_signer(owner, now).await?;
                let deadline = session_intent_deadline(now, expiry)?;
                (
                    PapertradeTypedIntent::CancelIntent {
                        user: owner.clone(),
                        intent_id: intent_id.clone(),
                        deadline: deadline.to_string(),
                    },
                    signer,
                    Some(registration_id),
                )
            }
            IntentRequest::Stake { owner, amount } => {
                let nonce = self
                    .allocate_nonce(
                        owner,
                        gofin_papertrade::PapertradeDomainTarget::PaperStaking,
                    )
                    .await?;
                let signer = self.owner_signer(owner)?;
                (
                    PapertradeTypedIntent::Stake {
                        user: owner.clone(),
                        amount: amount.to_string(),
                        nonce,
                        deadline: signed_intent_deadline(now)?,
                    },
                    signer,
                    None,
                )
            }
            IntentRequest::Unstake { owner, amount } => {
                let nonce = self
                    .allocate_nonce(
                        owner,
                        gofin_papertrade::PapertradeDomainTarget::PaperStaking,
                    )
                    .await?;
                let signer = self.owner_signer(owner)?;
                (
                    PapertradeTypedIntent::Unstake {
                        user: owner.clone(),
                        amount: amount.to_string(),
                        nonce,
                        deadline: signed_intent_deadline(now)?,
                    },
                    signer,
                    None,
                )
            }
            IntentRequest::Claim { owner } => {
                let nonce = self
                    .allocate_nonce(
                        owner,
                        gofin_papertrade::PapertradeDomainTarget::PaperStaking,
                    )
                    .await?;
                let signer = self.owner_signer(owner)?;
                (
                    PapertradeTypedIntent::Claim {
                        user: owner.clone(),
                        nonce,
                        deadline: signed_intent_deadline(now)?,
                    },
                    signer,
                    None,
                )
            }
            IntentRequest::RegisterSession { .. } | IntentRequest::RevokeSession { .. } => {
                return Err(GoFinError::validation(
                    "Session operations require the configuration lifecycle.",
                ));
            }
        };
        let mut bound = self.make_bound(typed, &signer, now, registration_id)?;
        bound.cancel_target_action = cancel_target_action;
        self.persist_bound_intent(operation_id, &bound)?;
        Ok(bound)
    }

    async fn revalidate_intent(
        &self,
        request: &IntentRequest,
        bound: Option<&BoundIntent>,
    ) -> Result<()> {
        request.validate(chrono::Utc::now().timestamp().max(0) as u64)?;
        if !request.owner().eq_ignore_ascii_case(&self.account.owner) {
            return Err(GoFinError::validation(
                "Papertrade owner changed after preparation.",
            ));
        }
        if let Some(bound) = bound {
            let now = chrono::Utc::now().timestamp().max(0) as u64;
            bound.typed.validate(now)?;
            let signer = self.signer_for_bound(bound, now).await?;
            let (address, id) =
                signer.papertrade_intent_identity(&to_evm_intent(&bound.typed), now)?;
            if !address.eq_ignore_ascii_case(&bound.signer)
                || !id.eq_ignore_ascii_case(&bound.intent_id)
            {
                return Err(GoFinError::validation(
                    "Papertrade intent signer identity changed.",
                ));
            }
            if !typed_owner(&bound.typed).eq_ignore_ascii_case(request.owner()) {
                return Err(GoFinError::validation(
                    "Papertrade frozen intent owner changed.",
                ));
            }
            if let Some(registration_id) = &bound.registration_id {
                let (_, _, _, expiry) = self
                    .session_signer(request.owner(), registration_id, now)
                    .await?;
                if expiry < intent_deadline(&bound.typed) {
                    return Err(GoFinError::validation(
                        "Papertrade session is expired or cannot cover the frozen intent deadline.",
                    ));
                }
            }
            self.validate_mutable_evidence(&bound.typed).await?;
        }
        Ok(())
    }

    async fn prepare_session_registration(
        &self,
        owner: &str,
        registration_id: &str,
        expiry: u64,
        operation_id: &str,
    ) -> Result<BoundIntent> {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        if !owner.eq_ignore_ascii_case(&self.account.owner) || registration_id != operation_id {
            return Err(GoFinError::validation(
                "Session registration identity must be stable and owner-bound.",
            ));
        }
        if let Some(record) = self
            .credentials
            .papertrade_runtime_record(&format!("papertrade-registration-{registration_id}"))?
        {
            let intent: BoundIntent =
                serde_json::from_value(record.get("bound_intent").cloned().ok_or_else(|| {
                    invalid_response("Protected session registration intent is missing.")
                })?)
                .map_err(|_| {
                    invalid_response("Protected session registration intent is malformed.")
                })?;
            if !matches!(&intent.typed, PapertradeTypedIntent::RegisterSessionKey { user, expiry: stored, .. } if user.eq_ignore_ascii_case(owner) && *stored == expiry)
            {
                return Err(GoFinError::validation(
                    "Registration ID is already bound to different owner or expiry.",
                ));
            }
            return Ok(intent);
        }
        let (credential, key, _) =
            self.credentials
                .papertrade_session_signer(registration_id, owner, expiry)?;
        let nonce = self
            .allocate_nonce(owner, gofin_papertrade::PapertradeDomainTarget::Exchange)
            .await?;
        let deadline = session_registration_deadline(now, expiry)?;
        let typed = PapertradeTypedIntent::RegisterSessionKey {
            user: owner.to_owned(),
            key,
            expiry,
            fee_address: "0x0000000000000000000000000000000000000000".into(),
            nonce,
            deadline,
        };
        let signer = self.owner_signer(owner)?;
        let bound = self.make_bound(typed, &signer, now, Some(registration_id.to_owned()))?;
        self.persist_bound_intent(operation_id, &bound)?;
        self.credentials.put_papertrade_runtime_record(&format!("papertrade-registration-{registration_id}"), &json!({"version":1,"owner":owner,"expiry":expiry,"credential_ref":credential.as_str(),"bound_intent":bound}))?;
        Ok(bound)
    }

    async fn prepare_session_revocation(
        &self,
        owner: &str,
        registration_id: &str,
        operation_id: &str,
    ) -> Result<gofin_papertrade::BoundSessionRevoke> {
        use gofin_evm::alloy::primitives::{Bytes, U256};
        if !owner.eq_ignore_ascii_case(&self.account.owner) || registration_id != operation_id {
            return Err(GoFinError::validation(
                "Session revocation identity must be stable and owner-bound.",
            ));
        }
        if let Some(value) = self
            .credentials
            .papertrade_runtime_record(&format!("papertrade-revocation-{registration_id}"))?
        {
            let bound: gofin_papertrade::BoundSessionRevoke = serde_json::from_value(value)
                .map_err(|_| invalid_response("Protected revocation transaction is malformed."))?;
            bound.validate()?;
            if bound.owner.eq_ignore_ascii_case(owner) && bound.registration_id == registration_id {
                return Ok(bound);
            }
            return Err(GoFinError::validation(
                "Revocation ID is already bound to another session.",
            ));
        }
        let record = self
            .credentials
            .papertrade_runtime_record(&format!("papertrade-registration-{registration_id}"))?
            .ok_or_else(|| unavailable("Protected session registration is unavailable."))?;
        let intent: BoundIntent =
            serde_json::from_value(record.get("bound_intent").cloned().ok_or_else(|| {
                invalid_response("Protected session registration intent is missing.")
            })?)
            .map_err(|_| invalid_response("Protected session registration intent is malformed."))?;
        let PapertradeTypedIntent::RegisterSessionKey {
            user, key, expiry, ..
        } = intent.typed
        else {
            return Err(invalid_response(
                "Protected registration record is not a session-key intent.",
            ));
        };
        if !user.eq_ignore_ascii_case(owner)
            || expiry <= chrono::Utc::now().timestamp().max(0) as u64
        {
            return Err(GoFinError::validation(
                "Session registration is expired or belongs to another owner.",
            ));
        }
        let mut calldata = hex::decode("84f4fc6a").expect("fixed selector");
        calldata.extend_from_slice(&[0; 12]);
        calldata.extend_from_slice(
            &hex::decode(key.strip_prefix("0x").unwrap_or_default())
                .map_err(|_| invalid_response("Session key address is malformed."))?,
        );
        let prepared = self
            .evm
            .prepare(
                gofin_evm::address(owner)?,
                gofin_evm::address(gofin_papertrade::SESSION_KEY_MANAGER)?,
                U256::ZERO,
                Bytes::from(calldata),
                U256::from(10_000_000_000_000_000u64),
            )
            .await?;
        let bound = gofin_papertrade::BoundSessionRevoke {
            owner: owner.to_ascii_lowercase(),
            session_key: key.to_ascii_lowercase(),
            registration_id: registration_id.to_owned(),
            submission_id: format!("papertrade-session-revoke:{operation_id}"),
            transaction: prepared.transaction,
        };
        bound.validate()?;
        self.credentials.put_papertrade_runtime_record(
            &format!("papertrade-revocation-{registration_id}"),
            &serde_json::to_value(&bound)
                .map_err(|_| unavailable("Revocation transaction could not be protected."))?,
        )?;
        Ok(bound)
    }
    async fn revalidate_session_configuration(
        &self,
        binding: &gofin_papertrade::SessionConfigurationBinding,
    ) -> Result<()> {
        match binding {
            gofin_papertrade::SessionConfigurationBinding::Register { registration } => {
                let intent = self
                    .session_registration_intent(registration)
                    .await?
                    .ok_or_else(|| {
                        unavailable("Protected session registration binding is unavailable.")
                    })?;
                let now = chrono::Utc::now().timestamp().max(0) as u64;
                intent.typed.validate(now)?;
                if registration.expiry < registration.deadline || registration.expiry <= now {
                    return Err(GoFinError::validation(
                        "Session registration expiry does not cover its signed deadline.",
                    ));
                }
                let signer = self.owner_signer(&registration.owner)?;
                let (address, id) =
                    signer.papertrade_intent_identity(&to_evm_intent(&intent.typed), now)?;
                if !address.eq_ignore_ascii_case(&registration.signer)
                    || !id.eq_ignore_ascii_case(&registration.intent_id)
                {
                    return Err(GoFinError::validation(
                        "Session registration identity changed after preparation.",
                    ));
                }
                let PapertradeTypedIntent::RegisterSessionKey { key, .. } = &intent.typed else {
                    return Err(invalid_response(
                        "Stored registration intent has the wrong primary type.",
                    ));
                };
                let (registered_expiry, fee_address) =
                    self.read_session_key(&registration.owner, key).await?;
                if registered_expiry != 0
                    || fee_address != "0x0000000000000000000000000000000000000000"
                {
                    return Err(GoFinError::new(
                        "PAPERTRADE_SESSION_KEY_COLLISION",
                        "revalidation",
                        "Generated Papertrade session key is already registered on chain.",
                        "Reconcile the original registration before creating a different session.",
                    ));
                }
                let nonce: u64 = registration
                    .nonce
                    .parse()
                    .map_err(|_| invalid_response("Session registration nonce is malformed."))?;
                if self
                    .nonce_used_at(
                        gofin_papertrade::SESSION_KEY_MANAGER,
                        &registration.owner,
                        nonce,
                    )
                    .await?
                {
                    return Err(GoFinError::validation(
                        "Session registration nonce has already been consumed.",
                    ));
                }
                Ok(())
            }
            gofin_papertrade::SessionConfigurationBinding::Revoke { revocation } => {
                let transaction = self
                    .session_revocation_transaction(revocation)
                    .await?
                    .ok_or_else(|| {
                        unavailable("Protected session revocation transaction is unavailable.")
                    })?;
                transaction.validate()?;
                if transaction.owner != revocation.owner
                    || transaction.registration_id != revocation.registration_id
                    || transaction.submission_id != revocation.submission_id
                {
                    return Err(GoFinError::validation(
                        "Session revocation identity changed after preparation.",
                    ));
                }
                Ok(())
            }
        }
    }

    async fn dispatch(&self, intent: &BoundIntent) -> Result<Receipt> {
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        intent.typed.validate(now)?;
        self.validate_mutable_evidence(&intent.typed).await?;
        let signer = self.signer_for_bound(intent, now).await?;
        match &intent.typed {
            PapertradeTypedIntent::CancelIntent { intent_id, .. } => {
                let target_action = intent.cancel_target_action.as_deref().ok_or_else(|| {
                    GoFinError::validation("Papertrade cancel binding omitted the target action.")
                })?;
                let receipt = self
                    .http
                    .submit_cancel(intent, &signer, target_action)
                    .await?;
                let proof = cancel_proof_from_receipt(
                    &self.account.owner,
                    intent_id,
                    target_action,
                    &receipt.status,
                )?;
                self.credentials.put_papertrade_runtime_record(
                    &cancel_record_key(&self.account.owner, intent_id),
                    &serde_json::to_value(proof)
                        .map_err(|_| unavailable("Cancel evidence could not be persisted."))?,
                )?;
                Ok(receipt)
            }
            _ => self.http.submit_typed(intent, &signer).await,
        }
    }
    async fn dispatch_session_registration(
        &self,
        binding: &gofin_papertrade::SessionRegistrationBinding,
    ) -> Result<Receipt> {
        let intent = self
            .session_registration_intent(binding)
            .await?
            .ok_or_else(|| unavailable("Protected session registration intent is unavailable."))?;
        if !binding.intent_id.eq_ignore_ascii_case(&intent.intent_id) {
            return Err(GoFinError::validation(
                "Session registration intent differs from the approved binding.",
            ));
        }
        let start_key = registration_dispatch_block_key(&binding.registration_id);
        if self
            .credentials
            .papertrade_runtime_record(&start_key)?
            .is_some()
        {
            return Err(GoFinError::new(
                "PAPERTRADE_REGISTRATION_ALREADY_DISPATCHED",
                "dispatch",
                "This session registration already has a durable dispatch attempt.",
                "Reconcile the original intent identity; do not submit it again.",
            ));
        }
        // Resolve protected owner credentials before recording the no-replay
        // dispatch marker. Credential lookup failures are local preflight
        // failures, not evidence that a relayer request could have escaped.
        let signer = self.owner_signer(&binding.owner)?;
        let head = self.evm.rpc.request("eth_blockNumber", json!([])).await?;
        let from_block = rpc_hex_u64(&head)?;
        self.credentials.put_papertrade_runtime_record(
            &start_key,
            &json!({"version":1,"registration_id":binding.registration_id,"intent_id":binding.intent_id,"from_block":from_block}),
        )?;
        self.http.submit_typed(&intent, signer.as_ref()).await
    }
    async fn session_registration_intent(
        &self,
        binding: &gofin_papertrade::SessionRegistrationBinding,
    ) -> Result<Option<BoundIntent>> {
        let Some(record) = self.credentials.papertrade_runtime_record(&format!(
            "papertrade-registration-{}",
            binding.registration_id
        ))?
        else {
            return Ok(None);
        };
        let intent: BoundIntent =
            serde_json::from_value(record.get("bound_intent").cloned().ok_or_else(|| {
                invalid_response("Protected session registration intent is missing.")
            })?)
            .map_err(|_| invalid_response("Protected session registration intent is malformed."))?;
        let expected = gofin_papertrade::SessionRegistrationBinding::from_bound(
            binding.registration_id.clone(),
            &intent,
        )?;
        if expected != *binding {
            return Err(GoFinError::validation(
                "Session registration differs from protected generated key identity.",
            ));
        }
        Ok(Some(intent))
    }
    async fn session_registration_from_block(
        &self,
        binding: &gofin_papertrade::SessionRegistrationBinding,
    ) -> Result<Option<u64>> {
        let Some(record) =
            self.credentials
                .papertrade_runtime_record(&registration_dispatch_block_key(
                    &binding.registration_id,
                ))?
        else {
            return Ok(None);
        };
        if record["version"].as_u64() != Some(1)
            || record["registration_id"].as_str() != Some(binding.registration_id.as_str())
            || record["intent_id"]
                .as_str()
                .is_none_or(|intent_id| !intent_id.eq_ignore_ascii_case(&binding.intent_id))
        {
            return Err(invalid_response(
                "Protected registration dispatch search anchor is malformed.",
            ));
        }
        record["from_block"]
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid_response("Registration dispatch block is malformed."))
    }
    async fn session_registration_expiry_proof(
        &self,
        binding: &gofin_papertrade::SessionRegistrationBinding,
    ) -> Result<Option<gofin_papertrade::SessionRegistrationExpiryProof>> {
        BrokerPapertradeRuntime::session_registration_expiry_proof(self, binding).await
    }
    async fn core_withdrawal_expiry_proof(
        &self,
        bound: &BoundIntent,
    ) -> Result<Option<gofin_papertrade::CoreWithdrawalExpiryProof>> {
        BrokerPapertradeRuntime::core_withdrawal_expiry_proof(self, bound).await
    }
    async fn intent_expiry_proof(
        &self,
        bound: &BoundIntent,
    ) -> Result<Option<gofin_papertrade::PapertradeIntentExpiryProof>> {
        BrokerPapertradeRuntime::intent_expiry_proof(self, bound).await
    }
    async fn activate_session_registration(
        &self,
        binding: &gofin_papertrade::SessionRegistrationBinding,
        _proof: &gofin_papertrade::VerifiedPapertradeCall,
    ) -> Result<()> {
        self.credentials.activate_papertrade_session(
            &binding.registration_id,
            &binding.owner,
            binding.expiry,
            chrono::Utc::now().timestamp().max(0) as u64,
        )
    }
    async fn dispatch_session_revocation(
        &self,
        binding: &gofin_papertrade::SessionRevocationBinding,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Receipt> {
        let bound = self
            .session_revocation_transaction(binding)
            .await?
            .ok_or_else(|| {
                unavailable("Protected session revocation transaction is unavailable.")
            })?;
        bound.validate()?;
        if bound.submission_id != binding.submission_id {
            return Err(GoFinError::validation(
                "Revocation submission identity mismatch.",
            ));
        }
        journal.assert_authorized()?;
        // EvmRuntime::revalidate checks chain, nonce, fee, balance and simulation,
        // and returns only the signed envelope bytes for immediate dispatch.
        let raw = self.evm.revalidate(&bound.transaction).await?;
        let hash = gofin_evm::verify_signed_envelope(&bound.transaction, &raw)?;
        journal.assert_authorized()?;
        let returned = self
            .evm
            .rpc
            .request(
                "eth_sendRawTransaction",
                json!([format!("0x{}", hex::encode(raw))]),
            )
            .await?;
        let tx_hash = returned
            .as_str()
            .ok_or_else(|| invalid_response("Revocation RPC response omitted transaction hash."))?;
        if !tx_hash.eq_ignore_ascii_case(&format!("{hash:#x}")) {
            return Err(invalid_response(
                "Revocation RPC returned a different signed transaction hash.",
            ));
        }
        Ok(Receipt {
            intent_id: bound.submission_id,
            action: "revokeSessionKey".into(),
            status: "accepted".into(),
            transaction_hash: Some(tx_hash.to_ascii_lowercase()),
        })
    }
    async fn session_revocation_transaction(
        &self,
        binding: &gofin_papertrade::SessionRevocationBinding,
    ) -> Result<Option<gofin_papertrade::BoundSessionRevoke>> {
        let Some(value) = self.credentials.papertrade_runtime_record(&format!(
            "papertrade-revocation-{}",
            binding.registration_id
        ))?
        else {
            return Ok(None);
        };
        let transaction: gofin_papertrade::BoundSessionRevoke = serde_json::from_value(value)
            .map_err(|_| invalid_response("Protected revocation transaction is malformed."))?;
        transaction.validate()?;
        if transaction.owner != binding.owner
            || transaction.registration_id != binding.registration_id
            || transaction.submission_id != binding.submission_id
        {
            return Err(GoFinError::validation(
                "Protected revocation transaction identity differs from its public binding.",
            ));
        }
        Ok(Some(transaction))
    }
    async fn deactivate_session_registration(
        &self,
        binding: &gofin_papertrade::SessionRevocationBinding,
        _proof: &gofin_papertrade::VerifiedSessionRevocation,
    ) -> Result<()> {
        self.credentials.deactivate_papertrade_session(
            &binding.registration_id,
            &binding.owner,
            chrono::Utc::now().timestamp().max(0) as u64,
        )
    }
    async fn canonical_transaction(
        &self,
        hash: &str,
    ) -> Result<Option<gofin_papertrade::CanonicalTransaction>> {
        self.observer.by_hash(hash).await
    }
    async fn canonical_intent_transaction(
        &self,
        intent_id: &str,
        from_block: u64,
    ) -> Result<Option<gofin_papertrade::CanonicalTransaction>> {
        self.observer.by_intent(intent_id, from_block).await
    }
    async fn cancel_status(
        &self,
        owner: &str,
        target_intent_id: &str,
        _from_block: u64,
    ) -> Result<Option<gofin_papertrade::CancelStatusProof>> {
        if !owner.eq_ignore_ascii_case(&self.account.owner) {
            return Err(GoFinError::validation(
                "Cancel status owner differs from the enrolled Papertrade account.",
            ));
        }
        let Some(value) = self
            .credentials
            .papertrade_runtime_record(&cancel_record_key(owner, target_intent_id))?
        else {
            // A fresh full snapshot deliberately omits historical outcome patches.
            // Missing evidence after restart is not terminal cancellation proof.
            return Ok(None);
        };
        let proof: gofin_papertrade::CancelStatusProof = serde_json::from_value(value)
            .map_err(|_| invalid_response("Protected Papertrade cancel proof is malformed."))?;
        if !proof.owner.eq_ignore_ascii_case(owner)
            || !proof
                .target_intent_id
                .eq_ignore_ascii_case(target_intent_id)
        {
            return Err(GoFinError::validation(
                "Protected cancel proof identity mismatch.",
            ));
        }
        Ok(Some(proof))
    }
    async fn liquidation_transaction(
        &self,
        owner: &str,
        position_id: &str,
        since_block: Option<u64>,
    ) -> Result<Option<gofin_papertrade::CanonicalTransaction>> {
        if !owner.eq_ignore_ascii_case(&self.account.owner) {
            return Err(GoFinError::validation(
                "Liquidation owner differs from the enrolled Papertrade account.",
            ));
        }
        self.observer
            .by_liquidation(owner, position_id, since_block)
            .await
    }
    async fn funding_valuation(
        &self,
        intent: &FundingIntent,
    ) -> Result<Option<gofin_domain::Valuation>> {
        use gofin_papertrade::{FundingDirection, FundingRoute};
        let Some(value) = self
            .credentials
            .papertrade_runtime_record(&funding_record_key(&intent.request_id))?
        else {
            return Ok(None);
        };
        let plan: crate::papertrade_funding::BoundFunding = serde_json::from_value(value)
            .map_err(|_| invalid_response("Protected funding valuation plan is malformed."))?;
        if plan.intent != *intent {
            return Err(invalid_response(
                "Funding valuation request differs from the protected route plan.",
            ));
        }
        if intent.direction == FundingDirection::Withdraw {
            // Stage one moves only the requested Papertrade cash to the
            // owner's HyperCore spot account. Any onward route fee belongs to
            // the separately authorized continuation operation.
            let underlying = self
                .valuator
                .asset_price(&underlying_hyperevm_usdc())
                .await?;
            let internal = gofin_papertrade::trading_usdc_asset();
            let price = gofin_domain::AssetPrice {
                asset: internal,
                unit_atoms: gofin_domain::Quantity::new("1000000000000000000")?,
                usd_micros_per_unit: underlying.usd_micros_per_unit,
                observed_at: underlying.observed_at,
                source: format!("{}; internal USDC denomination mapping", underlying.source),
            };
            let movement = price.usd_micros_per_unit.mul_div_ceil(
                &gofin_domain::Quantity::new(format!(
                    "{}000000000000",
                    plan.bounds.source_usdc_atoms
                ))?,
                &price.unit_atoms,
            )?;
            return Ok(Some(gofin_domain::Valuation {
                movement_usd_micros: movement,
                maximum_fee_usd_micros: gofin_domain::Quantity::from_u64(0),
                additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
                observed_at: price.observed_at.clone(),
                source: price.source.clone(),
                prices: vec![price],
            }));
        }
        if plan.bounds.maximum_source_fee_usdc_atoms > 0
            && !(intent.route == FundingRoute::Core
                && plan.bounds.source_usdc_atoms == intent.amount_usdc_atoms
                && plan.bounds.destination_min_usdc_atoms == intent.amount_usdc_atoms
                && plan.bounds.maximum_source_fee_usdc_atoms <= 1_000_000
                && plan.bounds.maximum_cctp_fee_usdc_atoms == 0
                && plan.bounds.maximum_activation_fee_atoms == 0)
        {
            return Err(invalid_response(
                "Additional source fee is not valid for this exact Papertrade route.",
            ));
        }
        let (route_asset, token, decimals) = match intent.route {
            // Hyperliquid's reviewed USDC asset uses 36-decimal accounting
            // units even though the funding wire amount is six decimals.
            FundingRoute::Core => ("hyperliquid:mainnet", "USDC", 36),
            FundingRoute::Hyperevm => (
                "ethereum:999",
                "0xb88339cb7199b77e23db6e890353e22632ba630f",
                6,
            ),
            FundingRoute::Ethereum => (
                "ethereum:1",
                "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                6,
            ),
            FundingRoute::Arbitrum => (
                "ethereum:42161",
                "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
                6,
            ),
            FundingRoute::Solana => (
                "solana:mainnet-beta",
                "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                6,
            ),
        };
        let route_usdc = gofin_domain::AssetId {
            network: route_asset.into(),
            reference: token.into(),
            decimals,
        };
        let route_usdc_price = if matches!(intent.route, FundingRoute::Core) {
            // HyperCore's reviewed USDC ledger uses 36-decimal raw atoms. The
            // USD quote comes from the configured native-USDC market source;
            // this is an explicit denomination mapping, never a $1 peg.
            let underlying = self
                .valuator
                .asset_price(&underlying_hyperevm_usdc())
                .await?;
            gofin_domain::AssetPrice {
                asset: route_usdc.clone(),
                unit_atoms: gofin_domain::Quantity::new("1000000000000000000000000000000000000")?,
                usd_micros_per_unit: underlying.usd_micros_per_unit,
                observed_at: underlying.observed_at,
                source: format!(
                    "{}; HyperCore USDC 36dp denomination mapping",
                    underlying.source
                ),
            }
        } else {
            self.valuator.asset_price(&route_usdc).await?
        };
        let (movement_price, movement_atoms, embedded_fee_atoms, embedded_fee_scale) =
            match intent.direction {
                FundingDirection::Deposit => (
                    route_usdc_price.clone(),
                    if matches!(intent.route, FundingRoute::Core) {
                        format!(
                            "{}000000000000000000000000000000",
                            plan.bounds.destination_min_usdc_atoms
                        )
                    } else {
                        plan.bounds.destination_min_usdc_atoms.to_string()
                    },
                    if matches!(intent.route, FundingRoute::Core) {
                        // Core's documented transfer fee is an additional source
                        // debit. The requested amount is still the exact proxy
                        // credit, so show the fee separately from that movement.
                        plan.bounds.maximum_source_fee_usdc_atoms
                    } else {
                        plan.bounds
                            .source_usdc_atoms
                            .checked_sub(plan.bounds.destination_min_usdc_atoms)
                            .ok_or_else(|| {
                                invalid_response("Funding destination bound exceeds source amount.")
                            })?
                    },
                    if matches!(intent.route, FundingRoute::Core) {
                        30
                    } else {
                        0
                    },
                ),
                FundingDirection::Withdraw => unreachable!("stage-one valuation returned above"),
            };
        let movement = movement_price.usd_micros_per_unit.mul_div_ceil(
            &gofin_domain::Quantity::new(movement_atoms)?,
            &movement_price.unit_atoms,
        )?;
        let mut prices = vec![movement_price];
        let mut fee_usd = gofin_domain::Quantity::from_u64(0);
        if embedded_fee_atoms > 0 {
            let price = &prices[0];
            let fee_atoms = format!("{embedded_fee_atoms}{}", "0".repeat(embedded_fee_scale));
            fee_usd = price
                .usd_micros_per_unit
                .mul_div_ceil(&gofin_domain::Quantity::new(fee_atoms)?, &price.unit_atoms)?;
        }
        if let (Some(asset_name), Some(cap)) = (
            &plan.bounds.native_fee_asset,
            &plan.bounds.maximum_native_fee_atoms,
        ) {
            let (expected, fee_network, fee_reference, fee_decimals) = match intent.route {
                FundingRoute::Core => {
                    ("hyperliquid:mainnet:HYPE", "hyperliquid:mainnet", "HYPE", 8)
                }
                FundingRoute::Hyperevm => ("ethereum:999:native", "ethereum:999", "native", 18),
                FundingRoute::Ethereum => ("ethereum:1:native", "ethereum:1", "native", 18),
                FundingRoute::Arbitrum => ("ethereum:42161:native", "ethereum:42161", "native", 18),
                FundingRoute::Solana => (
                    "solana:mainnet-beta:native",
                    "solana:mainnet-beta",
                    "native",
                    9,
                ),
            };
            if asset_name != expected {
                return Err(GoFinError::validation(
                    "Funding native fee asset differs from the reviewed route identity.",
                ));
            }
            let native = if intent.route == FundingRoute::Core {
                // HyperCore's fee atom is eight-decimal HYPE, while broker
                // prices use the venue's canonical 36-decimal asset identity.
                gofin_hyperliquid::profile::asset("HYPE")
            } else {
                gofin_domain::AssetId {
                    network: fee_network.into(),
                    reference: fee_reference.into(),
                    decimals: fee_decimals,
                }
            };
            let price = self.valuator.asset_price(&native).await?;
            let native_fee_atoms = if intent.route == FundingRoute::Core {
                format!("{}{}", cap, "0".repeat(28))
            } else {
                cap.clone()
            };
            let native_fee_usd = price.usd_micros_per_unit.mul_div_ceil(
                &gofin_domain::Quantity::new(native_fee_atoms)?,
                &price.unit_atoms,
            )?;
            fee_usd = fee_usd.checked_add(&native_fee_usd)?;
            prices.push(price);
        }
        let observed_at = prices
            .iter()
            .map(|price| price.observed_at.as_str())
            .min()
            .unwrap_or("")
            .to_owned();
        let source = prices
            .iter()
            .map(|price| price.source.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Ok(Some(gofin_domain::Valuation {
            movement_usd_micros: movement,
            maximum_fee_usd_micros: fee_usd,
            additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
            observed_at,
            source,
            prices,
        }))
    }
    async fn funding_continuation_valuation(
        &self,
        binding: &FundingContinuationBinding,
    ) -> Result<Option<gofin_domain::Valuation>> {
        binding.validate()?;
        let stored = self.load_funding_continuation(binding)?;
        let (parent, _, plan) = self.load_withdrawal_parent(&binding.route_id)?;
        if parent.status != gofin_domain::Status::Completed
            || parent.submission.as_ref() != Some(&binding.parent_submission)
            || plan.intent != binding.intent
            || stored.operation_id != binding.operation_id
            || stored.plan_fingerprint != binding.plan_fingerprint
            || stored.stage1_proof_fingerprint != binding.stage1_proof_fingerprint
        {
            return Err(GoFinError::validation(
                "Continuation valuation differs from the exact completed withdrawal stage.",
            ));
        }
        let usdc = self
            .valuator
            .asset_price(&underlying_hyperevm_usdc())
            .await?;
        let core_usdc = gofin_domain::AssetPrice {
            asset: gofin_hyperliquid::profile::asset("USDC"),
            unit_atoms: gofin_domain::Quantity::new("1000000000000000000000000000000000000")?,
            usd_micros_per_unit: usdc.usd_micros_per_unit.clone(),
            observed_at: usdc.observed_at.clone(),
            source: format!("{}; HyperCore USDC 36dp denomination mapping", usdc.source),
        };
        let movement = core_usdc.usd_micros_per_unit.mul_div_ceil(
            &gofin_domain::Quantity::new(format!(
                "{}000000000000000000000000000000",
                binding.bounds.source_usdc_atoms
            ))?,
            &core_usdc.unit_atoms,
        )?;
        let mut fee_usd = gofin_domain::Quantity::from_u64(0);
        let usdc_fee_atoms = binding
            .bounds
            .maximum_source_fee_usdc_atoms
            .checked_add(binding.bounds.maximum_cctp_fee_usdc_atoms)
            .and_then(|value| value.checked_add(binding.bounds.maximum_activation_fee_atoms))
            .ok_or_else(|| invalid_response("Continuation fee bound overflow."))?;
        if usdc_fee_atoms > 0 {
            let raw = format!("{usdc_fee_atoms}000000000000000000000000000000");
            fee_usd = fee_usd.checked_add(
                &core_usdc
                    .usd_micros_per_unit
                    .mul_div_ceil(&gofin_domain::Quantity::new(raw)?, &core_usdc.unit_atoms)?,
            )?;
        }
        let mut prices = vec![core_usdc];
        if let (Some(asset), Some(cap)) = (
            binding.bounds.native_fee_asset.as_deref(),
            binding.bounds.maximum_native_fee_atoms.as_deref(),
        ) {
            if asset != "hyperliquid:mainnet:HYPE" {
                return Err(GoFinError::validation(
                    "Continuation native fee asset is not the reviewed HyperCore HYPE identity.",
                ));
            }
            let cap = cap
                .parse::<u64>()
                .map_err(|_| invalid_response("Continuation HYPE fee cap is malformed."))?;
            if cap == 0 || cap > 100_000 {
                return Err(GoFinError::validation(
                    "Continuation HYPE fee cap exceeds the reviewed bound.",
                ));
            }
            let hype = gofin_hyperliquid::profile::asset("HYPE");
            let price = self.valuator.asset_price(&hype).await?;
            let wire_to_profile_atoms = format!("{cap}{}", "0".repeat(28));
            fee_usd = fee_usd.checked_add(&price.usd_micros_per_unit.mul_div_ceil(
                &gofin_domain::Quantity::new(wire_to_profile_atoms)?,
                &price.unit_atoms,
            )?)?;
            prices.push(price);
        } else if binding.bounds.native_fee_asset.is_some()
            || binding.bounds.maximum_native_fee_atoms.is_some()
        {
            return Err(invalid_response(
                "Continuation native fee bound has mismatched asset and amount.",
            ));
        }
        let observed_at = prices
            .iter()
            .map(|price| price.observed_at.as_str())
            .min()
            .unwrap_or("")
            .to_owned();
        let source = prices
            .iter()
            .map(|price| price.source.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Ok(Some(gofin_domain::Valuation {
            movement_usd_micros: movement,
            maximum_fee_usd_micros: fee_usd,
            additional_allocation_usd_micros: gofin_domain::Quantity::from_u64(0),
            observed_at,
            source,
            prices,
        }))
    }
    async fn prepare_funding(
        &self,
        intent: FundingIntent,
        operation_id: &str,
    ) -> Result<FundingBinding> {
        let plan = crate::papertrade_funding::prepare_funding(
            self.funding_host.as_ref(),
            operation_id,
            intent,
        )
        .await?;
        let transaction = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::EvmCall { transaction, .. } => {
                Some(transaction.clone())
            }
            _ => None,
        });
        let papertrade_intent = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::PapertradeWithdrawToCore { bound } => {
                Some(bound.clone())
            }
            _ => None,
        });
        let solana_transaction = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::SolanaCctpBurn { transaction, .. } => {
                Some(transaction.clone())
            }
            _ => None,
        });
        let solana_message_event_account = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::SolanaCctpBurn { request, .. } => {
                Some(request.message_sent_event_data.clone())
            }
            _ => None,
        });
        let binding = FundingBinding {
            version: plan.version,
            operation_id: plan.operation_id.clone(),
            intent: plan.intent.clone(),
            source_identity: plan.source_identity.clone(),
            destination_identity: plan.destination_identity.clone(),
            bounds: plan.bounds.clone(),
            transaction,
            solana_transaction,
            solana_message_event_account,
            papertrade_intent,
        };
        self.credentials.put_papertrade_runtime_record(
            &funding_record_key(operation_id),
            &serde_json::to_value(&plan)
                .map_err(|_| unavailable("Funding plan could not be protected."))?,
        )?;
        Ok(binding)
    }
    async fn dispatch_funding(
        &self,
        binding: &FundingBinding,
        outer_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<FundingProgress> {
        let plan = self.load_funding_plan(binding)?;
        crate::papertrade_funding::dispatch_funding(
            self.funding_host.as_ref(),
            &plan,
            outer_submission,
            journal,
            validated,
        )
        .await
    }
    async fn reconcile_funding(&self, binding: &FundingBinding) -> Result<FundingProgress> {
        let plan = self.load_funding_plan(binding)?;
        let mut progress =
            crate::papertrade_funding::reconcile_funding(self.funding_host.as_ref(), &plan).await?;
        if progress.status == gofin_papertrade::FundingProgressStatus::Rejected
            && plan.intent.route == gofin_papertrade::FundingRoute::Core
            && plan.intent.direction == gofin_papertrade::FundingDirection::Deposit
            && plan.legs.len() == 1
            && let Some(proof) = self.proven_core_nonce_invalidation(&plan)?
            && let Some(evidence) = progress.evidence.as_object_mut()
        {
            evidence.insert("core_deposit_nonce_recovery_terminal".into(), proof);
        }
        Ok(progress)
    }

    async fn prepare_funding_continuation(
        &self,
        route_id: &str,
        operation_id: &str,
    ) -> Result<FundingContinuationBinding> {
        let (parent, parent_binding, plan) = self.load_withdrawal_parent(route_id)?;
        let parent_submission = parent
            .submission
            .clone()
            .ok_or_else(|| unavailable("Settled stage-one withdrawal submission is missing."))?;
        if parent.status != gofin_domain::Status::Completed
            || !parent_submission_matches_funding(&parent_submission, route_id)
        {
            return Err(GoFinError::validation(
                "Funding continuation requires the exact completed Papertrade withdrawal stage.",
            ));
        }
        if parent_binding.intent.owner != self.account.owner.to_ascii_lowercase() {
            return Err(GoFinError::validation(
                "Funding continuation parent belongs to a different enrolled owner.",
            ));
        }
        let stored = crate::papertrade_funding::prepare_funding_continuation(
            self.funding_host.as_ref(),
            &plan,
            &parent_submission,
            operation_id,
        )
        .await?;
        let binding = FundingContinuationBinding {
            version: stored.version,
            operation_id: stored.operation_id.clone(),
            route_id: stored.route_id.clone(),
            parent_submission: stored.parent_submission.clone(),
            plan_fingerprint: stored.plan_fingerprint.clone(),
            stage1_proof_fingerprint: stored.stage1_proof_fingerprint.clone(),
            intent: stored.intent.clone(),
            source_identity: stored.source_identity.clone(),
            destination_identity: stored.destination_identity.clone(),
            bounds: stored.bounds.clone(),
        };
        binding.validate()?;
        let key = funding_continuation_record_key(operation_id);
        let serialized = serde_json::to_value(&stored)
            .map_err(|_| unavailable("Funding continuation could not be protected."))?;
        if let Some(existing) = self.credentials.papertrade_runtime_record(&key)? {
            let existing: crate::papertrade_funding::BoundFundingContinuation =
                serde_json::from_value(existing).map_err(|_| {
                    invalid_response("Protected funding continuation is malformed.")
                })?;
            if existing != stored {
                return Err(GoFinError::validation(
                    "Continuation operation ID is already bound to a different withdrawal stage.",
                ));
            }
        } else {
            self.credentials
                .put_papertrade_runtime_record(&key, &serialized)?;
        }
        Ok(binding)
    }

    async fn revalidate_funding_continuation(
        &self,
        binding: &FundingContinuationBinding,
    ) -> Result<()> {
        binding.validate()?;
        let stored = self.load_funding_continuation(binding)?;
        let (parent, _, plan) = self.load_withdrawal_parent(&binding.route_id)?;
        if parent.status != gofin_domain::Status::Completed
            || parent.submission.as_ref() != Some(&binding.parent_submission)
        {
            return Err(GoFinError::validation(
                "Funding continuation parent is no longer the exact completed stage-one withdrawal.",
            ));
        }
        let progress = crate::papertrade_funding::reconcile_funding_continuation(
            self.funding_host.as_ref(),
            &plan,
            &stored,
        )
        .await?;
        if progress.status == gofin_papertrade::FundingProgressStatus::Rejected {
            return Err(GoFinError::new(
                "PAPERTRADE_FUNDING_CONTINUATION_REJECTED",
                "revalidation",
                "The onward funding leg has a canonical rejected outcome.",
                "Reconcile the existing continuation; do not submit it again.",
            ));
        }
        Ok(())
    }

    async fn dispatch_funding_continuation(
        &self,
        binding: &FundingContinuationBinding,
        outer_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<FundingProgress> {
        binding.validate()?;
        if outer_submission.kind != "papertrade_funding_continuation"
            || outer_submission.reference != binding.operation_id
            || outer_submission
                .data
                .get("operation_id")
                .and_then(Value::as_str)
                != Some(binding.operation_id.as_str())
            || outer_submission
                .data
                .get("route_id")
                .and_then(Value::as_str)
                != Some(binding.route_id.as_str())
        {
            return Err(GoFinError::validation(
                "Continuation dispatch submission does not match the immutable stage-two binding.",
            ));
        }
        let stored = self.load_funding_continuation(binding)?;
        let (parent, _, plan) = self.load_withdrawal_parent(&binding.route_id)?;
        if parent.status != gofin_domain::Status::Completed
            || parent.submission.as_ref() != Some(&binding.parent_submission)
        {
            return Err(GoFinError::validation(
                "Continuation dispatch requires the exact completed stage-one withdrawal.",
            ));
        }
        crate::papertrade_funding::dispatch_funding_continuation(
            self.funding_host.as_ref(),
            &plan,
            &stored,
            outer_submission,
            journal,
            validated,
        )
        .await
    }

    async fn reconcile_funding_continuation(
        &self,
        binding: &FundingContinuationBinding,
    ) -> Result<FundingProgress> {
        binding.validate()?;
        let stored = self.load_funding_continuation(binding)?;
        let (parent, _, plan) = self.load_withdrawal_parent(&binding.route_id)?;
        if parent.status != gofin_domain::Status::Completed
            || parent.submission.as_ref() != Some(&binding.parent_submission)
        {
            return Ok(FundingProgress {
                status: gofin_papertrade::FundingProgressStatus::Unknown,
                receipt: None,
                evidence: json!({
                    "reason":"The exact stage-one parent is no longer available as a completed withdrawal.",
                    "route_id":binding.route_id,
                }),
            });
        }
        crate::papertrade_funding::reconcile_funding_continuation(
            self.funding_host.as_ref(),
            &plan,
            &stored,
        )
        .await
    }

    async fn prepare_funding_nonce_recovery(
        &self,
        parent_operation_id: &str,
        child_operation_id: &str,
    ) -> Result<CoreDepositNonceRecoveryBinding> {
        let child_id_is_valid = child_operation_id
            .strip_prefix("execution:")
            .unwrap_or(child_operation_id)
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:.".contains(&byte));
        if child_operation_id.trim().is_empty()
            || child_operation_id.len() > 512
            || !child_id_is_valid
        {
            return Err(GoFinError::validation(
                "Nonce recovery requires the engine's stable child operation identity.",
            ));
        }
        let (parent, plan, leg, _nonce, plan_fingerprint, _action_fingerprint) =
            self.core_deposit_nonce_recovery_context(parent_operation_id)?;
        if !matches!(
            parent.status,
            gofin_domain::Status::Unknown
                | gofin_domain::Status::Pending
                | gofin_domain::Status::Submitted
        ) {
            return Err(GoFinError::validation(
                "Nonce recovery requires an unresolved original deposit.",
            ));
        }
        let proof_key = format!(
            "papertrade-funding:{}:nonce_invalidation_proof",
            hex::encode(leg.submission_id.as_bytes())
        );
        if self
            .credentials
            .papertrade_runtime_record(&proof_key)?
            .is_some()
        {
            return Err(GoFinError::validation(
                "Original Core deposit already has a nonce-invalidation proof; reconcile it.",
            ));
        }
        let existing_child = self
            .execution_state()?
            .recovery_child(parent_operation_id)?;
        if let Some(child) = existing_child {
            let same_unsubmitted_preview = child.operation == "papertrade.deposit.invalidate_nonce"
                && format!("execution:{}", child.request_id) == child_operation_id
                && child.submission.is_none()
                && child.status == gofin_domain::Status::AwaitingApproval;
            let retryable_unsubmitted_terminal = child.operation
                == "papertrade.deposit.invalidate_nonce"
                && child.submission.is_none()
                && matches!(
                    child.status,
                    gofin_domain::Status::Failed
                        | gofin_domain::Status::Rejected
                        | gofin_domain::Status::Expired
                )
                && self
                    .credentials
                    .papertrade_runtime_record(&format!(
                        "papertrade-funding:{}:nonce_invalidation_attempted",
                        hex::encode(leg.submission_id.as_bytes())
                    ))?
                    .is_none();
            if !(same_unsubmitted_preview || retryable_unsubmitted_terminal) {
                return Err(GoFinError::new(
                    "PAPERTRADE_NONCE_RECOVERY_ALREADY_ATTEMPTED",
                    "preflight",
                    "A different or already-submitted nonce recovery child exists for this deposit.",
                    "Reconcile the exact linked recovery child; do not create another Noop action.",
                ));
            }
        }
        let binding = CoreDepositNonceRecoveryBinding {
            version: 1,
            child_operation_id: child_operation_id.to_owned(),
            parent_operation_id: parent.id.clone(),
            parent_submission: parent.submission.ok_or_else(|| {
                GoFinError::validation("Original Papertrade deposit submission is unavailable.")
            })?,
            owner: plan.intent.owner.to_ascii_lowercase(),
            route_id: plan.operation_id.clone(),
            plan_fingerprint,
        };
        binding.validate()?;
        let binding_key = Self::core_nonce_recovery_binding_key(child_operation_id);
        let binding_value = serde_json::to_value(&binding)
            .map_err(|_| unavailable("Nonce recovery binding could not be protected."))?;
        if let Some(existing) = self.credentials.papertrade_runtime_record(&binding_key)? {
            let existing: CoreDepositNonceRecoveryBinding = serde_json::from_value(existing)
                .map_err(|_| invalid_response("Protected nonce recovery binding is malformed."))?;
            if existing != binding {
                return Err(GoFinError::validation(
                    "A different immutable nonce recovery binding already exists for this child.",
                ));
            }
        } else {
            self.credentials
                .put_papertrade_runtime_record(&binding_key, &binding_value)?;
        }
        Ok(binding)
    }

    async fn revalidate_funding_nonce_recovery(
        &self,
        binding: &CoreDepositNonceRecoveryBinding,
    ) -> Result<()> {
        binding.validate()?;
        let stored: CoreDepositNonceRecoveryBinding = serde_json::from_value(
            self.credentials
                .papertrade_runtime_record(&Self::core_nonce_recovery_binding_key(
                    &binding.child_operation_id,
                ))?
                .ok_or_else(|| unavailable("Protected nonce recovery binding is unavailable."))?,
        )
        .map_err(|_| invalid_response("Protected nonce recovery binding is malformed."))?;
        if stored != *binding {
            return Err(GoFinError::validation(
                "Nonce recovery binding differs from the protected immutable child record.",
            ));
        }
        let (parent, _plan, _leg, _nonce, fingerprint, _action_fingerprint) =
            self.core_deposit_nonce_recovery_context(&binding.parent_operation_id)?;
        if parent.submission.as_ref() != Some(&binding.parent_submission)
            || !binding.owner.eq_ignore_ascii_case(&self.account.owner)
            || binding.route_id != binding.parent_submission.reference
            || binding.route_id != _plan.operation_id
            || binding.plan_fingerprint != fingerprint
        {
            return Err(GoFinError::validation(
                "Original Core deposit changed after nonce recovery preparation.",
            ));
        }
        Ok(())
    }

    async fn dispatch_funding_nonce_recovery(
        &self,
        binding: &CoreDepositNonceRecoveryBinding,
        outer_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<CoreDepositNonceRecoveryProgress> {
        self.revalidate_funding_nonce_recovery(binding).await?;
        let prepared = validated.prepared();
        if prepared.operation != "papertrade.deposit.invalidate_nonce"
            || gofin_domain::RecoveryBinding::from_prepared_binding(&prepared.binding)?.is_none_or(
                |recovery| {
                    recovery.parent_operation_id != binding.parent_operation_id
                        || recovery.parent_submission != binding.parent_submission
                },
            )
        {
            return Err(GoFinError::validation(
                "Nonce recovery child is not linked to the exact unresolved deposit submission.",
            ));
        }
        journal.assert_candidate(prepared)?;
        journal.assert_authorized()?;
        validated.assert_fresh()?;
        let child = self
            .execution_state()?
            .recovery_child(&binding.parent_operation_id)?
            .ok_or_else(|| {
                unavailable("Durable nonce recovery child is not linked to its parent.")
            })?;
        if format!("execution:{}", child.request_id) != binding.child_operation_id
            || child.operation != prepared.operation
            || child.submission.as_ref() != Some(outer_submission)
            || !matches!(
                child.status,
                gofin_domain::Status::Submitted
                    | gofin_domain::Status::Pending
                    | gofin_domain::Status::Unknown
            )
        {
            return Err(GoFinError::validation(
                "Durable recovery child identity differs from its approved submission.",
            ));
        }
        let (parent, plan, leg, nonce, plan_fingerprint, action_fingerprint) =
            self.core_deposit_nonce_recovery_context(&binding.parent_operation_id)?;
        if parent.submission.as_ref() != Some(&binding.parent_submission)
            || binding.plan_fingerprint != plan_fingerprint
        {
            return Err(GoFinError::validation(
                "Core nonce recovery parent or plan changed before dispatch.",
            ));
        }
        let acknowledgement = match self
            .funding_host
            .invalidate_core_deposit_nonce(
                &plan.operation_id,
                &parent.id,
                &plan.intent.owner,
                nonce,
                &binding.child_operation_id,
                outer_submission,
                validated,
                journal,
            )
            .await
        {
            Ok(value)
                if value["kind"] == "hyperliquid_noop_nonce_consumed"
                    && value["owner"]
                        .as_str()
                        .is_some_and(|owner| owner.eq_ignore_ascii_case(&plan.intent.owner))
                    && value["nonce"].as_u64() == Some(nonce)
                    && value["action_fingerprint"].as_str() == Some(&action_fingerprint)
                    && value["child_operation_id"].as_str()
                        == Some(binding.child_operation_id.as_str())
                    && value["child_submission_id"].as_str()
                        == Some(outer_submission.reference.as_str())
                    && value["response_status"] == "ok"
                    && value["response_type"] == "default" =>
            {
                value
            }
            Ok(_) => {
                return Ok(CoreDepositNonceRecoveryProgress {
                    status: CoreDepositNonceRecoveryStatus::Unknown,
                    proof: None,
                    evidence: json!({"reason":"No exact successful Noop acknowledgement was returned; keep the original deposit unresolved"}),
                });
            }
            Err(error) => {
                return Ok(CoreDepositNonceRecoveryProgress {
                    status: CoreDepositNonceRecoveryStatus::Unknown,
                    proof: None,
                    evidence: json!({"reason":"Noop submission outcome is unresolved; reconcile this exact recovery child","error_code":error.code}),
                });
            }
        };
        let evidence_id = recovery_evidence_id(binding, outer_submission, &action_fingerprint)?;
        let proof = CoreDepositNonceRecoveryProof {
            version: 1,
            child_operation_id: binding.child_operation_id.clone(),
            child_record_id: child.id.clone(),
            child_submission: outer_submission.clone(),
            parent_operation_id: binding.parent_operation_id.clone(),
            parent_submission: binding.parent_submission.clone(),
            owner: binding.owner.clone(),
            route_id: binding.route_id.clone(),
            plan_fingerprint: binding.plan_fingerprint.clone(),
            evidence_id: evidence_id.clone(),
        };
        proof.validate_against(binding)?;
        let proof_value = json!({"version":1,"proof":proof,"acknowledgement":acknowledgement});
        self.credentials.put_papertrade_runtime_record(
            &Self::core_nonce_recovery_ack_key(&binding.child_operation_id),
            &proof_value,
        )?;
        let _ = leg;
        Ok(CoreDepositNonceRecoveryProgress {
            status: CoreDepositNonceRecoveryStatus::Settled,
            proof: Some(proof),
            evidence: json!({"kind":"exact_core_deposit_nonce_noop_acknowledgement","evidence_id":evidence_id}),
        })
    }

    async fn reconcile_funding_nonce_recovery(
        &self,
        binding: &CoreDepositNonceRecoveryBinding,
        child_submission: Option<&gofin_domain::Submission>,
    ) -> Result<CoreDepositNonceRecoveryProgress> {
        binding.validate()?;
        let stored_binding: CoreDepositNonceRecoveryBinding = serde_json::from_value(
            self.credentials
                .papertrade_runtime_record(&Self::core_nonce_recovery_binding_key(
                    &binding.child_operation_id,
                ))?
                .ok_or_else(|| unavailable("Protected nonce recovery binding is unavailable."))?,
        )
        .map_err(|_| invalid_response("Protected nonce recovery binding is malformed."))?;
        if stored_binding != *binding {
            return Err(GoFinError::validation(
                "Nonce recovery binding differs from the protected immutable child record.",
            ));
        }
        let (parent, plan, leg, expected_nonce, fingerprint, expected_action_fingerprint) =
            self.core_deposit_nonce_recovery_context(&binding.parent_operation_id)?;
        if parent.submission.as_ref() != Some(&binding.parent_submission)
            || plan.operation_id != binding.route_id
            || fingerprint != binding.plan_fingerprint
        {
            return Err(GoFinError::validation(
                "Nonce recovery parent differs from the immutable protected binding.",
            ));
        }
        let Some(child) = self
            .execution_state()?
            .recovery_child(&binding.parent_operation_id)?
        else {
            return Ok(CoreDepositNonceRecoveryProgress {
                status: CoreDepositNonceRecoveryStatus::Unknown,
                proof: None,
                evidence: json!({"reason":"No durable linked nonce-recovery child exists"}),
            });
        };
        if format!("execution:{}", child.request_id) != binding.child_operation_id
            || child.operation != "papertrade.deposit.invalidate_nonce"
            || !child.preview.as_ref().is_some_and(|preview| {
                gofin_domain::RecoveryBinding::from_prepared_binding(&preview.binding)
                    .ok()
                    .flatten()
                    .is_some_and(|recovery| {
                        recovery.parent_operation_id == binding.parent_operation_id
                            && recovery.parent_submission == binding.parent_submission
                    })
            })
        {
            return Err(GoFinError::validation(
                "Linked nonce-recovery record does not match the exact protected parent binding.",
            ));
        }
        let Some(child_submission) = child_submission else {
            return Ok(CoreDepositNonceRecoveryProgress {
                status: CoreDepositNonceRecoveryStatus::Unknown,
                proof: None,
                evidence: json!({"reason":"Nonce-recovery child has no durable submission"}),
            });
        };
        if child.submission.as_ref() != Some(child_submission) {
            return Err(GoFinError::validation(
                "Nonce-recovery submission differs from the durable linked child.",
            ));
        }
        let stored =
            self.credentials
                .papertrade_runtime_record(&Self::core_nonce_recovery_ack_key(
                    &binding.child_operation_id,
                ))?;
        let host_proof_key = format!(
            "papertrade-funding:{}:nonce_invalidation_proof",
            hex::encode(leg.submission_id.as_bytes())
        );
        let host_proof = self
            .credentials
            .papertrade_runtime_record(&host_proof_key)?;
        let proof: CoreDepositNonceRecoveryProof = if let Some(stored) = &stored {
            serde_json::from_value(
                stored.get("proof").cloned().ok_or_else(|| {
                    invalid_response("Protected Noop recovery proof is malformed.")
                })?,
            )
            .map_err(|_| invalid_response("Protected Noop recovery proof is malformed."))?
        } else if let Some(host_proof) = &host_proof {
            if host_proof["kind"] != "hyperliquid_core_deposit_nonce_invalidated"
                || host_proof["parent_operation_id"].as_str()
                    != Some(binding.parent_operation_id.as_str())
                || host_proof["funding_plan_id"].as_str() != Some(binding.route_id.as_str())
                || host_proof["owner"]
                    .as_str()
                    .is_none_or(|owner| !owner.eq_ignore_ascii_case(&binding.owner))
                || host_proof["nonce"].as_u64() != Some(expected_nonce)
                || host_proof["action_fingerprint"].as_str()
                    != Some(expected_action_fingerprint.as_str())
                || host_proof["child_operation_id"].as_str()
                    != Some(binding.child_operation_id.as_str())
                || host_proof["child_submission_id"].as_str()
                    != Some(child_submission.reference.as_str())
            {
                return Err(GoFinError::validation(
                    "Host Noop proof does not match the exact linked child, owner, nonce and action.",
                ));
            }
            let proof = CoreDepositNonceRecoveryProof {
                version: 1,
                child_operation_id: binding.child_operation_id.clone(),
                child_record_id: child.id.clone(),
                child_submission: child_submission.clone(),
                parent_operation_id: binding.parent_operation_id.clone(),
                parent_submission: binding.parent_submission.clone(),
                owner: binding.owner.clone(),
                route_id: binding.route_id.clone(),
                plan_fingerprint: binding.plan_fingerprint.clone(),
                evidence_id: recovery_evidence_id(
                    binding,
                    child_submission,
                    &expected_action_fingerprint,
                )?,
            };
            proof.validate_against(binding)?;
            let ack = json!({
                "kind":"hyperliquid_noop_nonce_consumed",
                "owner":binding.owner,
                "nonce":expected_nonce,
                "response_status":"ok",
                "response_type":"default",
                "action_fingerprint":expected_action_fingerprint,
                "child_operation_id":binding.child_operation_id,
                "child_submission_id":child_submission.reference
            });
            self.credentials.put_papertrade_runtime_record(
                &Self::core_nonce_recovery_ack_key(&binding.child_operation_id),
                &json!({"version":1,"proof":proof,"acknowledgement":ack}),
            )?;
            proof
        } else {
            return Ok(CoreDepositNonceRecoveryProgress {
                status: if child.status.terminal() {
                    CoreDepositNonceRecoveryStatus::Rejected
                } else {
                    CoreDepositNonceRecoveryStatus::Unknown
                },
                proof: None,
                evidence: json!({"reason":"No durable exact successful Noop acknowledgement is recorded"}),
            });
        };
        proof.validate_against(binding)?;
        let stored_ack = stored
            .as_ref()
            .and_then(|value| value.get("acknowledgement"))
            .cloned()
            .or_else(|| {
                host_proof.as_ref().map(|host_proof| {
                    json!({
                        "owner":host_proof["owner"],
                        "nonce":host_proof["nonce"],
                        "response_status":"ok",
                        "response_type":"default",
                        "action_fingerprint":host_proof["action_fingerprint"],
                        "child_operation_id":host_proof["child_operation_id"],
                        "child_submission_id":host_proof["child_submission_id"]
                    })
                })
            })
            .unwrap_or(Value::Null);
        if !matches!(
            child.status,
            gofin_domain::Status::Submitted
                | gofin_domain::Status::Pending
                | gofin_domain::Status::Unknown
                | gofin_domain::Status::Completed
        ) || proof.child_record_id != child.id
            || proof.child_submission != *child_submission
            || !stored_ack["owner"]
                .as_str()
                .is_some_and(|owner| owner.eq_ignore_ascii_case(&binding.owner))
            || stored_ack["nonce"].as_u64() != Some(expected_nonce)
            || stored_ack["action_fingerprint"].as_str()
                != Some(expected_action_fingerprint.as_str())
            || stored_ack["child_operation_id"].as_str()
                != Some(binding.child_operation_id.as_str())
            || stored_ack["child_submission_id"].as_str()
                != Some(child_submission.reference.as_str())
            || stored_ack["response_status"] != "ok"
            || stored_ack["response_type"] != "default"
        {
            return Ok(CoreDepositNonceRecoveryProgress {
                status: CoreDepositNonceRecoveryStatus::Unknown,
                proof: None,
                evidence: json!({"reason":"Durable child is not completed with the exact acknowledged Noop"}),
            });
        }
        Ok(CoreDepositNonceRecoveryProgress {
            status: CoreDepositNonceRecoveryStatus::Settled,
            proof: Some(proof.clone()),
            evidence: json!({"kind":"exact_core_deposit_nonce_noop_acknowledgement","evidence_id":proof.evidence_id}),
        })
    }
}

impl BrokerPapertradeRuntime {
    async fn signer_for_bound(
        &self,
        bound: &BoundIntent,
        now: u64,
    ) -> Result<Arc<gofin_evm::LocalEnvelopeSigner>> {
        if matches!(
            bound.typed,
            PapertradeTypedIntent::RegisterSessionKey { .. }
                | PapertradeTypedIntent::Stake { .. }
                | PapertradeTypedIntent::Unstake { .. }
                | PapertradeTypedIntent::Claim { .. }
        ) {
            return self.owner_signer(typed_owner(&bound.typed));
        }
        let expected_registration = bound.registration_id.as_deref().ok_or_else(|| {
            GoFinError::validation("Papertrade session identity is missing from intent binding.")
        })?;
        let (registration_id, signer, _, expiry) = self
            .session_signer(typed_owner(&bound.typed), expected_registration, now)
            .await?;
        if registration_id != expected_registration || expiry < intent_deadline(&bound.typed) {
            return Err(GoFinError::validation(
                "Papertrade session identity or expiry changed after preparation.",
            ));
        }
        let (address, id) = signer.papertrade_intent_identity(&to_evm_intent(&bound.typed), now)?;
        if !address.eq_ignore_ascii_case(&bound.signer)
            || !id.eq_ignore_ascii_case(&bound.intent_id)
        {
            return Err(GoFinError::validation(
                "Papertrade typed intent is not bound to the active protected session signer.",
            ));
        }
        Ok(signer)
    }

    fn load_funding_plan(
        &self,
        binding: &FundingBinding,
    ) -> Result<crate::papertrade_funding::BoundFunding> {
        let value = self
            .credentials
            .papertrade_runtime_record(&funding_record_key(&binding.operation_id))?
            .ok_or_else(|| unavailable("Protected Papertrade funding plan is unavailable."))?;
        let plan: crate::papertrade_funding::BoundFunding = serde_json::from_value(value)
            .map_err(|_| invalid_response("Protected funding plan is malformed."))?;
        let transaction = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::EvmCall { transaction, .. } => {
                Some(transaction)
            }
            _ => None,
        });
        let papertrade_intent = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::PapertradeWithdrawToCore { bound } => {
                Some(bound)
            }
            _ => None,
        });
        let solana_transaction = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::SolanaCctpBurn { transaction, .. } => {
                Some(transaction)
            }
            _ => None,
        });
        let solana_message_event_account = plan.legs.iter().find_map(|leg| match &leg.action {
            crate::papertrade_funding::FundingAction::SolanaCctpBurn { request, .. } => {
                Some(&request.message_sent_event_data)
            }
            _ => None,
        });
        if binding.version != plan.version
            || binding.operation_id != plan.operation_id
            || binding.intent != plan.intent
            || binding.source_identity != plan.source_identity
            || binding.destination_identity != plan.destination_identity
            || binding.bounds != plan.bounds
            || binding.transaction.as_ref() != transaction
            || binding.solana_transaction.as_ref() != solana_transaction
            || binding.solana_message_event_account.as_ref() != solana_message_event_account
            || binding.papertrade_intent.as_ref() != papertrade_intent
        {
            return Err(GoFinError::validation(
                "Public funding binding differs from protected immutable plan.",
            ));
        }
        Ok(plan)
    }

    fn load_withdrawal_parent(
        &self,
        route_id: &str,
    ) -> Result<(
        gofin_domain::OperationRecord,
        FundingBinding,
        crate::papertrade_funding::BoundFunding,
    )> {
        if route_id.trim().is_empty() {
            return Err(GoFinError::validation("Funding route ID is required."));
        }
        let candidates = self.execution_state()?.operations_by_prepared_binding(
            "papertrade.withdraw",
            "/preview/binding/funding_binding/operation_id",
            &json!(route_id),
        )?;
        if candidates.len() != 1 {
            return Err(unavailable(
                "The exact stage-one withdrawal record is missing or ambiguous.",
            ));
        }
        let parent = candidates
            .into_iter()
            .next()
            .ok_or_else(|| unavailable("The exact stage-one withdrawal record is unavailable."))?;
        if parent.operation != "papertrade.withdraw"
            || parent.status != gofin_domain::Status::Completed
        {
            return Err(GoFinError::validation(
                "Funding continuation requires a completed Papertrade withdrawal record.",
            ));
        }
        let submission = parent
            .submission
            .as_ref()
            .ok_or_else(|| invalid_response("Stage-one withdrawal submission is missing."))?;
        if !parent_submission_matches_funding(submission, route_id) {
            return Err(GoFinError::validation(
                "Stage-one withdrawal submission does not match its protected route identity.",
            ));
        }
        let preview = parent
            .preview
            .as_ref()
            .ok_or_else(|| invalid_response("Stage-one withdrawal preview is missing."))?;
        if preview.operation != "papertrade.withdraw" {
            return Err(GoFinError::validation(
                "Stage-one withdrawal preview has an unexpected operation.",
            ));
        }
        let binding: FundingBinding = serde_json::from_value(
            preview
                .binding
                .get("funding_binding")
                .cloned()
                .ok_or_else(|| invalid_response("Stage-one funding binding is missing."))?,
        )
        .map_err(|_| invalid_response("Stage-one funding binding is malformed."))?;
        if binding.operation_id != route_id
            || binding.intent.direction != gofin_papertrade::FundingDirection::Withdraw
            || binding.intent.route == gofin_papertrade::FundingRoute::Core
            || binding.intent.owner != self.account.owner.to_ascii_lowercase()
        {
            return Err(GoFinError::validation(
                "Stage-one funding plan is not an onward withdrawal for this enrolled owner.",
            ));
        }
        let plan = self.load_funding_plan(&binding)?;
        Ok((parent, binding, plan))
    }

    fn load_funding_continuation(
        &self,
        binding: &FundingContinuationBinding,
    ) -> Result<crate::papertrade_funding::BoundFundingContinuation> {
        let value = self
            .credentials
            .papertrade_runtime_record(&funding_continuation_record_key(&binding.operation_id))?
            .ok_or_else(|| unavailable("Protected funding continuation is unavailable."))?;
        let stored: crate::papertrade_funding::BoundFundingContinuation =
            serde_json::from_value(value)
                .map_err(|_| invalid_response("Protected funding continuation is malformed."))?;
        let public = FundingContinuationBinding {
            version: stored.version,
            operation_id: stored.operation_id.clone(),
            route_id: stored.route_id.clone(),
            parent_submission: stored.parent_submission.clone(),
            plan_fingerprint: stored.plan_fingerprint.clone(),
            stage1_proof_fingerprint: stored.stage1_proof_fingerprint.clone(),
            intent: stored.intent.clone(),
            source_identity: stored.source_identity.clone(),
            destination_identity: stored.destination_identity.clone(),
            bounds: stored.bounds.clone(),
        };
        if &public != binding {
            return Err(GoFinError::validation(
                "Public continuation binding differs from its protected immutable record.",
            ));
        }
        Ok(stored)
    }

    fn proven_core_nonce_invalidation(
        &self,
        plan: &crate::papertrade_funding::BoundFunding,
    ) -> Result<Option<Value>> {
        let leg = plan
            .legs
            .first()
            .ok_or_else(|| invalid_response("Core deposit leg is missing."))?;
        let Some(host_proof) = self.credentials.papertrade_runtime_record(&format!(
            "papertrade-funding:{}:nonce_invalidation_proof",
            hex::encode(leg.submission_id.as_bytes())
        ))?
        else {
            return Ok(None);
        };
        let Some(parent_record_id) = host_proof["parent_operation_id"].as_str() else {
            return Ok(None);
        };
        let Some(parent) = self.execution_state()?.operation(parent_record_id)? else {
            return Ok(None);
        };
        let Some(parent_submission) = parent.submission.as_ref() else {
            return Ok(None);
        };
        let expected_nonce = match &leg.action {
            crate::papertrade_funding::FundingAction::CoreDeposit { action } => match action {
                gofin_hyperliquid::funding::CoreToPapertradeAction::SpotSend(action) => action.time,
                gofin_hyperliquid::funding::CoreToPapertradeAction::SendAsset(action) => {
                    action.nonce
                }
            },
            _ => return Ok(None),
        };
        let action_fingerprint =
            crate::papertrade_funding_host::MultiRouteFundingServices::core_action_fingerprint(
                leg,
            )?;
        if host_proof["version"].as_u64() != Some(1)
            || host_proof["kind"] != "hyperliquid_core_deposit_nonce_invalidated"
            || host_proof["parent_operation_id"].as_str() != Some(parent.id.as_str())
            || host_proof["funding_plan_id"].as_str() != Some(plan.operation_id.as_str())
            || host_proof["owner"]
                .as_str()
                .is_none_or(|owner| !owner.eq_ignore_ascii_case(&plan.intent.owner))
            || host_proof["nonce"].as_u64() != Some(expected_nonce)
            || host_proof["action_fingerprint"].as_str() != Some(action_fingerprint.as_str())
            || parent.id != parent_record_id
            || parent.operation != "papertrade.deposit"
            || parent_submission.kind != "papertrade_funding"
            || parent_submission.reference != plan.operation_id
            || parent_submission.data["operation_id"] != plan.operation_id
        {
            return Ok(None);
        }
        let Some(child) = self.execution_state()?.recovery_child(&parent.id)? else {
            return Ok(None);
        };
        let child_operation_id = format!("execution:{}", child.request_id);
        if child.operation != "papertrade.deposit.invalidate_nonce"
            || child.status != gofin_domain::Status::Completed
            || host_proof["child_operation_id"].as_str() != Some(child_operation_id.as_str())
            || child.submission.as_ref().is_none_or(|submission| {
                host_proof["child_submission_id"].as_str() != Some(submission.reference.as_str())
            })
        {
            return Ok(None);
        }
        let Some(preview) = child.preview.as_ref() else {
            return Ok(None);
        };
        let Some(recovery) =
            gofin_domain::RecoveryBinding::from_prepared_binding(&preview.binding)?
        else {
            return Ok(None);
        };
        if recovery.parent_operation_id != parent.id
            || recovery.parent_submission != *parent_submission
        {
            return Ok(None);
        }
        let Some(child_submission) = child.submission.as_ref() else {
            return Ok(None);
        };
        let Some(protected_binding) =
            self.credentials
                .papertrade_runtime_record(&Self::core_nonce_recovery_binding_key(
                    &child_operation_id,
                ))?
        else {
            return Ok(None);
        };
        let recovery_binding: CoreDepositNonceRecoveryBinding =
            serde_json::from_value(protected_binding)
                .map_err(|_| invalid_response("Protected nonce recovery binding is malformed."))?;
        let Some(ack) = self
            .credentials
            .papertrade_runtime_record(&Self::core_nonce_recovery_ack_key(&child_operation_id))?
        else {
            return Ok(None);
        };
        let proof: CoreDepositNonceRecoveryProof = serde_json::from_value(
            ack.get("proof")
                .cloned()
                .ok_or_else(|| invalid_response("Protected Noop proof is malformed."))?,
        )
        .map_err(|_| invalid_response("Protected Noop proof is malformed."))?;
        if recovery_binding.parent_operation_id != parent.id
            || recovery_binding.parent_submission != *parent_submission
            || recovery_binding.route_id != plan.operation_id
            || recovery_binding.child_operation_id != child_operation_id
            || proof.child_record_id != child.id
            || proof.child_submission != *child_submission
            || proof.validate_against(&recovery_binding).is_err()
        {
            return Ok(None);
        }
        Ok(Some(json!({
            "version":1,
            "parent_record_id":parent.id,
            "parent_submission":parent_submission,
            "plan_id":plan.operation_id,
            "child_record_id":child.id,
            "child_completed":true,
            "child_submission":child_submission,
            "host_proof":host_proof,
            "recovery_binding":recovery_binding,
            "proof":proof
        })))
    }

    fn core_deposit_nonce_recovery_context(
        &self,
        parent_operation_id: &str,
    ) -> Result<(
        gofin_domain::OperationRecord,
        crate::papertrade_funding::BoundFunding,
        crate::papertrade_funding::FundingLeg,
        u64,
        String,
        String,
    )> {
        let parent = self
            .execution_state()?
            .operation(parent_operation_id)?
            .ok_or_else(|| {
                unavailable("Original Papertrade deposit is not in the protected journal.")
            })?;
        let submission = parent.submission.as_ref().ok_or_else(|| {
            GoFinError::validation("Original Papertrade deposit has no durable submission.")
        })?;
        if parent.id != parent_operation_id
            || parent.operation != "papertrade.deposit"
            || !matches!(
                &parent.status,
                gofin_domain::Status::Unknown
                    | gofin_domain::Status::Pending
                    | gofin_domain::Status::Submitted
                    | gofin_domain::Status::Failed
            )
            || submission.kind != "papertrade_funding"
            || submission.reference.trim().is_empty()
            || submission.data.get("operation_id").and_then(Value::as_str)
                != Some(submission.reference.as_str())
        {
            return Err(GoFinError::validation(
                "Nonce recovery requires the exact unresolved submitted Papertrade deposit.",
            ));
        }
        let public_binding: FundingBinding = serde_json::from_value(
            parent
                .preview
                .as_ref()
                .and_then(|preview| preview.binding.pointer("/funding_binding"))
                .cloned()
                .ok_or_else(|| unavailable("Original Papertrade deposit plan is unavailable."))?,
        )
        .map_err(|_| invalid_response("Original Papertrade funding binding is malformed."))?;
        let plan = self.load_funding_plan(&public_binding)?;
        if plan.operation_id != submission.reference
            || !plan.intent.owner.eq_ignore_ascii_case(&self.account.owner)
            || plan.intent.route != gofin_papertrade::FundingRoute::Core
            || plan.intent.direction != gofin_papertrade::FundingDirection::Deposit
            || plan.legs.len() != 1
        {
            return Err(GoFinError::validation(
                "Nonce recovery is limited to this owner's exact single-leg Core deposit.",
            ));
        }
        let leg = plan.legs[0].clone();
        let nonce = match &leg.action {
            crate::papertrade_funding::FundingAction::CoreDeposit { action } => match action {
                gofin_hyperliquid::funding::CoreToPapertradeAction::SpotSend(action) => action.time,
                gofin_hyperliquid::funding::CoreToPapertradeAction::SendAsset(action) => {
                    action.nonce
                }
            },
            _ => {
                return Err(GoFinError::validation(
                    "Original Papertrade deposit has no Core owner-signed action.",
                ));
            }
        };
        let action_fingerprint =
            crate::papertrade_funding_host::MultiRouteFundingServices::core_action_fingerprint(
                &leg,
            )?;
        let attempted_key = format!(
            "papertrade-funding:{}:dispatch_attempted",
            hex::encode(leg.submission_id.as_bytes())
        );
        let attempted = self
            .credentials
            .papertrade_runtime_record(&attempted_key)?
            .ok_or_else(|| {
                GoFinError::validation(
                    "Original Core deposit has no persisted dispatch-attempt marker.",
                )
            })?;
        if attempted["nonce"].as_u64() != Some(nonce)
            || attempted["outer_submission"]
                != serde_json::to_value(submission).unwrap_or(Value::Null)
        {
            return Err(GoFinError::validation(
                "Original Core deposit attempt marker differs from its exact frozen action.",
            ));
        }
        let serialized = serde_json::to_vec(&(&plan, &leg))
            .map_err(|_| unavailable("Protected funding plan fingerprint failed."))?;
        use sha2::Digest;
        let plan_fingerprint = format!("0x{}", hex::encode(sha2::Sha256::digest(serialized)));
        if plan_fingerprint.len() != 66 || action_fingerprint.len() != 66 {
            return Err(invalid_response(
                "Core deposit recovery fingerprint is malformed.",
            ));
        }
        Ok((
            parent,
            plan,
            leg,
            nonce,
            plan_fingerprint,
            action_fingerprint,
        ))
    }

    fn core_nonce_recovery_binding_key(child_operation_id: &str) -> String {
        format!("papertrade-core-nonce-recovery:{child_operation_id}")
    }

    fn core_nonce_recovery_ack_key(child_operation_id: &str) -> String {
        format!("papertrade-core-nonce-recovery-ack:{child_operation_id}")
    }
}

fn funding_record_key(operation_id: &str) -> String {
    format!("papertrade-funding-{operation_id}")
}

fn funding_continuation_record_key(operation_id: &str) -> String {
    use sha2::Digest;
    format!(
        "papertrade-funding-continuation-{}",
        hex::encode(sha2::Sha256::digest(operation_id.as_bytes()))
    )
}

fn registration_dispatch_block_key(registration_id: &str) -> String {
    use sha2::Digest;
    format!(
        "papertrade-registration-dispatch-{}",
        hex::encode(sha2::Sha256::digest(registration_id.as_bytes()))
    )
}

fn parent_submission_matches_funding(
    submission: &gofin_domain::Submission,
    route_id: &str,
) -> bool {
    submission.kind == "papertrade_funding"
        && submission.reference == route_id
        && submission.data.get("operation_id").and_then(Value::as_str) == Some(route_id)
}

fn recovery_evidence_id(
    binding: &CoreDepositNonceRecoveryBinding,
    child_submission: &gofin_domain::Submission,
    action_fingerprint: &str,
) -> Result<String> {
    let encoded = serde_json::to_vec(&(binding, child_submission, action_fingerprint))
        .map_err(|_| unavailable("Noop recovery evidence could not be encoded."))?;
    use sha2::Digest;
    Ok(format!("0x{}", hex::encode(sha2::Sha256::digest(encoded))))
}

fn underlying_hyperevm_usdc() -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: "ethereum:999".into(),
        reference: "0xb88339cb7199b77e23db6e890353e22632ba630f".into(),
        decimals: 6,
    }
}

fn close_margin_wad(snapshot: &Value, owner: &str, requested_ids: &[String]) -> Result<u128> {
    if snapshot
        .get("owner")
        .and_then(Value::as_str)
        .is_none_or(|observed| !observed.eq_ignore_ascii_case(owner))
        || snapshot
            .get("indexedThroughBlock")
            .and_then(|value| value.as_str()?.parse::<u64>().ok())
            .is_none()
    {
        return Err(GoFinError::validation(
            "Papertrade close valuation lacks the exact owner or indexed position snapshot.",
        ));
    }
    let rows = snapshot
        .get("positions")
        .and_then(Value::as_array)
        .ok_or_else(|| GoFinError::validation("Papertrade positions snapshot omitted its rows."))?;
    let mut total_margin = BigUint::from(0u8);
    for position_id in requested_ids {
        let matches: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.get("positionId").and_then(Value::as_str) == Some(position_id.as_str())
            })
            .collect();
        if matches.len() != 1 {
            return Err(GoFinError::validation(
                "A selected Papertrade position is missing or duplicated in the valuation snapshot.",
            ));
        }
        let margin = matches[0]
            .get("marginRaw")
            .and_then(Value::as_str)
            .and_then(|raw| raw.parse::<BigUint>().ok())
            .ok_or_else(|| {
                GoFinError::validation("A selected Papertrade position has malformed exact margin.")
            })?;
        total_margin += margin;
    }
    total_margin.to_u128().ok_or_else(|| {
        GoFinError::validation(
            "Selected Papertrade close margin exceeds the supported exact range.",
        )
    })
}

fn lookup_cancel_target_action(snapshot: &Value, target: &str) -> Result<String> {
    let operations = snapshot
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("Papertrade wallet snapshot omitted queued operations."))?;
    for row in operations {
        let Some(fields) = row.as_array() else {
            continue;
        };
        if fields
            .first()
            .and_then(Value::as_str)
            .is_some_and(|id| id.eq_ignore_ascii_case(target))
        {
            if fields.len() != 10 {
                return Err(invalid_response(
                    "Papertrade queued operation tuple has the wrong length.",
                ));
            }
            let action = fields.get(1).and_then(Value::as_str).ok_or_else(|| {
                invalid_response("Papertrade queued operation omitted its action.")
            })?;
            if matches!(
                action,
                "registerSessionKey"
                    | "openPosition"
                    | "closePositions"
                    | "withdrawToCore"
                    | "paperStake"
                    | "paperUnstake"
                    | "paperClaim"
            ) {
                return Ok(action.to_owned());
            }
        }
    }
    Err(GoFinError::new(
        "PAPERTRADE_CANCEL_TARGET_UNKNOWN",
        "preview",
        "The exact cancel target is not present in the owner's indexed intent outcomes.",
        "Refresh the Papertrade account snapshot and retry with the exact pending intent ID.",
    ))
}

fn typed_owner(typed: &PapertradeTypedIntent) -> &str {
    match typed {
        PapertradeTypedIntent::RegisterSessionKey { user, .. }
        | PapertradeTypedIntent::OpenPosition { user, .. }
        | PapertradeTypedIntent::Close { user, .. }
        | PapertradeTypedIntent::WithdrawToCore { user, .. }
        | PapertradeTypedIntent::Stake { user, .. }
        | PapertradeTypedIntent::Unstake { user, .. }
        | PapertradeTypedIntent::Claim { user, .. }
        | PapertradeTypedIntent::CancelIntent { user, .. } => user,
    }
}

fn intent_deadline(typed: &PapertradeTypedIntent) -> u64 {
    match typed {
        PapertradeTypedIntent::RegisterSessionKey { deadline, .. }
        | PapertradeTypedIntent::OpenPosition { deadline, .. }
        | PapertradeTypedIntent::Close { deadline, .. }
        | PapertradeTypedIntent::WithdrawToCore { deadline, .. }
        | PapertradeTypedIntent::Stake { deadline, .. }
        | PapertradeTypedIntent::Unstake { deadline, .. }
        | PapertradeTypedIntent::Claim { deadline, .. } => *deadline,
        PapertradeTypedIntent::CancelIntent { deadline, .. } => deadline.parse().unwrap_or(0),
    }
}

fn session_deadline(now: u64, expiry: u64, maximum_ttl: u64) -> Result<u64> {
    let latest_safe_deadline = expiry
        .checked_sub(SESSION_DEADLINE_SAFETY_SECONDS)
        .ok_or_else(|| {
            GoFinError::validation(
                "Session registration expiry is too short for a safe signed deadline.",
            )
        })?;
    let deadline = now
        .checked_add(maximum_ttl)
        .ok_or_else(|| GoFinError::validation("Papertrade deadline overflowed."))?
        .min(latest_safe_deadline);
    if deadline <= now {
        return Err(GoFinError::validation(
            "Session registration expiry is too short for a safe signed deadline.",
        ));
    }
    Ok(deadline)
}

fn session_registration_deadline(now: u64, expiry: u64) -> Result<u64> {
    session_deadline(now, expiry, SIGNED_INTENT_TTL_SECONDS)
}

fn funding_withdrawal_deadline(now: u64) -> Result<u64> {
    signed_intent_deadline(now)
}

fn signed_intent_deadline(now: u64) -> Result<u64> {
    now.checked_add(SIGNED_INTENT_TTL_SECONDS)
        .ok_or_else(|| GoFinError::validation("Papertrade deadline overflowed."))
}

fn session_intent_deadline(now: u64, expiry: u64) -> Result<u64> {
    session_deadline(now, expiry, SIGNED_INTENT_TTL_SECONDS)
}

fn cancel_record_key(owner: &str, target: &str) -> String {
    let mut hash = sha2::Sha256::new();
    use sha2::Digest;
    hash.update(owner.to_ascii_lowercase().as_bytes());
    hash.update([0]);
    hash.update(target.to_ascii_lowercase().as_bytes());
    format!("papertrade-cancel-{}", hex::encode(hash.finalize()))
}

fn cancel_proof_from_receipt(
    owner: &str,
    target: &str,
    action: &str,
    state: &str,
) -> Result<gofin_papertrade::CancelStatusProof> {
    use gofin_papertrade::{CancelIntentState as S, CancelStatusProof, CancelStatusSource};
    let state = match state {
        "cancelled" => S::Cancelled,
        "succeeded" => S::Succeeded,
        "failed" => S::Failed,
        "expired" => S::Expired,
        "pending" => S::Pending,
        "unknown" => S::Unknown,
        _ => {
            return Err(invalid_response(
                "Papertrade cancellation returned an unknown target state.",
            ));
        }
    };
    Ok(CancelStatusProof {
        owner: owner.to_ascii_lowercase(),
        target_intent_id: target.to_ascii_lowercase(),
        action: action.to_owned(),
        state,
        source: CancelStatusSource::CancelEndpoint,
        account_revision: None,
        indexed_through_block: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_deadline_is_short_lived_and_inside_session_expiry() {
        let preview_now = 1_800_000_000;
        let requested_expiry = preview_now + 3_600;
        let execution_now = preview_now + 1;
        let deadline = session_registration_deadline(execution_now, requested_expiry).unwrap();
        assert!(deadline > execution_now);
        assert!(deadline <= requested_expiry - SESSION_DEADLINE_SAFETY_SECONDS);
        assert_eq!(deadline, execution_now + SIGNED_INTENT_TTL_SECONDS);
        // A requested one-hour session remains distinct from the five-minute
        // deadline used by every newly signed Papertrade operation.
        assert_eq!(
            session_intent_deadline(execution_now, requested_expiry).unwrap(),
            execution_now + SIGNED_INTENT_TTL_SECONDS
        );

        let short_session_expiry = execution_now + 240;
        assert_eq!(
            session_registration_deadline(execution_now, short_session_expiry).unwrap(),
            short_session_expiry - SESSION_DEADLINE_SAFETY_SECONDS
        );

        assert!(session_registration_deadline(execution_now, execution_now + 20).is_err());
    }

    #[test]
    fn core_withdrawal_deadline_is_short_lived() {
        let now = 1_800_000_000;
        assert_eq!(funding_withdrawal_deadline(now).unwrap(), 1_800_000_300);
        assert!(funding_withdrawal_deadline(u64::MAX).is_err());
    }

    #[test]
    fn typed_api_uint_fields_are_decimal_strings() {
        let register = typed_api_fields(&PapertradeTypedIntent::RegisterSessionKey {
            user: "0x1111111111111111111111111111111111111111".into(),
            key: "0x2222222222222222222222222222222222222222".into(),
            expiry: 1_792_191_349,
            fee_address: "0x0000000000000000000000000000000000000000".into(),
            nonce: "7".into(),
            deadline: 1_792_191_319,
        })
        .unwrap();
        assert_eq!(register["expiry"], "1792191349");
        assert_eq!(register["deadline"], "1792191319");
        assert_eq!(register["nonce"], "7");

        let open = typed_api_fields(&PapertradeTypedIntent::OpenPosition {
            user: "0x1111111111111111111111111111111111111111".into(),
            instrument_id: 0,
            is_long: true,
            size: 12_345_678_901_234_567_890,
            leverage: 5,
            use_debt: false,
            nonce: "8".into(),
            deadline: 1_792_191_319,
        })
        .unwrap();
        assert_eq!(open["instrumentId"], "0");
        assert_eq!(open["size"], "12345678901234567890");
        assert_eq!(open["leverage"], "5");
        assert_eq!(open["deadline"], "1792191319");
        assert_eq!(open["isLong"], true);
        assert_eq!(open["useDebt"], false);

        let close = typed_api_fields(&PapertradeTypedIntent::Close {
            user: "0x1111111111111111111111111111111111111111".into(),
            position_ids: vec!["9".into()],
            nonce: "10".into(),
            deadline: 1_792_191_319,
        })
        .unwrap();
        assert_eq!(close["positionIds"][0], "9");
        assert_eq!(close["deadline"], "1792191319");
    }
    use crate::BrokerCredentialProvider;
    use std::collections::HashMap;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn staking_normalization_uses_exact_integer_formula() {
        let snapshot =
            json!({"paper":{"balance":"9","staked":"3","pendingBase":"5","rewardDebt":"2"}});
        let trading = json!({"accRewardPerShareRaw":"1000000000000000000000000000"});
        let value = normalize_staking(&snapshot, &trading, "3").unwrap();
        assert_eq!(value["claimableRewardsRaw"], "6");
        assert_eq!(value["paper"]["balance"], "9");
        assert_eq!(value["rewardReserveRaw"], "3");
    }

    #[test]
    fn wallet_events_are_bound_to_exact_owner() {
        let owner = "0x1111111111111111111111111111111111111111";
        let event = format!("event: wallet\ndata: {{\"wallet\":\"{owner}\"}}\n\n");
        assert!(
            parse_wallet_event(event.as_bytes(), owner)
                .unwrap()
                .is_some()
        );
        assert!(
            parse_wallet_event(
                event.as_bytes(),
                "0x2222222222222222222222222222222222222222"
            )
            .is_err()
        );
    }

    #[test]
    fn position_tuples_are_normalized_without_losing_exact_amounts() {
        let owner = "0x1111111111111111111111111111111111111111";
        let snapshot = json!({"indexedThroughBlock":"0x2a"});
        let positions = json!([[
            "7",
            0,
            true,
            false,
            "123456789012345678901",
            "2000000000000000000",
            5,
            "1",
            1700000000000u64,
            format!("0x{}", "a".repeat(64))
        ]]);
        let value = normalize_positions(&snapshot, owner, &positions).unwrap();
        assert_eq!(value["owner"], owner);
        assert_eq!(value["indexedThroughBlock"], "42");
        assert_eq!(value["positions"][0]["positionId"], "7");
        assert_eq!(
            value["positions"][0]["entryPriceRaw"],
            "123456789012345678901"
        );
        assert_eq!(value["positions"][0]["marginRaw"], "2000000000000000000");
    }

    #[test]
    fn legacy_position_without_intent_id_does_not_hide_entire_snapshot() {
        let owner = "0x1111111111111111111111111111111111111111";
        let snapshot = json!({"indexedThroughBlock":"0x2a"});
        let positions = json!([[
            "7",
            0,
            true,
            false,
            "100",
            "2000000000000000000",
            5,
            "1",
            1700000000000u64,
            null
        ]]);
        let value = normalize_positions(&snapshot, owner, &positions).unwrap();
        assert_eq!(value["positions"].as_array().unwrap().len(), 1);
        assert!(value["positions"][0]["intentId"].is_null());
    }

    #[test]
    fn close_valuation_uses_exact_selected_owner_margin_snapshot() {
        let owner = "0x1111111111111111111111111111111111111111";
        let snapshot = json!({
            "owner":owner,
            "indexedThroughBlock":"48718516",
            "positions":[
                {"positionId":"7","marginRaw":"10000000000000000000"},
                {"positionId":"8","marginRaw":"20000000000000000000"},
                {"positionId":"9","marginRaw":"99000000000000000000"}
            ]
        });
        assert_eq!(
            close_margin_wad(&snapshot, owner, &["7".into(), "8".into()]).unwrap(),
            30_000_000_000_000_000_000
        );
        assert!(close_margin_wad(&snapshot, owner, &["missing".into()]).is_err());
        assert!(
            close_margin_wad(
                &snapshot,
                "0x2222222222222222222222222222222222222222",
                &["7".into()]
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_staking_data_fails_closed() {
        let snapshot =
            json!({"paper":{"balance":"0","staked":"3","pendingBase":"0","rewardDebt":"8"}});
        let trading = json!({"accRewardPerShareRaw":"0"});
        assert!(normalize_staking(&snapshot, &trading, "0").is_err());
    }

    #[test]
    fn intake_controls_must_be_well_formed_and_gate_exact_action() {
        let trading = json!({"intakeControls":["4","1000",["paperStake"]]});
        assert!(action_disabled(&trading, "paperStake").unwrap());
        assert!(!action_disabled(&trading, "openPosition").unwrap());
        assert!(action_disabled(&json!({"intakeControls":[0,1,[null]]}), "paperStake").is_err());
        assert!(action_disabled(&json!({}), "paperStake").is_err());
    }

    #[test]
    fn build_identity_is_parsed_from_the_exported_main_asset_without_evaluation() {
        let html = r#"<script type="module" src="/assets/build-7BCG1_XT.js"></script>"#;
        let asset = parse_build_asset(html).unwrap();
        assert_eq!(asset, "/assets/build-7BCG1_XT.js");
        assert_eq!(
            parse_build_id("var e=`85359652ed61`;export{e as n}").unwrap(),
            "85359652ed61"
        );
        assert_eq!(
            parse_build_id("var e=`f76cd7d6a153`,t=1;export{r as i,e as n,n as r,t}").unwrap(),
            "f76cd7d6a153"
        );
        assert!(parse_build_id("var e=`85359652ed61`;export{other as n,e as x}").is_err());
        assert!(parse_build_asset("<script src=\"https://other.invalid/build-x.js\">").is_err());
        assert!(parse_build_id("var e=`not-a-build`;export{e as n}").is_err());
    }

    #[test]
    fn cancel_response_binds_target_identity_and_explicit_terminal_state() {
        let target = format!("0x{}", "1".repeat(64));
        let signed = format!("0x{}", "2".repeat(64));
        let receipt = validate_cancel_response(
            json!({"intentId":target,"action":"openPosition","state":"cancelled"}),
            &target,
            "openPosition",
            &signed,
        )
        .unwrap();
        assert_eq!(receipt.status, "cancelled");
        assert_eq!(receipt.intent_id, signed);
        assert!(
            validate_cancel_response(
                json!({"intentId":signed,"action":"openPosition","state":"cancelled"}),
                &target,
                "openPosition",
                &signed,
            )
            .is_err()
        );
        assert!(
            validate_cancel_response(
                json!({"intentId":target,"action":"paperStake","state":"cancelled"}),
                &target,
                "openPosition",
                &signed,
            )
            .is_err()
        );
    }

    #[derive(Default)]
    struct RuntimeFixtureCredentials {
        records: Mutex<HashMap<String, Value>>,
        sessions: Mutex<Vec<gofin_credentials::PapertradeSessionRegistration>>,
        response_body: Mutex<Option<Value>>,
        response_intent_id: Mutex<Option<String>>,
        origin_header: Mutex<Option<String>>,
    }

    impl gofin_exchange::ccxt::CredentialProvider for RuntimeFixtureCredentials {
        fn credentials(
            &self,
            _reference: &str,
        ) -> std::result::Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError>
        {
            Err(gofin_exchange::ccxt::CcxtError::new(
                gofin_exchange::ccxt::CcxtErrorKind::Unsupported,
            ))
        }
    }

    impl crate::BrokerCredentialProvider for RuntimeFixtureCredentials {
        fn envelope_signer(
            &self,
            credential: &gofin_credentials::CredentialRef,
        ) -> Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
            if credential.as_str() != "00000000-0000-4000-8000-000000000001" {
                return Err(GoFinError::validation("fixture owner credential mismatch"));
            }
            let signer =
                gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1))?;
            Ok((format!("{:#x}", signer.address()), Arc::new(signer)))
        }

        fn papertrade_wallet_signer(
            &self,
            _credential: &gofin_credentials::CredentialRef,
            owner: &str,
        ) -> Result<Arc<gofin_evm::LocalEnvelopeSigner>> {
            let signer = Arc::new(gofin_evm::LocalEnvelopeSigner::from_private_key(&format!(
                "0x{:064x}",
                1
            ))?);
            if !format!("{:#x}", signer.address()).eq_ignore_ascii_case(owner) {
                return Err(GoFinError::validation("fixture owner does not match key"));
            }
            Ok(signer)
        }

        fn papertrade_session_signer(
            &self,
            _registration_id: &str,
            _owner: &str,
            _expires_at: u64,
        ) -> Result<(
            gofin_credentials::CredentialRef,
            String,
            Arc<gofin_evm::LocalEnvelopeSigner>,
        )> {
            let signer = Arc::new(gofin_evm::LocalEnvelopeSigner::from_private_key(&format!(
                "0x{:064x}",
                2
            ))?);
            Ok((
                gofin_credentials::CredentialRef::new("00000000-0000-4000-8000-000000000002")?,
                format!("{:#x}", signer.address()).to_ascii_lowercase(),
                signer,
            ))
        }

        fn papertrade_sessions(
            &self,
            owner: &str,
        ) -> Result<Vec<gofin_credentials::PapertradeSessionRegistration>> {
            Ok(self
                .sessions
                .lock()
                .map_err(|_| GoFinError::validation("fixture lock poisoned"))?
                .iter()
                .filter(|session| session.owner_address.eq_ignore_ascii_case(owner))
                .cloned()
                .collect())
        }

        fn papertrade_runtime_record(&self, key: &str) -> Result<Option<Value>> {
            Ok(self
                .records
                .lock()
                .map_err(|_| GoFinError::validation("fixture lock poisoned"))?
                .get(key)
                .cloned())
        }

        fn put_papertrade_runtime_record(&self, key: &str, value: &Value) -> Result<()> {
            self.records
                .lock()
                .map_err(|_| GoFinError::validation("fixture lock poisoned"))?
                .insert(key.to_owned(), value.clone());
            Ok(())
        }

        fn activate_papertrade_session(
            &self,
            registration_id: &str,
            owner: &str,
            expiry: u64,
            registered_at: u64,
        ) -> Result<()> {
            let record = self
                .records
                .lock()
                .map_err(|_| GoFinError::validation("fixture lock poisoned"))?
                .get(&format!("papertrade-registration-{registration_id}"))
                .cloned()
                .ok_or_else(|| GoFinError::validation("fixture registration record missing"))?;
            let credential_ref: gofin_credentials::CredentialRef = serde_json::from_value(
                record
                    .get("credential_ref")
                    .cloned()
                    .unwrap_or_else(|| json!("00000000-0000-4000-8000-000000000002")),
            )
            .map_err(|_| GoFinError::validation("fixture credential reference malformed"))?;
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| GoFinError::validation("fixture lock poisoned"))?;
            sessions.push(gofin_credentials::PapertradeSessionRegistration {
                owner_address: owner.to_ascii_lowercase(),
                registration_id: registration_id.to_owned(),
                expires_at: expiry,
                credential_ref,
                registered_at,
                revoked_at: None,
            });
            Ok(())
        }
    }

    #[derive(Default)]
    struct RuntimeFixtureExecutionState(Mutex<HashMap<String, u64>>);

    impl gofin_domain::execution_state::ExecutionState for RuntimeFixtureExecutionState {
        fn allocate_nonce(&self, scope: &str, seed: u64) -> Result<u64> {
            let mut lanes = self
                .0
                .lock()
                .map_err(|_| GoFinError::validation("fixture nonce lock poisoned"))?;
            let next = lanes
                .get(scope)
                .copied()
                .map_or(seed, |previous| previous.saturating_add(1));
            lanes.insert(scope.to_owned(), next);
            Ok(next)
        }

        fn operation(&self, _operation_id: &str) -> Result<Option<gofin_domain::OperationRecord>> {
            Ok(None)
        }
    }

    #[derive(Default)]
    struct RuntimeFixtureRpc {
        chain: Mutex<Option<RuntimeFixtureChain>>,
        expiry: Mutex<Option<RuntimeFixtureExpiry>>,
        session_queries: Mutex<Vec<(String, String)>>,
        log_windows: Mutex<Vec<(u64, u64)>>,
    }

    #[derive(Clone)]
    struct RuntimeFixtureChain {
        intent_id: String,
        tx_input: String,
        tx_hash: String,
        block_hash: String,
        logs: Value,
    }

    #[derive(Clone)]
    struct RuntimeFixtureExpiry {
        block_hash: String,
        timestamp: u64,
        head_number: u64,
        timestamp_per_block: u64,
        implementation: String,
        exchange_implementation: Option<String>,
        staking_implementation: Option<String>,
        batch_implementation: Option<String>,
        exchange_code_invalid: bool,
        staking_code_invalid: bool,
        batch_code_invalid: bool,
        code: String,
        nonce_used: bool,
        session_words: String,
        intent_id: Option<String>,
        intent_present: bool,
        changed_hash_on_reread: bool,
        changed_hash_on_final_reread: bool,
        block_reads: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for RuntimeFixtureRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            let fixture = self
                .chain
                .lock()
                .map_err(|_| GoFinError::validation("fixture RPC lock poisoned"))?
                .clone();
            let block_hash = fixture
                .as_ref()
                .map(|chain| chain.block_hash.clone())
                .unwrap_or_else(|| format!("0x{}", "b".repeat(64)));
            let tx_hash = fixture
                .as_ref()
                .map(|chain| chain.tx_hash.clone())
                .unwrap_or_else(|| format!("0x{}", "a".repeat(64)));
            let expiry = self
                .expiry
                .lock()
                .map_err(|_| GoFinError::validation("fixture expiry lock poisoned"))?
                .clone();
            let head_number = expiry.as_ref().map_or(10, |value| value.head_number);
            let tx_input = fixture
                .as_ref()
                .map(|chain| chain.tx_input.clone())
                .unwrap_or_else(|| "0x".into());
            let logs = fixture
                .as_ref()
                .map(|chain| chain.logs.clone())
                .unwrap_or(json!([]));
            match method {
                "eth_chainId" => Ok(json!("0x3e7")),
                "eth_blockNumber" => Ok(json!(format!("0x{head_number:x}"))),
                "eth_getLogs" => {
                    let from_block = params[0]["fromBlock"]
                        .as_str()
                        .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
                        .ok_or_else(|| GoFinError::validation("fixture log start is malformed"))?;
                    let to_block = params[0]["toBlock"]
                        .as_str()
                        .and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok())
                        .ok_or_else(|| GoFinError::validation("fixture log end is malformed"))?;
                    self.log_windows
                        .lock()
                        .map_err(|_| GoFinError::validation("fixture log lock poisoned"))?
                        .push((from_block, to_block));
                    let requested = params[0]["topics"][3].as_str().unwrap_or_default();
                    if fixture
                        .as_ref()
                        .is_some_and(|chain| requested.eq_ignore_ascii_case(&chain.intent_id))
                    {
                        let call_log = logs
                            .as_array()
                            .and_then(|rows| rows.last())
                            .cloned()
                            .ok_or_else(|| GoFinError::validation("fixture call log missing"))?;
                        Ok(json!([{
                            "address": gofin_papertrade::BATCH_EXECUTOR,
                            "topics": call_log["topics"],
                            "data": call_log["data"],
                            "transactionHash": tx_hash,
                        }]))
                    } else if expiry.as_ref().is_some_and(|value| {
                        value.intent_present
                            && value
                                .intent_id
                                .as_deref()
                                .is_some_and(|intent_id| requested.eq_ignore_ascii_case(intent_id))
                    }) {
                        Ok(json!([{
                            "address": BATCH_EXECUTOR,
                            "topics": [CALL_EXECUTED_TOPIC, format!("0x{}", "0".repeat(64)), format!("0x{}", "0".repeat(64)), requested],
                            "data": "0x",
                            "transactionHash": tx_hash,
                        }]))
                    } else {
                        Ok(json!([]))
                    }
                }
                "eth_getTransactionReceipt" => Ok(json!({
                    "transactionHash":tx_hash,
                    "blockNumber":"0x5",
                    "blockHash":block_hash,
                    "status":"0x1",
                    "gasUsed":"0x0",
                    "effectiveGasPrice":"0x0",
                    "logs":logs,
                })),
                "eth_getTransactionByHash" => Ok(json!({
                    "hash":tx_hash,
                    "from":"0x7e5f4552091a69125d5dfcb7b8c2659029395bdf",
                    "to":gofin_papertrade::BATCH_EXECUTOR,
                    "input":tx_input,
                    "blockNumber":"0x5",
                    "blockHash":block_hash,
                })),
                "eth_getBlockByNumber" => Ok(json!({
                    "number": format!("0x{:x}", if params[0] == "finalized" { head_number } else { params[0].as_str().and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok()).unwrap_or(5) }),
                    "hash": if let Some(expiry) = &expiry {
                        let read_index = expiry
                            .block_reads
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if expiry.changed_hash_on_reread
                            && ((!expiry.changed_hash_on_final_reread && read_index == 1)
                                || (expiry.changed_hash_on_final_reread && read_index > 1))
                        {
                            format!("0x{}", "e".repeat(64))
                        } else {
                            expiry.block_hash.clone()
                        }
                    } else if params[0] == "finalized" {
                        format!("0x{}", "c".repeat(64))
                    } else {
                        block_hash
                    },
                    "timestamp": format!("0x{:x}", expiry.as_ref().map_or(0x65000000, |value| value.timestamp.saturating_add(value.timestamp_per_block.saturating_mul(if params[0] == "finalized" { head_number } else { params[0].as_str().and_then(|raw| u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok()).unwrap_or(5) })))),
                })),
                "eth_getStorageAt" => {
                    let contract = params[0].as_str().unwrap_or_default();
                    let implementation = if contract
                        .eq_ignore_ascii_case(gofin_papertrade::SESSION_KEY_MANAGER)
                    {
                        expiry
                            .as_ref()
                            .map(|value| value.implementation.trim_start_matches("0x").to_owned())
                            .unwrap_or_else(|| {
                                "8a5069fe303e8f81e91825436e956d595032e558".to_owned()
                            })
                    } else {
                        match contract.to_ascii_lowercase().as_str() {
                            value if value.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE) => {
                                expiry
                                    .as_ref()
                                    .and_then(|value| value.exchange_implementation.clone())
                                    .unwrap_or_else(|| {
                                        "0f3febfc94876b0c895f7acecdc8f5c863b549ee".to_owned()
                                    })
                            }
                            value
                                if value.eq_ignore_ascii_case(gofin_papertrade::PAPER_STAKING) =>
                            {
                                expiry
                                    .as_ref()
                                    .and_then(|value| value.staking_implementation.clone())
                                    .unwrap_or_else(|| {
                                        "e6e3519455170913c5c3bfe541dd918a2d0cbb15".to_owned()
                                    })
                            }
                            value
                                if value.eq_ignore_ascii_case(gofin_papertrade::BATCH_EXECUTOR) =>
                            {
                                expiry
                                    .as_ref()
                                    .and_then(|value| value.batch_implementation.clone())
                                    .unwrap_or_else(|| {
                                        "e0bc9cc7b067060aafccbaa3e250700c9cdd7262".to_owned()
                                    })
                            }
                            _ => "e0bc9cc7b067060aafccbaa3e250700c9cdd7262".to_owned(),
                        }
                    };
                    Ok(json!(format!("0x{}{}", "0".repeat(24), implementation)))
                }
                "eth_getCode" => {
                    let address = params[0].as_str().unwrap_or_default();
                    let code = if address
                        .eq_ignore_ascii_case(gofin_papertrade::EXCHANGE_IMPLEMENTATION)
                    {
                        if expiry
                            .as_ref()
                            .is_some_and(|value| value.exchange_code_invalid)
                        {
                            "0x6000".to_owned()
                        } else {
                            include_str!("testdata/exchange-implementation.hex")
                                .trim()
                                .to_owned()
                        }
                    } else {
                        let pinned = if address
                            .eq_ignore_ascii_case(gofin_papertrade::PAPER_STAKING_IMPLEMENTATION)
                        {
                            Some((
                                expiry
                                    .as_ref()
                                    .is_some_and(|value| value.staking_code_invalid),
                                include_str!("testdata/paperstaking-implementation.hex"),
                            ))
                        } else if address
                            .eq_ignore_ascii_case(gofin_papertrade::BATCH_EXECUTOR_IMPLEMENTATION)
                        {
                            Some((
                                expiry
                                    .as_ref()
                                    .is_some_and(|value| value.batch_code_invalid),
                                include_str!("testdata/batch-executor-implementation.hex"),
                            ))
                        } else {
                            None
                        };
                        if let Some((invalid, code)) = pinned {
                            if invalid {
                                "0x6000".to_owned()
                            } else {
                                code.trim().to_owned()
                            }
                        } else {
                            expiry
                                .as_ref()
                                .map_or_else(|| "0x6000".to_owned(), |value| value.code.clone())
                        }
                    };
                    Ok(json!(code))
                }
                "eth_call" => {
                    let call = params[0]["data"].as_str().unwrap_or_default();
                    if call.starts_with("0x96ade1f9") {
                        let bytes = hex::decode(call.trim_start_matches("0x")).map_err(|_| {
                            GoFinError::validation("fixture session call malformed")
                        })?;
                        if bytes.len() != 68 {
                            return Err(GoFinError::validation(
                                "fixture session call has wrong calldata length",
                            ));
                        }
                        let owner = format!("0x{}", hex::encode(&bytes[16..36]));
                        let session_key = format!("0x{}", hex::encode(&bytes[48..68]));
                        self.session_queries
                            .lock()
                            .map_err(|_| GoFinError::validation("fixture query lock poisoned"))?
                            .push((owner, session_key));
                        Ok(json!(expiry.as_ref().map_or_else(
                            || format!("0x{}", "0".repeat(128)),
                            |value| value.session_words.clone()
                        )))
                    } else if call.starts_with("0xcab7e8eb") {
                        let used = expiry.as_ref().is_some_and(|value| value.nonce_used);
                        Ok(json!(format!("0x{}{}", "0".repeat(63), u8::from(used))))
                    } else {
                        Ok(json!(format!("0x{}", "0".repeat(64))))
                    }
                }
                _ => Err(GoFinError::validation("unexpected fixture RPC method")),
            }
        }
    }

    struct RuntimeFixtureFunding;

    #[async_trait::async_trait]
    impl crate::papertrade_funding_host::PapertradeFundingServices for RuntimeFixtureFunding {
        async fn preflight(
            &self,
            _: &FundingIntent,
        ) -> Result<crate::papertrade_funding::FundingPreflight> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn check_proxy_registration(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<crate::papertrade_funding::ProxyRegistrationProof> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn cctp_quote(
            &self,
            _: &crate::papertrade_funding::CctpQuoteRequest,
        ) -> Result<crate::papertrade_funding::FundingCctpQuote> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn prepare_evm_call(
            &self,
            _: &str,
            _: &FundingIntent,
            _: &gofin_bridge::EvmCall,
            _: &str,
        ) -> Result<gofin_evm::FrozenTransaction> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn solana_burn_request(
            &self,
            _: &str,
            _: &FundingIntent,
            _: &gofin_bridge::cctp::CctpQuoteMetadata,
            _: &str,
        ) -> Result<gofin_bridge::cctp::solana::hypercore_source::BurnRequest> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn reserve_hyperliquid_nonce(&self, _: &str) -> Result<u64> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn papertrade_withdraw_to_core_intent(
            &self,
            _: &str,
            _: &FundingIntent,
        ) -> Result<BoundIntent> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn persist_plan(&self, _: &crate::papertrade_funding::BoundFunding) -> Result<()> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn assert_persisted_plan(
            &self,
            _: &crate::papertrade_funding::BoundFunding,
        ) -> Result<()> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn dispatch_persisted_leg(
            &self,
            _: &crate::papertrade_funding::FundingLeg,
            _: &gofin_domain::Submission,
            _: Arc<dyn gofin_domain::SubmissionJournal>,
            _: &gofin_domain::RevalidatedOperation,
        ) -> Result<crate::papertrade_funding::FundingLegState> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn reconcile_persisted_leg(
            &self,
            _: &crate::papertrade_funding::BoundFunding,
            _: &crate::papertrade_funding::FundingLeg,
        ) -> Result<crate::papertrade_funding::FundingLegState> {
            Err(GoFinError::validation("unused funding fixture"))
        }
        async fn prove_destination(
            &self,
            _: &crate::papertrade_funding::BoundFunding,
            _: &[crate::papertrade_funding::FundingLegState],
        ) -> Result<Option<gofin_papertrade::FundingReceipt>> {
            Err(GoFinError::validation("unused funding fixture"))
        }
    }

    fn push_fixture_word(output: &mut Vec<u8>, value: u64) {
        output.extend_from_slice(&[0u8; 24]);
        output.extend_from_slice(&value.to_be_bytes());
    }

    fn fixture_address_word(value: &str) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend(hex::decode(value.trim_start_matches("0x")).unwrap());
        word
    }

    fn fixture_batch_input(target: &str, call: &[u8], intent_id: &str) -> String {
        let mut output = hex::decode("251fb27d").unwrap();
        push_fixture_word(&mut output, 32);
        push_fixture_word(&mut output, 1);
        push_fixture_word(&mut output, 32);
        output.extend(fixture_address_word(target));
        push_fixture_word(&mut output, 160);
        push_fixture_word(&mut output, 1);
        push_fixture_word(&mut output, 0);
        output.extend(hex::decode(intent_id.trim_start_matches("0x")).unwrap());
        push_fixture_word(&mut output, call.len() as u64);
        output.extend_from_slice(call);
        while output.len() % 32 != 4 {
            output.push(0);
        }
        format!("0x{}", hex::encode(output))
    }

    async fn serve_fixture_request(
        stream: tokio::net::TcpStream,
        credentials: Arc<RuntimeFixtureCredentials>,
    ) {
        let mut stream = BufReader::new(stream);
        let mut first = String::new();
        stream.read_line(&mut first).await.unwrap();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("origin")
            {
                *credentials.origin_header.lock().unwrap() = Some(value.trim().to_owned());
            }
        }
        let mut body = vec![0; content_length];
        if content_length > 0 {
            stream.read_exact(&mut body).await.unwrap();
        }
        let path = first.split_whitespace().nth(1).unwrap_or_default();
        let response = if first.starts_with("GET /trade/btc ") {
            (
                200,
                "text/html",
                r#"<script type="module" src="/assets/build-fixture.js"></script>"#.to_owned(),
            )
        } else if first.starts_with("GET /assets/build-fixture.js ") {
            (
                200,
                "application/javascript",
                "var e=`85359652ed61`;export{e as n}".to_owned(),
            )
        } else if first.starts_with("GET /state/trading ") {
            (
                200,
                "application/json",
                json!({"intakeControls":["fixture-revision","1000000000000000000000",[]]})
                    .to_string(),
            )
        } else if first.starts_with("POST /intents ") && path == "/intents" {
            let submitted: Value = serde_json::from_slice(&body).unwrap();
            *credentials.response_body.lock().unwrap() = Some(submitted);
            if credentials
                .response_body
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|value| value.get("action"))
                == Some(&json!("withdrawToCore"))
            {
                let request = credentials.response_body.lock().unwrap().clone().unwrap();
                let intent_id = credentials
                    .response_intent_id
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("fixture withdrawal response id");
                let has_route_metadata =
                    request.get("withdrawal") == Some(&json!({"route":"hypercore"}));
                if has_route_metadata {
                    (
                        200,
                        "application/json",
                        json!({"action":"withdrawToCore","intentId":intent_id,"queueRevision":1,"state":"queued"}).to_string(),
                    )
                } else {
                    (
                        422,
                        "application/json",
                        json!({"code":"INVALID_WITHDRAWAL_ROUTE"}).to_string(),
                    )
                }
            } else if credentials
                .response_body
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|value| value.get("action"))
                == Some(&json!("paperClaim"))
            {
                let intent_id = credentials
                    .response_intent_id
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("fixture claim intent id");
                (
                    200,
                    "application/json",
                    json!({"action":"paperClaim","intentId":intent_id,"queueRevision":1,"state":"queued"}).to_string(),
                )
            } else {
                let record = credentials
                    .records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(key, _)| key.as_str() == "papertrade-registration-fixture-registration")
                    .map(|(_, value)| value.clone())
                    .unwrap();
                let bound: BoundIntent =
                    serde_json::from_value(record["bound_intent"].clone()).unwrap();
                (200, "application/json", json!({"action":"registerSessionKey","intentId":bound.intent_id,"queueRevision":1,"state":"queued"}).to_string())
            }
        } else {
            (404, "application/json", "{}".to_owned())
        };
        let response_bytes = response.2.as_bytes();
        let head = format!(
            "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.0,
            response.1,
            response_bytes.len()
        );
        let mut stream = stream.into_inner();
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(response_bytes).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn http_withdraw_submission_requires_official_hypercore_route_metadata() {
        let credentials = Arc::new(RuntimeFixtureCredentials::default());
        let signer =
            gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap();
        let owner = format!("{:#x}", signer.address()).to_ascii_lowercase();
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let typed = PapertradeTypedIntent::WithdrawToCore {
            user: owner.clone(),
            amount: "19000000".into(),
            nonce: "2010216708".into(),
            deadline: now + 120,
        };
        let (_, intent_id) = signer
            .papertrade_intent_identity(&to_evm_intent(&typed), now)
            .unwrap();
        *credentials.response_intent_id.lock().unwrap() = Some(intent_id.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server_credentials = credentials.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                serve_fixture_request(stream, server_credentials.clone()).await;
            }
        });

        let bound = BoundIntent {
            typed,
            intent_id: intent_id.clone(),
            signer: owner,
            action: Some("withdrawToCore".into()),
            registration_id: None,
            cancel_target_action: None,
        };
        let receipt = PapertradeHttp::test_with_base(&endpoint)
            .unwrap()
            .submit_typed(&bound, &signer)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(receipt.intent_id, intent_id);
        let request = credentials.response_body.lock().unwrap().clone().unwrap();
        assert_eq!(
            credentials.origin_header.lock().unwrap().as_deref(),
            Some(API_ORIGIN)
        );
        assert_eq!(request["withdrawal"], json!({"route":"hypercore"}));
        assert_eq!(request["signed"]["amount"], "19000000");
        assert_eq!(request["signed"]["nonce"], "2010216708");
        assert!(request["signed"]["deadline"].is_string());
    }

    #[test]
    fn papertrade_error_codes_preserve_safe_lowercase_classification_only() {
        assert_eq!(
            bounded_exchange_error_code(&json!({
                "error":"intake_action_paused",
                "message":"sensitive response detail"
            }))
            .as_deref(),
            Some("intake_action_paused")
        );
        assert_eq!(
            bounded_exchange_error_code(&json!({"error":{"code":"bad_nonce"}})).as_deref(),
            Some("bad_nonce")
        );
        assert_eq!(
            bounded_exchange_error_code(&json!({"code":"bad nonce"})),
            None
        );
        assert_eq!(
            bounded_exchange_error_code(&json!({"error":"x".repeat(65)})),
            None
        );
    }

    #[tokio::test]
    async fn broker_runtime_submits_and_proves_zero_reward_claim_through_real_http_observer() {
        let credentials = Arc::new(RuntimeFixtureCredentials::default());
        let rpc = Arc::new(RuntimeFixtureRpc::default());
        let owner_signer = Arc::new(
            gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap(),
        );
        let owner = format!("{:#x}", owner_signer.address()).to_ascii_lowercase();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let http_credentials = credentials.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(serve_fixture_request(stream, http_credentials.clone()));
            }
        });
        let funding = Arc::new(
            crate::papertrade_funding_host::PapertradeFundingHostAdapter::new(Arc::new(
                RuntimeFixtureFunding,
            )),
        );
        let runtime = BrokerPapertradeRuntime::new_with_http(
            PapertradeAccount {
                name: "fixture".into(),
                owner: owner.clone(),
                signing_enabled: true,
            },
            gofin_credentials::CredentialRef::new("00000000-0000-4000-8000-000000000001").unwrap(),
            credentials.clone(),
            Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 999, None)),
            2,
            false,
            Arc::new(RuntimeFixtureValuator),
            funding,
            PapertradeHttp::test_with_base(&endpoint).unwrap(),
        )
        .unwrap();

        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let typed = PapertradeTypedIntent::Claim {
            user: owner.clone(),
            nonce: "17".into(),
            deadline: now + 120,
        };
        let evm_intent = to_evm_intent(&typed);
        let (signer_address, intent_id) = owner_signer
            .papertrade_intent_identity(&evm_intent, now)
            .unwrap();
        assert_eq!(signer_address, owner);
        *credentials.response_intent_id.lock().unwrap() = Some(intent_id.clone());
        let bound = BoundIntent {
            typed: typed.clone(),
            intent_id: intent_id.clone(),
            signer: owner.clone(),
            action: Some("paperClaim".into()),
            registration_id: None,
            cancel_target_action: None,
        };
        let receipt = runtime.dispatch(&bound).await.unwrap();
        assert_eq!(receipt.status, "submitted");
        assert_eq!(receipt.intent_id, intent_id);
        let request = credentials.response_body.lock().unwrap().clone().unwrap();
        assert_eq!(
            credentials.origin_header.lock().unwrap().as_deref(),
            Some(API_ORIGIN)
        );
        assert_eq!(request["action"], "paperClaim");
        assert!(request["signed"]["nonce"].is_string());
        assert!(request["signed"]["deadline"].is_string());
        assert!(request["signed"]["v"].is_string());

        let signature = owner_signer
            .sign_papertrade_intent(&evm_intent, now)
            .unwrap();
        let PapertradeTypedIntent::Claim {
            user,
            nonce,
            deadline,
        } = &typed
        else {
            unreachable!()
        };
        let mut call = hex::decode("9c96d4a1").unwrap();
        call.extend(fixture_address_word(user));
        push_fixture_word(&mut call, nonce.parse().unwrap());
        push_fixture_word(&mut call, *deadline);
        push_fixture_word(&mut call, signature.v.parse().unwrap());
        call.extend(hex::decode(signature.r.trim_start_matches("0x")).unwrap());
        call.extend(hex::decode(signature.s.trim_start_matches("0x")).unwrap());
        let tx_input = fixture_batch_input(gofin_papertrade::PAPER_STAKING, &call, &intent_id);
        // CallExecuted(bool,bytes) returns one exact zero uint256. A zero
        // claim intentionally emits no Claimed event, so verification relies
        // on both the result and pinned deployed implementation identity.
        let call_data = format!("0x{:064x}{:064x}{:064x}{:064x}", 1, 64, 32, 0);
        let tx_hash = format!("0x{}", "a".repeat(64));
        let block_hash = format!("0x{}", "b".repeat(64));
        let call_log = json!({
            "address":BATCH_EXECUTOR,
            "topics":[CALL_EXECUTED_TOPIC,format!("0x{}", "1".repeat(64)),format!("0x{}", "0".repeat(64)),intent_id],
            "data":call_data,
            "logIndex":"0x0"
        });
        *rpc.chain.lock().unwrap() = Some(RuntimeFixtureChain {
            intent_id: intent_id.clone(),
            tx_input,
            tx_hash,
            block_hash,
            logs: json!([call_log]),
        });
        let canonical = runtime
            .canonical_intent_transaction(&intent_id, 1)
            .await
            .unwrap()
            .expect("canonical intent lookup must find the exact batch event");
        let proof =
            gofin_papertrade::verify_canonical_call(&canonical, &typed, &intent_id).unwrap();
        assert_eq!(proof.action, "paperClaim");
        assert_eq!(
            proof.call_return_data.as_deref(),
            Some(format!("0x{}", "0".repeat(64)).as_str())
        );
        assert!(proof.events.is_empty());
        assert_eq!(
            canonical.staking_implementation.as_deref(),
            Some(gofin_papertrade::PAPER_STAKING_IMPLEMENTATION)
        );
        assert_eq!(
            canonical.staking_code_hash.as_deref(),
            Some(gofin_papertrade::PAPER_STAKING_CODE_HASH)
        );
        server.abort();
    }

    #[tokio::test]
    async fn broker_runtime_signs_submits_and_canonically_verifies_session_registration() {
        let credentials = Arc::new(RuntimeFixtureCredentials::default());
        let rpc = Arc::new(RuntimeFixtureRpc::default());
        let owner_signer =
            gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap();
        let owner = format!("{:#x}", owner_signer.address()).to_ascii_lowercase();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let http_credentials = credentials.clone();
        let http_server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let credentials = http_credentials.clone();
                tokio::spawn(serve_fixture_request(stream, credentials));
            }
        });
        let valuator: Arc<dyn crate::AssetValuator> = Arc::new(RuntimeFixtureValuator);
        let funding = Arc::new(
            crate::papertrade_funding_host::PapertradeFundingHostAdapter::new(Arc::new(
                RuntimeFixtureFunding,
            )),
        );
        let mut runtime = Arc::new(
            BrokerPapertradeRuntime::new_with_http(
                PapertradeAccount {
                    name: "fixture".into(),
                    owner: owner.clone(),
                    signing_enabled: true,
                },
                gofin_credentials::CredentialRef::new("00000000-0000-4000-8000-000000000001")
                    .unwrap(),
                credentials.clone(),
                Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 999, None)),
                2,
                false,
                valuator,
                funding,
                PapertradeHttp::test_with_base(&endpoint).unwrap(),
            )
            .unwrap(),
        );
        Arc::get_mut(&mut runtime)
            .expect("fixture owns the runtime")
            .observer
            .page_delay = Duration::ZERO;
        runtime
            .attach_execution_state(Arc::new(RuntimeFixtureExecutionState::default()))
            .unwrap();

        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let expiry = now + 3_600;
        let bound = runtime
            .prepare_session_registration(
                &owner,
                "fixture-registration",
                expiry,
                "fixture-registration",
            )
            .await
            .unwrap();
        let binding = gofin_papertrade::SessionRegistrationBinding::from_bound(
            "fixture-registration".into(),
            &bound,
        )
        .unwrap();
        let session_key = match &bound.typed {
            PapertradeTypedIntent::RegisterSessionKey { key, .. } => key.clone(),
            _ => unreachable!(),
        };
        let mut revoke_calldata = hex::decode("84f4fc6a").unwrap();
        revoke_calldata.extend_from_slice(&[0; 12]);
        revoke_calldata.extend(hex::decode(session_key.trim_start_matches("0x")).unwrap());
        let revoke = gofin_papertrade::BoundSessionRevoke {
            owner: owner.clone(),
            session_key,
            registration_id: binding.registration_id.clone(),
            submission_id: "fixture-revoke".into(),
            transaction: gofin_evm::FrozenTransaction {
                chain_id: 999,
                sender: owner.clone(),
                to: gofin_papertrade::SESSION_KEY_MANAGER.into(),
                value: gofin_domain::Quantity::from_u64(0),
                calldata: format!("0x{}", hex::encode(revoke_calldata)),
                nonce: 1,
                gas: 100_000,
                max_fee_per_gas: gofin_domain::Quantity::from_u64(1_000_000_000),
                max_priority_fee_per_gas: gofin_domain::Quantity::from_u64(100_000_000),
                rollup_fee: None,
            },
        };
        let revoke_valuation = runtime
            .session_revocation_valuation(&revoke)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            revoke_valuation.maximum_fee_usd_micros,
            gofin_domain::Quantity::from_u64(200_000)
        );
        assert_eq!(
            revoke_valuation.prices[0].asset,
            gofin_domain::AssetId {
                network: "ethereum:999".into(),
                reference: "native".into(),
                decimals: 18,
            }
        );
        assert_eq!(
            runtime
                .session_registration_from_block(&binding)
                .await
                .unwrap(),
            None,
            "the historical/lost-response case has no dispatch anchor"
        );
        let pinned_code = include_str!("testdata/session-key-manager-implementation.hex")
            .trim()
            .to_owned();
        let set_expiry_fixture = |timestamp: u64,
                                  implementation: &str,
                                  code: &str,
                                  nonce_used: bool,
                                  session_words: &str,
                                  changed_hash_on_reread: bool| {
            *rpc.expiry.lock().unwrap() = Some(RuntimeFixtureExpiry {
                block_hash: format!("0x{}", "d".repeat(64)),
                timestamp,
                head_number: 10,
                timestamp_per_block: 0,
                implementation: implementation.to_owned(),
                exchange_implementation: None,
                staking_implementation: None,
                batch_implementation: None,
                exchange_code_invalid: false,
                staking_code_invalid: false,
                batch_code_invalid: false,
                code: code.to_owned(),
                nonce_used,
                session_words: session_words.to_owned(),
                intent_id: None,
                intent_present: false,
                changed_hash_on_reread,
                changed_hash_on_final_reread: false,
                block_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            });
        };
        set_expiry_fixture(
            binding.deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            &format!("0x{}", "0".repeat(128)),
            false,
        );
        let expiry_proof = runtime
            .session_registration_expiry_proof(&binding)
            .await
            .unwrap()
            .expect("canonical post-deadline unused nonce proves expiry");
        expiry_proof.validate_against(&binding).unwrap();
        let registered_key = match &bound.typed {
            PapertradeTypedIntent::RegisterSessionKey { key, .. } => key.to_ascii_lowercase(),
            _ => unreachable!(),
        };
        assert_ne!(registered_key, owner.to_ascii_lowercase());
        assert!(
            rpc.session_queries
                .lock()
                .unwrap()
                .iter()
                .any(|(queried_owner, queried_key)| {
                    queried_owner.eq_ignore_ascii_case(&owner)
                        && queried_key.eq_ignore_ascii_case(&registered_key)
                })
        );

        set_expiry_fixture(
            binding.deadline,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            &format!("0x{}", "0".repeat(128)),
            false,
        );
        assert!(
            runtime
                .session_registration_expiry_proof(&binding)
                .await
                .unwrap()
                .is_none()
        );
        set_expiry_fixture(
            binding.deadline + 1,
            "0x1111111111111111111111111111111111111111",
            &pinned_code,
            false,
            &format!("0x{}", "0".repeat(128)),
            false,
        );
        assert!(
            runtime
                .session_registration_expiry_proof(&binding)
                .await
                .unwrap()
                .is_none()
        );
        set_expiry_fixture(
            binding.deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            "0x6000",
            false,
            &format!("0x{}", "0".repeat(128)),
            false,
        );
        assert!(
            runtime
                .session_registration_expiry_proof(&binding)
                .await
                .unwrap()
                .is_none()
        );
        set_expiry_fixture(
            binding.deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            true,
            &format!("0x{}", "0".repeat(128)),
            false,
        );
        assert!(
            runtime
                .session_registration_expiry_proof(&binding)
                .await
                .unwrap()
                .is_none()
        );
        set_expiry_fixture(
            binding.deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            &format!("0x{:064x}{}", 1, "0".repeat(64)),
            false,
        );
        assert!(
            runtime
                .session_registration_expiry_proof(&binding)
                .await
                .unwrap()
                .is_none()
        );
        set_expiry_fixture(
            binding.deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            &format!("0x{}", "0".repeat(128)),
            true,
        );
        assert!(
            runtime
                .session_registration_expiry_proof(&binding)
                .await
                .unwrap()
                .is_none()
        );
        let withdrawal_id = format!("0x{}", "1".repeat(64));
        let withdrawal_deadline = now;
        let withdrawal = BoundIntent {
            typed: PapertradeTypedIntent::WithdrawToCore {
                user: owner.clone(),
                amount: "1000000".into(),
                nonce: "17".into(),
                deadline: withdrawal_deadline,
            },
            intent_id: withdrawal_id.clone(),
            signer: owner.clone(),
            action: Some("withdrawToCore".into()),
            registration_id: None,
            cancel_target_action: None,
        };
        let set_withdrawal_expiry =
            |timestamp: u64,
             manager_implementation: &str,
             manager_code: &str,
             nonce_used: bool,
             exchange_implementation: Option<&str>,
             intent_present: bool,
             changed_hash_on_final_reread: bool| {
                *rpc.expiry.lock().unwrap() = Some(RuntimeFixtureExpiry {
                    block_hash: format!("0x{}", "d".repeat(64)),
                    timestamp,
                    head_number: 10,
                    timestamp_per_block: 0,
                    implementation: manager_implementation.trim_start_matches("0x").to_owned(),
                    exchange_implementation: exchange_implementation
                        .map(|implementation| implementation.trim_start_matches("0x").to_owned()),
                    staking_implementation: None,
                    batch_implementation: None,
                    exchange_code_invalid: false,
                    staking_code_invalid: false,
                    batch_code_invalid: false,
                    code: manager_code.to_owned(),
                    nonce_used,
                    session_words: format!("0x{}", "0".repeat(128)),
                    intent_id: Some(withdrawal_id.clone()),
                    intent_present,
                    changed_hash_on_reread: changed_hash_on_final_reread,
                    changed_hash_on_final_reread,
                    block_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                });
            };
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            None,
            false,
            false,
        );
        {
            let mut expiry = rpc.expiry.lock().unwrap();
            let fixture = expiry.as_mut().unwrap();
            fixture.timestamp = withdrawal_deadline - 1_000_000;
            fixture.head_number = 2_000_000;
            fixture.timestamp_per_block = 1;
        }
        let withdrawal_proof = runtime
            .core_withdrawal_expiry_proof(&withdrawal)
            .await
            .unwrap()
            .expect("same-block pins, unused nonce, and absent intent prove expiry");
        withdrawal_proof.validate_against(&withdrawal).unwrap();
        assert_eq!(withdrawal_proof.canonical_block_number, 1_000_001);
        let windows = rpc.log_windows.lock().unwrap().clone();
        assert_eq!(
            windows.first().map(|window| window.0),
            Some(1_000_000 - gofin_papertrade::MAX_INTENT_SECONDS)
        );
        assert_eq!(windows.last().map(|window| window.1), Some(1_000_001));
        assert!(windows.len() <= 73);
        assert!(windows.iter().all(|(start, end)| end - start < 50));
        assert!(
            windows
                .windows(2)
                .all(|pair| pair[0].1.saturating_add(1) == pair[1].0)
        );
        set_withdrawal_expiry(
            withdrawal_deadline,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            None,
            false,
            false,
        );
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            None,
            false,
            false,
        );
        rpc.expiry
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .exchange_code_invalid = true;
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            true,
            None,
            false,
            false,
        );
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            "0x1111111111111111111111111111111111111111",
            &pinned_code,
            false,
            None,
            false,
            false,
        );
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            "0x6000",
            false,
            None,
            false,
            false,
        );
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            Some("0x1111111111111111111111111111111111111111"),
            false,
            false,
        );
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            None,
            true,
            false,
        );
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        set_withdrawal_expiry(
            withdrawal_deadline + 1,
            gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION,
            &pinned_code,
            false,
            None,
            false,
            true,
        );
        assert!(
            runtime
                .core_withdrawal_expiry_proof(&withdrawal)
                .await
                .unwrap()
                .is_none()
        );
        *rpc.expiry.lock().unwrap() = None;
        runtime
            .revalidate_session_configuration(
                &gofin_papertrade::SessionConfigurationBinding::Register {
                    registration: binding.clone(),
                },
            )
            .await
            .unwrap();
        let receipt = runtime
            .dispatch_session_registration(&binding)
            .await
            .unwrap();
        assert_eq!(receipt.status, "submitted");
        assert_eq!(
            runtime
                .session_registration_from_block(&binding)
                .await
                .unwrap(),
            Some(10)
        );

        let intent = to_evm_intent(&bound.typed);
        let signature = owner_signer.sign_papertrade_intent(&intent, now).unwrap();
        let PapertradeTypedIntent::RegisterSessionKey {
            user,
            key,
            expiry,
            fee_address,
            nonce,
            deadline,
        } = &bound.typed
        else {
            panic!("wrong typed intent")
        };
        let mut call = hex::decode("5f76a1d6").unwrap();
        for address in [user, key, fee_address] {
            call.extend(fixture_address_word(address));
        }
        push_fixture_word(&mut call, *expiry);
        // Solidity argument order is user, key, expiry, feeAddress, nonce, deadline, v, r, s.
        let mut ordered = hex::decode("5f76a1d6").unwrap();
        ordered.extend(fixture_address_word(user));
        ordered.extend(fixture_address_word(key));
        push_fixture_word(&mut ordered, *expiry);
        ordered.extend(fixture_address_word(fee_address));
        push_fixture_word(&mut ordered, nonce.parse().unwrap());
        push_fixture_word(&mut ordered, *deadline);
        push_fixture_word(&mut ordered, signature.v.parse().unwrap());
        ordered.extend(hex::decode(signature.r.trim_start_matches("0x")).unwrap());
        ordered.extend(hex::decode(signature.s.trim_start_matches("0x")).unwrap());
        let tx_input = fixture_batch_input(
            gofin_papertrade::SESSION_KEY_MANAGER,
            &ordered,
            &bound.intent_id,
        );
        let owner_topic = format!("0x{}{}", "0".repeat(24), owner.trim_start_matches("0x"));
        let key_topic = format!("0x{}{}", "0".repeat(24), key.trim_start_matches("0x"));
        let expiry_word = format!("0x{expiry:064x}");
        let executed_data = format!("0x{:064x}{:064x}{:064x}", 1, 64, 0);
        let call_log = json!({"address":gofin_papertrade::BATCH_EXECUTOR,"topics":[CALL_EXECUTED_TOPIC,format!("0x{}", "1".repeat(64)),format!("0x{}", format!("{:064x}", 0)),bound.intent_id],"data":executed_data,"logIndex":"0x1"});
        let registered_log = json!({"address":gofin_papertrade::SESSION_KEY_MANAGER,"topics":["0x3cec0566535d67d6c92668517af0bf8a66cdc91d622dbb830fcc08e1bcf5d1df",owner_topic,key_topic],"data":expiry_word,"logIndex":"0x0"});
        let logs = json!([registered_log, call_log]);
        let tx_hash = format!("0x{}", "a".repeat(64));
        let block_hash = format!("0x{}", "b".repeat(64));
        *rpc.chain.lock().unwrap() = Some(RuntimeFixtureChain {
            intent_id: bound.intent_id.clone(),
            tx_input,
            tx_hash,
            block_hash,
            logs,
        });
        let tx = runtime
            .canonical_intent_transaction(&bound.intent_id, 1)
            .await
            .unwrap()
            .unwrap();
        let proof =
            gofin_papertrade::verify_canonical_call(&tx, &bound.typed, &bound.intent_id).unwrap();
        runtime
            .activate_session_registration(&binding, &proof)
            .await
            .unwrap();
        assert_eq!(credentials.papertrade_sessions(&owner).unwrap().len(), 1);
        let submitted = credentials.response_body.lock().unwrap().clone().unwrap();
        assert_eq!(submitted["action"], "registerSessionKey");
        assert_eq!(submitted["signed"]["primary_type"], Value::Null);
        assert_eq!(submitted["signed"]["expiry"], expiry.to_string());
        assert_eq!(submitted["signed"]["deadline"], deadline.to_string());
        assert!(submitted["signed"]["nonce"].is_string());
        assert!(submitted["signed"]["v"].is_string());
        http_server.abort();
    }

    #[tokio::test]
    async fn broker_runtime_proves_only_expired_bound_trade_or_staking_intents() {
        let credentials = Arc::new(RuntimeFixtureCredentials::default());
        let rpc = Arc::new(RuntimeFixtureRpc::default());
        let owner_signer =
            gofin_evm::LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap();
        let owner = format!("{:#x}", owner_signer.address()).to_ascii_lowercase();
        let funding = Arc::new(
            crate::papertrade_funding_host::PapertradeFundingHostAdapter::new(Arc::new(
                RuntimeFixtureFunding,
            )),
        );
        let runtime = BrokerPapertradeRuntime::new_with_http(
            PapertradeAccount {
                name: "fixture".into(),
                owner: owner.clone(),
                signing_enabled: true,
            },
            gofin_credentials::CredentialRef::new("00000000-0000-4000-8000-000000000001").unwrap(),
            credentials,
            Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 999, None)),
            2,
            false,
            Arc::new(RuntimeFixtureValuator),
            funding,
            PapertradeHttp::test_with_base("http://127.0.0.1:1").unwrap(),
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let bound = BoundIntent {
            typed: PapertradeTypedIntent::Stake {
                user: owner.clone(),
                amount: "1000000000000000000".into(),
                nonce: "17".into(),
                deadline: now.saturating_sub(10),
            },
            intent_id: format!("0x{}", "1".repeat(64)),
            signer: owner.clone(),
            action: Some("paperStake".into()),
            registration_id: None,
            cancel_target_action: None,
        };
        let manager_code = include_str!("testdata/session-key-manager-implementation.hex")
            .trim()
            .to_owned();
        let configure = |timestamp: u64,
                         nonce_used: bool,
                         intent_present: bool,
                         staking_impl: &str,
                         batch_impl: &str,
                         batch_code_invalid: bool,
                         changed_hash: bool| {
            *rpc.expiry.lock().unwrap() = Some(RuntimeFixtureExpiry {
                block_hash: format!("0x{}", "d".repeat(64)),
                timestamp,
                head_number: 10,
                timestamp_per_block: 0,
                implementation: gofin_papertrade::SESSION_KEY_MANAGER_IMPLEMENTATION.into(),
                exchange_implementation: None,
                staking_implementation: Some(staking_impl.trim_start_matches("0x").to_owned()),
                batch_implementation: Some(batch_impl.trim_start_matches("0x").to_owned()),
                exchange_code_invalid: false,
                staking_code_invalid: false,
                batch_code_invalid,
                code: manager_code.clone(),
                nonce_used,
                session_words: format!("0x{}", "0".repeat(128)),
                intent_id: Some(bound.intent_id.clone()),
                intent_present,
                changed_hash_on_reread: changed_hash,
                changed_hash_on_final_reread: changed_hash,
                block_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            });
        };
        let pinned_staking = gofin_papertrade::PAPER_STAKING_IMPLEMENTATION;
        let pinned_batch = gofin_papertrade::BATCH_EXECUTOR_IMPLEMENTATION;
        configure(
            now,
            false,
            false,
            pinned_staking,
            pinned_batch,
            false,
            false,
        );
        {
            let mut expiry = rpc.expiry.lock().unwrap();
            let fixture = expiry.as_mut().unwrap();
            fixture.timestamp = now.saturating_sub(12);
            fixture.timestamp_per_block = 1;
        }
        let proof = runtime.intent_expiry_proof(&bound).await.unwrap().unwrap();
        proof.validate_against(&bound).unwrap();
        assert_eq!(proof.canonical_block_number, 3);
        assert_eq!(proof.nonce_proxy, gofin_papertrade::PAPER_STAKING);
        assert_eq!(proof.target_proxy, gofin_papertrade::PAPER_STAKING);

        let exchange_bound = BoundIntent {
            typed: PapertradeTypedIntent::OpenPosition {
                user: owner.clone(),
                instrument_id: 1,
                is_long: true,
                size: 10_000_000_000_000_000_000,
                leverage: 1,
                use_debt: false,
                nonce: "18".into(),
                deadline: now.saturating_sub(10),
            },
            intent_id: bound.intent_id.clone(),
            signer: owner.clone(),
            action: Some("openPosition".into()),
            registration_id: Some("fixture-session-registration".into()),
            cancel_target_action: None,
        };
        configure(
            now,
            false,
            false,
            pinned_staking,
            pinned_batch,
            false,
            false,
        );
        {
            let mut expiry = rpc.expiry.lock().unwrap();
            let fixture = expiry.as_mut().unwrap();
            fixture.timestamp = now.saturating_sub(12);
            fixture.timestamp_per_block = 1;
        }
        let exchange_proof = runtime
            .intent_expiry_proof(&exchange_bound)
            .await
            .unwrap()
            .unwrap();
        exchange_proof.validate_against(&exchange_bound).unwrap();
        assert_eq!(exchange_proof.target_proxy, gofin_papertrade::EXCHANGE);
        assert_eq!(
            exchange_proof.nonce_proxy,
            gofin_papertrade::SESSION_KEY_MANAGER
        );

        configure(
            bound_deadline(&bound),
            false,
            false,
            pinned_staking,
            pinned_batch,
            false,
            false,
        );
        assert!(runtime.intent_expiry_proof(&bound).await.unwrap().is_none());
        configure(now, true, false, pinned_staking, pinned_batch, false, false);
        assert!(runtime.intent_expiry_proof(&bound).await.unwrap().is_none());
        configure(now, false, true, pinned_staking, pinned_batch, false, false);
        assert!(runtime.intent_expiry_proof(&bound).await.unwrap().is_none());
        configure(
            now,
            false,
            false,
            "0x1111111111111111111111111111111111111111",
            pinned_batch,
            false,
            false,
        );
        assert!(runtime.intent_expiry_proof(&bound).await.unwrap().is_none());
        configure(
            now,
            false,
            false,
            pinned_staking,
            pinned_batch,
            false,
            false,
        );
        rpc.expiry
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .staking_code_invalid = true;
        assert!(runtime.intent_expiry_proof(&bound).await.unwrap().is_none());
        configure(now, false, false, pinned_staking, pinned_batch, true, false);
        assert!(runtime.intent_expiry_proof(&bound).await.unwrap().is_none());
        configure(now, false, false, pinned_staking, pinned_batch, false, true);
        assert!(runtime.intent_expiry_proof(&bound).await.unwrap().is_none());
        let cancel = BoundIntent {
            typed: PapertradeTypedIntent::CancelIntent {
                user: owner.clone(),
                intent_id: format!("0x{}", "2".repeat(64)),
                deadline: (now - 1).to_string(),
            },
            intent_id: format!("0x{}", "3".repeat(64)),
            signer: owner,
            action: None,
            registration_id: None,
            cancel_target_action: Some("openPosition".into()),
        };
        assert!(
            runtime
                .intent_expiry_proof(&cancel)
                .await
                .unwrap()
                .is_none()
        );
    }

    fn bound_deadline(bound: &BoundIntent) -> u64 {
        match &bound.typed {
            PapertradeTypedIntent::Stake { deadline, .. } => *deadline,
            _ => unreachable!(),
        }
    }

    struct RuntimeFixtureValuator;
    #[async_trait::async_trait]
    impl crate::AssetValuator for RuntimeFixtureValuator {
        async fn usd_micros(
            &self,
            amount: &gofin_domain::Amount,
        ) -> Result<(String, String, String)> {
            if amount.asset.network != "ethereum:999" || amount.asset.reference != "native" {
                return Err(GoFinError::validation(
                    "fixture only prices HyperEVM native gas",
                ));
            }
            Ok((
                "2000000000".into(),
                "fixture-observation".into(),
                "controlled-price-source".into(),
            ))
        }
    }
}
