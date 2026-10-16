//! Reviewed mainnet-beta read pool and primary-only financial transport.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use reqwest::{Client, redirect::Policy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{sync::OnceCell, time::Instant};

use crate::{MAINNET_GENESIS_HASH, NETWORK, failure};
use gofin_domain::{GoFinError, Result};

pub const MAINNET_RPC_URLS: &[&str] = &[
    "https://api.mainnet-beta.solana.com",
    "https://solana-rpc.publicnode.com",
    "https://solana.api.onfinality.io/public",
    "https://rpc.solanatracker.io/public",
];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_ATTEMPTS: usize = 3;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const COOLDOWN: Duration = Duration::from_secs(30);
const MAX_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
const HEAD_CACHE_AGE: Duration = Duration::from_secs(2);
const SLOT_MARGIN: u64 = 32;
const MAX_RESPONSE_SCOPES: usize = 128;

#[derive(Clone, Copy, Debug, Default)]
pub struct RpcReadRequirements {
    pub minimum_context_slot: Option<u64>,
    pub require_non_null: bool,
    pub finalized_signature: bool,
    pub merge_signature_history: bool,
}

impl RpcReadRequirements {
    fn validate_request(self, method: &str) -> Result<()> {
        if !is_retryable_method(method)
            || (self.finalized_signature && method != "getSignatureStatuses")
            || (self.merge_signature_history && method != "getSignaturesForAddress")
            || (self.minimum_context_slot.is_some() && !has_context(method))
        {
            return Err(GoFinError::validation(
                "Invalid Solana read evidence requirements.",
            ));
        }
        Ok(())
    }

    // Absence and nonfinalized observations remain observations for injected
    // transports too. The HTTP pool searches all eligible providers first.
    fn validate(self, method: &str, value: &Value) -> Result<()> {
        if let Some(minimum) = self.minimum_context_slot {
            let slot = context_slot(method, value).ok_or_else(data_invalid)?;
            if slot < minimum {
                return Err(pool_error(
                    "SOLANA_RPC_STALE",
                    "The Solana response predates the required context slot.",
                ));
            }
        }
        Ok(())
    }

    fn sufficient(self, method: &str, value: &Value) -> bool {
        if value.is_null()
            || (has_context(method) && value.get("value").is_some_and(Value::is_null))
        {
            return false;
        }
        if self.require_non_null
            && method == "getMultipleAccounts"
            && value["value"]
                .as_array()
                .is_some_and(|items| items.iter().any(Value::is_null))
        {
            return false;
        }
        if method == "getSignatureStatuses" {
            return value["value"].as_array().is_some_and(|items| {
                !items.is_empty()
                    && items.iter().all(|item| {
                        !item.is_null()
                            && (!self.finalized_signature
                                || item["confirmationStatus"] == "finalized")
                    })
            });
        }
        true
    }
}

#[async_trait]
pub trait SolanaRpc: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value>;

    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        requirements.validate_request(method)?;
        let value = self.request(method, params).await?;
        requirements.validate(method, &value)?;
        Ok(value)
    }

    /// Complete nonfinancial primary preparation before the final send guard.
    async fn prepare_send(&self) -> Result<()> {
        Ok(())
    }

    /// Passive, bounded observations. Never URLs, credentials or response data.
    fn diagnostics(&self) -> Value {
        Value::Null
    }
}

struct Endpoint {
    url: String,
    // A wrong genesis is a permanent exclusion for this transport instance.
    verified: OnceCell<bool>,
}

#[derive(Default)]
struct SelectionState {
    cooldowns: BTreeMap<usize, Instant>,
    unsupported: BTreeMap<(usize, String), Instant>,
    heads: BTreeMap<(usize, String), (Instant, u64)>,
    response_slots: BTreeMap<(usize, String), u64>,
    response_order: VecDeque<(usize, String)>,
    slots: BTreeMap<String, u64>,
    heights: BTreeMap<String, u64>,
    health: BTreeMap<usize, (&'static str, Option<&'static str>)>,
    last_read: Option<Value>,
    last_failover: Option<Value>,
}

pub struct HttpRpc {
    client: Client,
    endpoints: Vec<Endpoint>,
    next_id: AtomicU64,
    selection: Mutex<SelectionState>,
}

impl HttpRpc {
    pub fn mainnet_beta() -> Result<Arc<Self>> {
        Self::new(
            MAINNET_RPC_URLS
                .iter()
                .map(|url| (*url).to_owned())
                .collect(),
        )
    }

    pub fn new(urls: Vec<String>) -> Result<Arc<Self>> {
        Self::construct(urls, false)
    }

    /// Internal synthetic harness only; production composition uses mainnet_beta.
    pub fn isolated_localhost_pool(urls: Vec<String>) -> Result<Arc<Self>> {
        Self::construct(urls, true)
    }

    fn construct(urls: Vec<String>, local: bool) -> Result<Arc<Self>> {
        if urls.is_empty() || urls.len() > 16 {
            return Err(GoFinError::validation(
                "A bounded, nonempty Solana RPC endpoint list is required.",
            ));
        }
        let mut endpoints = Vec::new();
        for text in urls {
            let url = reqwest::Url::parse(&text)
                .map_err(|_| GoFinError::validation("Invalid Solana RPC URL."))?;
            let loopback = matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            );
            if url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || (!local && (url.scheme() != "https" || loopback))
                || (local && (url.scheme() != "http" || !loopback))
                || endpoints
                    .iter()
                    .any(|endpoint: &Endpoint| endpoint.url == url.as_str())
            {
                return Err(GoFinError::validation(
                    "Production Solana RPCs require public HTTPS without embedded credentials; isolated fixtures require loopback HTTP and unique endpoints.",
                ));
            }
            endpoints.push(Endpoint {
                url: url.to_string(),
                verified: OnceCell::new(),
            });
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| transport_error())?;
        Ok(Arc::new(Self {
            client,
            endpoints,
            next_id: AtomicU64::new(1),
            selection: Mutex::new(SelectionState::default()),
        }))
    }

    #[cfg(test)]
    fn test_endpoint(endpoint: String) -> Self {
        Arc::try_unwrap(Self::isolated_localhost_pool(vec![endpoint]).unwrap())
            .ok()
            .unwrap()
    }

    async fn verify_network(&self, index: usize) -> Result<()> {
        let verified = self.endpoints[index]
            .verified
            .get_or_try_init(|| async {
                let genesis = self
                    .at(index, "getGenesisHash", json!([]), MAX_ATTEMPTS)
                    .await?;
                Ok::<bool, GoFinError>(genesis.as_str() == Some(MAINNET_GENESIS_HASH))
            })
            .await?;
        if !verified {
            return Err(network_mismatch());
        }
        Ok(())
    }

    fn cooldown_remaining(&self, index: usize) -> Option<Duration> {
        self.selection
            .lock()
            .ok()?
            .cooldowns
            .get(&index)?
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    }

    fn record_rate_limit(&self, index: usize, delay: Duration) {
        if let Ok(mut state) = self.selection.lock() {
            let until = Instant::now() + delay.min(MAX_COOLDOWN);
            state
                .cooldowns
                .entry(index)
                .and_modify(|old| *old = (*old).max(until))
                .or_insert(until);
        }
    }

    async fn at(
        &self,
        index: usize,
        method: &str,
        params: Value,
        attempts: usize,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        for attempt in 1..=attempts {
            if method != "sendTransaction" && self.cooldown_remaining(index).is_some() {
                return Err(pool_error(
                    "SOLANA_RPC_RATE_LIMITED",
                    "The Solana RPC is cooling down after a rate limit.",
                ));
            }
            match self.post_once(index, &body, id).await {
                Ok(value) => return Ok(value),
                Err(failure) => {
                    if failure.rate_limited {
                        self.record_rate_limit(index, failure.retry_after.unwrap_or(COOLDOWN));
                    }
                    if !failure.retryable || failure.rate_limited || attempt == attempts {
                        return Err(failure.error);
                    }
                    tokio::time::sleep(Duration::from_millis(if attempt == 1 { 100 } else { 250 }))
                        .await;
                }
            }
        }
        unreachable!("nonempty bounded attempts")
    }

    #[cfg(test)]
    async fn post(&self, method: &str, params: Value) -> Result<Value> {
        let started = Instant::now();
        let attempts = if is_retryable_method(method) {
            MAX_ATTEMPTS
        } else {
            1
        };
        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.at(0, method, params, attempts))
            .await
            .unwrap_or_else(|_| Err(timeout_error()));
        result.map_err(|mut error| {
            let details = error.details.get_or_insert_with(|| Box::new(json!({})));
            details["method"] = json!(method);
            details["attempt"] = json!(1);
            details["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
            error
        })
    }

    async fn post_once(
        &self,
        index: usize,
        body: &Value,
        id: u64,
    ) -> std::result::Result<Value, RpcFailure> {
        let response = self
            .client
            .post(&self.endpoints[index].url)
            .json(body)
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
                rate_limited: false,
                retry_after: None,
            })?;
        let status = response.status();
        if !status.is_success() {
            let rate_limited = status.as_u16() == 429;
            let mut error = rpc_error(
                "The Solana mainnet-beta RPC returned an HTTP error.",
                "http_status",
                Some(status.as_u16()),
                None,
            );
            if rate_limited {
                error.code = "SOLANA_RPC_RATE_LIMITED".into();
            }
            return Err(RpcFailure {
                error,
                retryable: matches!(status.as_u16(), 429 | 502 | 503 | 504),
                rate_limited,
                retry_after: retry_after_delay(response.headers(), SystemTime::now()),
            });
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(RpcFailure::invalid(
                "response_too_large",
                Some(status.as_u16()),
                None,
            ));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| RpcFailure {
                error: rpc_error(
                    "The Solana RPC response could not be read.",
                    "body_read",
                    Some(status.as_u16()),
                    None,
                ),
                retryable: true,
                rate_limited: false,
                retry_after: None,
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(RpcFailure::invalid(
                    "response_too_large",
                    Some(status.as_u16()),
                    None,
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let envelope: Value = serde_json::from_slice(&body)
            .map_err(|_| RpcFailure::invalid("json", Some(status.as_u16()), None))?;
        if envelope["jsonrpc"] != "2.0"
            || envelope.get("id").and_then(Value::as_u64) != Some(id)
            || (envelope.get("error").is_some() && envelope.get("result").is_some())
        {
            return Err(RpcFailure::invalid(
                "id_mismatch",
                Some(status.as_u16()),
                None,
            ));
        }
        if let Some(error) = envelope.get("error") {
            let code = error.get("code").and_then(Value::as_i64);
            let class = match code {
                Some(429 | -32429) => "SOLANA_RPC_RATE_LIMITED",
                Some(-32601) => "SOLANA_RPC_UNSUPPORTED",
                Some(-32016) => "SOLANA_RPC_STALE",
                Some(-32005) => "SOLANA_RPC_UNHEALTHY",
                Some(-32001 | -32004 | -32007 | -32009 | -32011) => {
                    "SOLANA_RPC_HISTORY_UNAVAILABLE"
                }
                _ => "SOLANA_RPC_UNAVAILABLE",
            };
            let mut mapped = rpc_error(
                "The Solana mainnet-beta RPC returned an application error.",
                "rpc_error",
                Some(status.as_u16()),
                code,
            );
            mapped.code = class.into();
            // Unknown semantic rejections are not reasons to search for success.
            if let Some(details) = mapped.details.as_deref_mut() {
                details["semantic_rejection"] = json!(!matches!(
                    code,
                    Some(
                        429 | -32429
                            | -32601
                            | -32016
                            | -32001
                            | -32004
                            | -32005
                            | -32007
                            | -32009
                            | -32011
                    )
                ));
            }
            return Err(RpcFailure {
                error: mapped,
                retryable: code == Some(-32005),
                rate_limited: matches!(code, Some(429 | -32429)),
                retry_after: None,
            });
        }
        envelope
            .get("result")
            .cloned()
            .ok_or_else(|| RpcFailure::invalid("missing_result", Some(status.as_u16()), None))
    }

    async fn fresh_head(&self, index: usize, commitment: &str) -> Result<u64> {
        let key = (index, commitment.to_owned());
        if let Some(head) = self
            .selection
            .lock()
            .map_err(|_| transport_error())?
            .heads
            .get(&key)
            .filter(|(at, _)| at.elapsed() < HEAD_CACHE_AGE)
            .map(|(_, slot)| *slot)
        {
            return Ok(head);
        }
        let slot = self
            .at(
                index,
                "getSlot",
                json!([{"commitment":commitment}]),
                MAX_ATTEMPTS,
            )
            .await?
            .as_u64()
            .ok_or_else(data_invalid)?;
        let timestamp = self
            .at(index, "getBlockTime", json!([slot]), MAX_ATTEMPTS)
            .await?
            .as_i64()
            .ok_or_else(stale_error)?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if timestamp < 0
            || timestamp as u64 > now.saturating_add(30)
            || now.saturating_sub(timestamp as u64) > 60
        {
            return Err(stale_error());
        }
        let mut state = self.selection.lock().map_err(|_| transport_error())?;
        if state
            .heads
            .get(&key)
            .is_some_and(|(_, previous)| slot < *previous)
            || state
                .slots
                .get(commitment)
                .is_some_and(|previous| slot.saturating_add(SLOT_MARGIN) < *previous)
        {
            return Err(stale_error());
        }
        state.heads.insert(key, (Instant::now(), slot));
        state
            .slots
            .entry(commitment.to_owned())
            .and_modify(|previous| *previous = (*previous).max(slot))
            .or_insert(slot);
        Ok(slot)
    }

    async fn eligible_read(
        &self,
        index: usize,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        self.verify_network(index).await?;
        let commitment = commitment(method, &params);
        let needs_head = has_context(method)
            || matches!(
                method,
                "getSlot" | "getBlockHeight" | "getSignaturesForAddress"
            );
        let head = if needs_head {
            Some(self.fresh_head(index, commitment).await?)
        } else {
            None
        };
        let value = self.at(index, method, params.clone(), MAX_ATTEMPTS).await?;
        validate_shape(method, &params, &value)?;
        requirements.validate(method, &value)?;
        if method == "getBlockHeight" {
            // A syntactically valid integer may actually be a slot (observed
            // on a public provider). A CURRENT blockhash's expiry is an upper
            // bound on height, never on slot. Check before updating watermarks.
            // This observation never replaces any frozen transaction blockhash.
            let latest = self
                .at(index, "getLatestBlockhash", params.clone(), MAX_ATTEMPTS)
                .await?;
            validate_shape("getLatestBlockhash", &params, &latest)?;
            let slot = context_slot("getLatestBlockhash", &latest).ok_or_else(data_invalid)?;
            if head.is_some_and(|head| slot.abs_diff(head) > SLOT_MARGIN)
                || request_minimum(method, &params).is_some_and(|minimum| slot < minimum)
            {
                return Err(stale_error());
            }
            if value.as_u64().ok_or_else(data_invalid)?
                > latest["value"]["lastValidBlockHeight"]
                    .as_u64()
                    .ok_or_else(data_invalid)?
            {
                return Err(data_invalid());
            }
        }
        let minimum = request_minimum(method, &params).max(requirements.minimum_context_slot);
        if let Some(head) = head {
            let observed = if method == "getSignaturesForAddress" || method == "getBlockHeight" {
                head
            } else {
                context_slot(method, &value).ok_or_else(data_invalid)?
            };
            if observed.saturating_add(SLOT_MARGIN) < head
                || (method != "getSignatureStatuses" && observed > head.saturating_add(SLOT_MARGIN))
                || minimum.is_some_and(|minimum| observed < minimum)
            {
                return Err(stale_error());
            }
            // Status context is the processing bank, even for finalized entries.
            // It must not advance the finalized slot watermark.
            let mut state = self.selection.lock().map_err(|_| transport_error())?;
            if method != "getSignatureStatuses" {
                // Independent methods/accounts can legitimately observe
                // different banks. Only identical read scopes have a strict
                // response watermark; caller minima still apply across scopes.
                // Bound the cache and retain no raw addresses or request data.
                let mut digest = Sha256::new();
                digest.update(method.as_bytes());
                digest.update([0]);
                digest.update(params.to_string().as_bytes());
                let key = (index, hex::encode(digest.finalize()));
                if has_context(method)
                    && state
                        .response_slots
                        .get(&key)
                        .is_some_and(|previous| observed < *previous)
                {
                    return Err(stale_error().with_details(json!({
                        "stage":"context_regression", "observed_slot":observed,
                        "previous_slot":state.response_slots[&key],
                    })));
                }
                if state
                    .slots
                    .get(commitment)
                    .is_some_and(|previous| observed.saturating_add(SLOT_MARGIN) < *previous)
                {
                    return Err(stale_error());
                }
                state
                    .slots
                    .entry(commitment.to_owned())
                    .and_modify(|previous| *previous = (*previous).max(observed))
                    .or_insert(observed);
                if has_context(method) {
                    if !state.response_slots.contains_key(&key) {
                        state.response_order.push_back(key.clone());
                        if state.response_order.len() > MAX_RESPONSE_SCOPES
                            && let Some(oldest) = state.response_order.pop_front()
                        {
                            state.response_slots.remove(&oldest);
                        }
                    }
                    state.response_slots.insert(key, observed);
                }
            }
            if method == "getBlockHeight" {
                let height = value.as_u64().ok_or_else(data_invalid)?;
                if state
                    .heights
                    .get(commitment)
                    .is_some_and(|previous| height < *previous)
                {
                    return Err(stale_error());
                }
                state
                    .heights
                    .entry(commitment.to_owned())
                    .and_modify(|previous| *previous = (*previous).max(height))
                    .or_insert(height);
            }
            if method == "getSignatureStatuses"
                && value["value"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["confirmationStatus"] == "finalized"
                            && item["slot"]
                                .as_u64()
                                .is_some_and(|slot| slot > head.saturating_add(SLOT_MARGIN))
                    })
                })
            {
                return Err(stale_error());
            }
        }
        Ok(value)
    }

    async fn pooled_read(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
        trace: &mut ReadTrace<'_>,
    ) -> Result<Value> {
        let mut eligible = Vec::new();
        let mut failure = transport_error();
        for index in 0..self.endpoints.len() {
            if self.endpoints[index].verified.get() == Some(&false) {
                trace.record(index, "incompatible");
                failure = network_mismatch();
            } else if self.cooldown_remaining(index).is_some() {
                trace.record(index, "rate_limited");
                failure = pool_error(
                    "SOLANA_RPC_RATE_LIMITED",
                    "All eligible Solana RPCs are cooling down.",
                );
            } else if self
                .selection
                .lock()
                .map_err(|_| transport_error())?
                .unsupported
                .get(&(index, method.to_owned()))
                .is_some_and(|until| *until > Instant::now())
            {
                trace.record(index, "unsupported");
                failure = pool_error(
                    "SOLANA_RPC_UNSUPPORTED",
                    "No eligible Solana RPC supports this read yet.",
                );
            } else {
                eligible.push(index);
            }
        }
        let mut next = 0;
        let mut pending = FuturesUnordered::new();
        let mut insufficient: Option<Value> = None;
        let mut history = BTreeMap::<String, Value>::new();
        let mut history_observed = false;
        loop {
            while pending.len() < 2 && next < eligible.len() {
                let index = eligible[next];
                next += 1;
                trace.record(index, "cancelled");
                let params = params.clone();
                pending.push(
                    async move {
                        (
                            index,
                            tokio::time::timeout(
                                ATTEMPT_TIMEOUT,
                                self.eligible_read(index, method, params, requirements),
                            )
                            .await
                            .unwrap_or_else(|_| Err(timeout_error())),
                        )
                    }
                    .boxed(),
                );
            }
            let Some(first) = pending.next().await else {
                break;
            };
            let mut batch = vec![first];
            while let Some(Some(completed)) = pending.next().now_or_never() {
                batch.push(completed);
            }
            // Record all already-ready observations before selecting a winner.
            // Contradictory finalized evidence or semantic rejection cannot be
            // hidden by the configured order of another completed response.
            let values = batch
                .iter()
                .filter_map(|(_, result)| result.as_ref().ok())
                .collect::<Vec<_>>();
            if responses_conflict(method, &values) {
                let error = conflict_error();
                for (index, result) in &batch {
                    if result.is_ok() {
                        self.record_health(*index, Some(&error));
                        trace.record(*index, "incompatible");
                    }
                }
                return Err(error);
            }
            for (index, result) in &batch {
                self.record_health(*index, result.as_ref().err());
                if let Err(error) = result {
                    trace.record(*index, reason(error));
                    if error.code == "SOLANA_RPC_UNSUPPORTED" {
                        self.selection
                            .lock()
                            .map_err(|_| transport_error())?
                            .unsupported
                            .insert((*index, method.to_owned()), Instant::now() + COOLDOWN);
                    }
                }
            }
            if let Some(error) = batch
                .iter()
                .filter_map(|(_, result)| result.as_ref().err())
                .find(|error| semantic_rejection(error))
            {
                return Err(error.clone());
            }
            if method == "simulateTransaction"
                && let Some((index, value)) = batch.iter().find_map(|(index, result)| {
                    result
                        .as_ref()
                        .ok()
                        .filter(|value| !value["value"]["err"].is_null())
                        .map(|value| (*index, value))
                })
            {
                trace.record(index, "ok");
                trace.finish("ok", Some(index));
                return Ok(value.clone());
            }
            batch.sort_by_key(|(index, _)| *index);
            let mut winner = None;
            for (index, result) in batch {
                match result {
                    Ok(value) if requirements.merge_signature_history => {
                        history_observed = true;
                        trace.record(index, "ok");
                        for entry in value.as_array().ok_or_else(data_invalid)? {
                            let signature = entry["signature"]
                                .as_str()
                                .ok_or_else(data_invalid)?
                                .to_owned();
                            if history.get(&signature).is_some_and(|old| {
                                old["slot"] != entry["slot"] || old["err"] != entry["err"]
                            }) {
                                let error = conflict_error();
                                self.record_health(index, Some(&error));
                                trace.record(index, "incompatible");
                                return Err(error);
                            }
                            history.entry(signature).or_insert_with(|| entry.clone());
                        }
                    }
                    Ok(value) if requirements.sufficient(method, &value) => {
                        trace.record(index, "ok");
                        if winner.is_none() {
                            winner = Some((index, value));
                        }
                    }
                    Ok(value) => {
                        trace.record(index, "missing");
                        if insufficient.as_ref().is_none_or(|old| {
                            observation_rank(method, &value) > observation_rank(method, old)
                        }) {
                            insufficient = Some(value);
                        }
                    }
                    Err(error) => {
                        failure = error;
                    }
                }
            }
            if let Some((index, value)) = winner {
                trace.finish("ok", Some(index));
                return Ok(value);
            }
        }
        if requirements.merge_signature_history && history_observed {
            let mut entries = history.into_values().collect::<Vec<_>>();
            entries.sort_by_key(|entry| std::cmp::Reverse(entry["slot"].as_u64().unwrap_or(0)));
            trace.finish(if entries.is_empty() { "missing" } else { "ok" }, None);
            return Ok(json!(entries));
        }
        if let Some(value) = insufficient {
            trace.finish("missing", None);
            return Ok(value);
        }
        Err(failure)
    }

    fn record_health(&self, index: usize, error: Option<&GoFinError>) {
        if let Ok(mut state) = self.selection.lock() {
            state.health.insert(
                index,
                error.map_or(("healthy", None), |error| {
                    let status = match reason(error) {
                        "rate_limited" => "cooling_down",
                        "lagging" => "lagging",
                        "unsupported" => "unsupported",
                        "incompatible" => "incompatible",
                        _ => "unavailable",
                    };
                    (status, Some(safe_code(&error.code)))
                }),
            );
        }
    }

    async fn read(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        requirements.validate_request(method)?;
        let mut trace = ReadTrace::new(self, method);
        let result = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.pooled_read(method, params, requirements, &mut trace),
        )
        .await
        .unwrap_or_else(|_| Err(timeout_error()));
        if result.is_err() {
            trace.finish("error", None);
        }
        result.map_err(|mut error| {
            let details = error.details.get_or_insert_with(|| Box::new(json!({})));
            details["rpc_health"] = self.diagnostics();
            error
        })
    }
}

#[async_trait]
impl SolanaRpc for HttpRpc {
    async fn request(&self, method: &str, mut params: Value) -> Result<Value> {
        if method == "sendTransaction" {
            let items = params
                .as_array_mut()
                .filter(|items| !items.is_empty() && items.len() <= 2)
                .ok_or_else(|| GoFinError::validation("Invalid Solana submission parameters."))?;
            if items.len() == 1 {
                items.push(json!({}));
            }
            let config = items[1].as_object_mut().ok_or_else(|| {
                GoFinError::validation("Invalid Solana submission configuration.")
            })?;
            config.insert("maxRetries".into(), json!(0));
            // No read race, read retry, or fallback can dispatch signed bytes.
            // prepare_send is called before the final runtime authorization guard.
            // Standalone internal callers still have to establish primary identity.
            self.prepare_send().await?;
            return tokio::time::timeout(REQUEST_TIMEOUT, self.at(0, method, params, 1))
                .await
                .unwrap_or_else(|_| Err(timeout_error()));
        }
        self.read(method, params, RpcReadRequirements::default())
            .await
    }

    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        self.read(method, params, requirements).await
    }

    async fn prepare_send(&self) -> Result<()> {
        let result = tokio::time::timeout(ATTEMPT_TIMEOUT, self.verify_network(0))
            .await
            .unwrap_or_else(|_| Err(timeout_error()));
        if let Err(error) = &result {
            self.record_health(0, Some(error));
        }
        result
    }

    fn diagnostics(&self) -> Value {
        let Ok(state) = self.selection.lock() else {
            return Value::Null;
        };
        let now = Instant::now();
        let endpoints = self.endpoints.iter().enumerate().map(|(index, endpoint)| {
            let remaining = state.cooldowns.get(&index).and_then(|until| until.checked_duration_since(now)).filter(|delay| !delay.is_zero());
            let (mut status, code) = state.health.get(&index).copied().unwrap_or(("unobserved",None));
            if remaining.is_some() { status = "cooling_down"; } else if status == "cooling_down" { status = "unobserved"; }
            let unsupported = state.unsupported.iter().filter(|((i,_),until)| *i == index && **until > now).map(|((_,method),_)| method.clone()).collect::<Vec<_>>();
            json!({"endpoint_id":format!("rpc-{}",index+1),"identity_verified":endpoint.verified.get()==Some(&true),"status":status,"last_error_code":code,"retry_after_ms":remaining.map(|delay| delay.as_nanos().div_ceil(1_000_000).min(u64::MAX as u128) as u64),"unsupported_methods":unsupported})
        }).collect::<Vec<_>>();
        json!({"network":NETWORK,"read_timeout_ms":20000,"endpoints":endpoints,"last_read":state.last_read,"last_failover":state.last_failover})
    }
}

fn has_context(method: &str) -> bool {
    matches!(
        method,
        "getAccountInfo"
            | "getBalance"
            | "getFeeForMessage"
            | "getLatestBlockhash"
            | "getMultipleAccounts"
            | "getSignatureStatuses"
            | "getSlot"
            | "getTokenAccountBalance"
            | "getTokenAccountsByOwner"
            | "getTokenAccountsByDelegate"
            | "getTokenLargestAccounts"
            | "getSupply"
            | "getTokenSupply"
            | "isBlockhashValid"
            | "simulateTransaction"
    )
}
fn context_slot(method: &str, value: &Value) -> Option<u64> {
    if method == "getSlot" {
        value.as_u64()
    } else {
        value["context"]["slot"].as_u64()
    }
}
fn config_index(method: &str) -> usize {
    match method {
        "getSlot" | "getBlockHeight" | "getLatestBlockhash" | "getSupply" => 0,
        "getTokenAccountsByOwner" | "getTokenAccountsByDelegate" => 2,
        _ => 1,
    }
}
fn commitment<'a>(method: &str, params: &'a Value) -> &'a str {
    if method == "getSignatureStatuses" {
        return "finalized";
    }
    params
        .get(config_index(method))
        .and_then(|config| config.get("commitment"))
        .and_then(Value::as_str)
        .unwrap_or("finalized")
}
fn request_minimum(method: &str, params: &Value) -> Option<u64> {
    params
        .get(config_index(method))
        .and_then(|config| config.get("minContextSlot"))
        .and_then(Value::as_u64)
}

fn validate_shape(method: &str, params: &Value, value: &Value) -> Result<()> {
    if value.is_null() && matches!(method, "getTransaction" | "getBlock" | "getBlockTime") {
        return Ok(());
    }
    if has_context(method) && context_slot(method, value).is_none() {
        return Err(data_invalid());
    }
    let valid = match method {
        "getGenesisHash" => value.as_str() == Some(MAINNET_GENESIS_HASH),
        "getBalance" => value["value"].is_u64(),
        "getBlockHeight"
        | "getSlot"
        | "getMinimumBalanceForRentExemption"
        | "minimumLedgerSlot"
        | "getFirstAvailableBlock" => value.is_u64(),
        "getBlockTime" => value.is_i64(),
        "getFeeForMessage" => value["value"].is_null() || value["value"].is_u64(),
        "getLatestBlockhash" => {
            value["value"]["blockhash"]
                .as_str()
                .is_some_and(|hash| crate::parse_address(hash).is_ok())
                && value["value"]["lastValidBlockHeight"].is_u64()
        }
        "isBlockhashValid" => value["value"].is_boolean(),
        "getAccountInfo" => value
            .get("value")
            .is_some_and(|v| v.is_null() || v.is_object()),
        "getMultipleAccounts" => value["value"].as_array().is_some_and(|items| {
            params[0]
                .as_array()
                .is_some_and(|keys| keys.len() == items.len())
                && items.iter().all(|item| item.is_null() || item.is_object())
        }),
        "getTokenAccountsByOwner" | "getTokenAccountsByDelegate" => value["value"].is_array(),
        "getSignatureStatuses" => value["value"].as_array().is_some_and(|items| {
            params[0]
                .as_array()
                .is_some_and(|signatures| signatures.len() == items.len())
                && items.iter().all(|item| {
                    item.is_null()
                        || (item["slot"].is_u64()
                            && item.get("err").is_some()
                            && item["confirmationStatus"].as_str().is_some_and(|status| {
                                matches!(status, "processed" | "confirmed" | "finalized")
                            }))
                })
        }),
        "getSignaturesForAddress" => value.as_array().is_some_and(|items| {
            items.len() <= params[1]["limit"].as_u64().unwrap_or(1000).min(1000) as usize
                && items.iter().all(|item| {
                    item["signature"]
                        .as_str()
                        .is_some_and(|signature| signature.len() <= 88)
                        && item["slot"].is_u64()
                        && item.get("err").is_some()
                })
        }),
        "getTransaction" => {
            let encoded = value["transaction"][0].as_str().ok_or_else(data_invalid)?;
            let tx = crate::transaction::decode_supported_transaction(encoded)
                .map_err(|_| data_invalid())?;
            value["slot"].is_u64()
                && value["transaction"][1] == "base64"
                && tx.signatures.first().map(ToString::to_string).as_deref() == params[0].as_str()
                && value["meta"].is_object()
                && value["meta"].get("err").is_some()
                && value["meta"]["fee"].is_u64()
                && value["meta"]["preBalances"].is_array()
                && value["meta"]["postBalances"].is_array()
        }
        "simulateTransaction" => value["value"].is_object() && value["value"].get("err").is_some(),
        _ => true,
    };
    if valid { Ok(()) } else { Err(data_invalid()) }
}

fn responses_conflict(method: &str, values: &[&Value]) -> bool {
    if method == "getSignatureStatuses" {
        for (i, left) in values.iter().enumerate() {
            for right in values.iter().skip(i + 1) {
                if let (Some(left), Some(right)) =
                    (left["value"].as_array(), right["value"].as_array())
                {
                    for (left, right) in left.iter().zip(right) {
                        if left["confirmationStatus"] == "finalized"
                            && right["confirmationStatus"] == "finalized"
                            && (left["slot"] != right["slot"] || left["err"] != right["err"])
                        {
                            return true;
                        }
                    }
                }
            }
        }
    }
    if method == "getTransaction" {
        let mut previous: Option<&Value> = None;
        for value in values.iter().filter(|value| !value.is_null()) {
            if previous.is_some_and(|old| {
                old["slot"] != value["slot"]
                    || old["transaction"] != value["transaction"]
                    || old["meta"] != value["meta"]
            }) {
                return true;
            }
            previous = Some(value);
        }
    }
    // A simulation's semantic rejection cannot be masked by another provider's
    // success. Return that result for the runtime's existing error validation.
    false
}
fn observation_rank(method: &str, value: &Value) -> u8 {
    if method == "getSignatureStatuses" {
        value["value"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| match item["confirmationStatus"].as_str() {
                Some("finalized") => 3,
                Some("confirmed") => 2,
                Some("processed") => 1,
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    } else {
        u8::from(!value.is_null())
    }
}
fn semantic_rejection(error: &GoFinError) -> bool {
    error.code == "SOLANA_RPC_CONFLICT"
        || error
            .details
            .as_deref()
            .is_some_and(|details| details["semantic_rejection"] == true)
}

struct RpcFailure {
    error: GoFinError,
    retryable: bool,
    rate_limited: bool,
    retry_after: Option<Duration>,
}
impl RpcFailure {
    fn invalid(stage: &'static str, status: Option<u16>, code: Option<i64>) -> Self {
        let mut error = rpc_error(
            "The Solana RPC returned an invalid response.",
            stage,
            status,
            code,
        );
        error.code = "SOLANA_RPC_DATA_INVALID".into();
        Self {
            error,
            retryable: false,
            rate_limited: false,
            retry_after: None,
        }
    }
}
fn retry_after_delay(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let mut values = headers.get_all(reqwest::header::RETRY_AFTER).iter();
    let value = values.next()?.to_str().ok()?.trim_matches([' ', '\t']);
    if values.next().is_some() || value.is_empty() {
        return None;
    }
    let delay = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        Duration::from_secs(value.parse().unwrap_or(u64::MAX))
    } else {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .unwrap_or_default()
    };
    Some(delay.min(MAX_COOLDOWN))
}
fn rpc_error(
    message: &'static str,
    stage: &'static str,
    http_status: Option<u16>,
    rpc_code: Option<i64>,
) -> GoFinError {
    let mut details = json!({"stage":stage});
    if let Some(status) = http_status {
        details["http_status"] = json!(status);
    }
    if let Some(code) = rpc_code {
        details["rpc_code"] = json!(code);
    }
    pool_error("SOLANA_RPC_UNAVAILABLE", message).with_details(details)
}
fn pool_error(code: &'static str, message: &'static str) -> GoFinError {
    failure(
        code,
        "rpc",
        message,
        "Retry the read; reconcile the original signature without repeating an uncertain financial submission.",
    )
}
fn transport_error() -> GoFinError {
    pool_error(
        "SOLANA_RPC_UNAVAILABLE",
        "No eligible Solana RPC supplied usable evidence.",
    )
}
fn timeout_error() -> GoFinError {
    pool_error(
        "SOLANA_RPC_UNAVAILABLE",
        "The bounded Solana RPC evidence deadline elapsed.",
    )
    .with_details(json!({"stage":"transport_timeout"}))
}
fn stale_error() -> GoFinError {
    pool_error(
        "SOLANA_RPC_STALE",
        "The Solana RPC observation is stale or below its required slot.",
    )
}
fn data_invalid() -> GoFinError {
    pool_error(
        "SOLANA_RPC_DATA_INVALID",
        "The Solana RPC returned malformed or incompatible evidence.",
    )
}
fn conflict_error() -> GoFinError {
    pool_error(
        "SOLANA_RPC_CONFLICT",
        "Observed Solana finalized evidence conflicts; the original operation remains unresolved.",
    )
}
fn network_mismatch() -> GoFinError {
    pool_error(
        "SOLANA_NETWORK_MISMATCH",
        "The RPC endpoint is not Solana mainnet-beta.",
    )
}
#[cfg(test)]
fn verify_mainnet_genesis(genesis: &Value) -> Result<()> {
    if genesis.as_str() == Some(MAINNET_GENESIS_HASH) {
        Ok(())
    } else {
        Err(network_mismatch())
    }
}
#[cfg(test)]
fn attempt_timeout_for(method: &str) -> Duration {
    if is_retryable_method(method) {
        ATTEMPT_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    }
}

fn safe_code(code: &str) -> &'static str {
    match code {
        "SOLANA_NETWORK_MISMATCH" => "SOLANA_NETWORK_MISMATCH",
        "SOLANA_RPC_RATE_LIMITED" => "SOLANA_RPC_RATE_LIMITED",
        "SOLANA_RPC_STALE" => "SOLANA_RPC_STALE",
        "SOLANA_RPC_UNHEALTHY" => "SOLANA_RPC_UNHEALTHY",
        "SOLANA_RPC_UNSUPPORTED" => "SOLANA_RPC_UNSUPPORTED",
        "SOLANA_RPC_HISTORY_UNAVAILABLE" => "SOLANA_RPC_HISTORY_UNAVAILABLE",
        "SOLANA_RPC_DATA_INVALID" => "SOLANA_RPC_DATA_INVALID",
        "SOLANA_RPC_CONFLICT" => "SOLANA_RPC_CONFLICT",
        _ => "SOLANA_RPC_UNAVAILABLE",
    }
}
fn reason(error: &GoFinError) -> &'static str {
    if error
        .details
        .as_deref()
        .is_some_and(|d| d["stage"] == "transport_timeout")
    {
        return "timeout";
    }
    match error.code.as_str() {
        "SOLANA_NETWORK_MISMATCH" | "SOLANA_RPC_DATA_INVALID" | "SOLANA_RPC_CONFLICT" => {
            "incompatible"
        }
        "SOLANA_RPC_RATE_LIMITED" => "rate_limited",
        "SOLANA_RPC_STALE" => "lagging",
        "SOLANA_RPC_UNSUPPORTED" => "unsupported",
        "SOLANA_RPC_HISTORY_UNAVAILABLE" => "historical_unavailable",
        _ => "unavailable",
    }
}
struct ReadTrace<'a> {
    rpc: &'a HttpRpc,
    method: &'a str,
    attempts: BTreeMap<usize, &'static str>,
    finished: bool,
}
impl<'a> ReadTrace<'a> {
    fn new(rpc: &'a HttpRpc, method: &'a str) -> Self {
        Self {
            rpc,
            method,
            attempts: BTreeMap::new(),
            finished: false,
        }
    }
    fn record(&mut self, index: usize, reason: &'static str) {
        self.attempts.insert(index, reason);
    }
    fn finish(&mut self, outcome: &str, selected: Option<usize>) {
        if let Ok(mut state) = self.rpc.selection.lock() {
            let attempts=self.attempts.iter().map(|(index,reason)|json!({"endpoint_id":format!("rpc-{}",index+1),"reason":reason})).collect::<Vec<_>>();
            let observation = json!({"method":self.method,"outcome":outcome,"selected_endpoint":selected.map(|index|format!("rpc-{}",index+1)),"attempts":attempts});
            if selected.is_some_and(|index| index != 0) && outcome == "ok" {
                state.last_failover = Some(observation.clone());
            }
            state.last_read = Some(observation);
        }
        self.finished = true;
    }
}
impl Drop for ReadTrace<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("cancelled", None);
        }
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
            | "getTokenSupply"
            | "getTransaction"
            | "getVersion"
            | "getVoteAccounts"
            | "isBlockhashValid"
            | "minimumLedgerSlot"
            | "simulateTransaction"
    )
}

#[cfg(test)]
#[path = "rpc_pool_tests.rs"]
mod pool_tests;
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
        assert_eq!(error.code, "SOLANA_RPC_DATA_INVALID");
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
        assert_eq!(error.code, "SOLANA_RPC_RATE_LIMITED");
        assert!(rpc.cooldown_remaining(0).unwrap() <= Duration::from_secs(21));
        assert_eq!(details["method"], "getSlot");
        assert_eq!(details["attempt"], 1);
        assert!(details["elapsed_ms"].as_u64().is_some());
        assert!(error.message.find("/rpc").is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[test]
    fn only_retryable_methods_use_short_per_attempt_budget() {
        assert_eq!(attempt_timeout_for("getAccountInfo"), ATTEMPT_TIMEOUT);
        assert_eq!(attempt_timeout_for("getMultipleAccounts"), ATTEMPT_TIMEOUT);
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
