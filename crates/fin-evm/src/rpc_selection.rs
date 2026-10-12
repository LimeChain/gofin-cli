//! Read eligibility and bounded, credential-free observations for an RPC pool.
use super::*;

const MAX_HEAD_AGE: Duration = Duration::from_secs(60);
const HEAD_CACHE_AGE: Duration = Duration::from_secs(2);
const CAPABILITY_RECHECK: Duration = Duration::from_secs(30);
const MAX_ANCHORS: usize = 64;

#[derive(Default)]
pub(super) struct SelectionState {
    heads: BTreeMap<usize, (Instant, u64)>,
    anchors: BTreeMap<u64, B256>,
    anchor_order: std::collections::VecDeque<u64>,
    unsupported: BTreeMap<(usize, String), Instant>,
    health: BTreeMap<usize, (&'static str, Option<&'static str>)>,
    last_read: Option<Value>,
    last_failover: Option<Value>,
}

#[cfg(test)]
impl SelectionState {
    pub(super) fn evict_anchors(&mut self) {
        self.anchors.clear();
        self.anchor_order.clear();
    }
}

pub(super) fn missing_evidence() -> FinError {
    error(
        "RPC_EVIDENCE_UNAVAILABLE",
        "rpc",
        "No eligible RPC supplied the required block evidence; the original operation remains unresolved.",
    )
}

fn incompatible() -> RpcAttemptFailure {
    RpcAttemptFailure {
        retryable: true,
        error: error(
            "RPC_INCOMPATIBLE_BLOCK",
            "rpc",
            "RPC block identity differs from the pinned observation; no state from this endpoint was accepted.",
        ),
    }
}

fn failure(code: &str, message: &str) -> RpcAttemptFailure {
    RpcAttemptFailure {
        retryable: true,
        error: error(code, "rpc", message),
    }
}

pub(super) fn safe_code(code: &str) -> &'static str {
    match code {
        "WRONG_CHAIN" => "WRONG_CHAIN",
        "RPC_DATA_INVALID" => "RPC_DATA_INVALID",
        "RPC_READ_TIMEOUT" => "RPC_READ_TIMEOUT",
        "RPC_ENDPOINT_TIMEOUT" => "RPC_ENDPOINT_TIMEOUT",
        "RPC_RATE_LIMITED" => "RPC_RATE_LIMITED",
        "RPC_HISTORICAL_STATE_UNAVAILABLE" => "RPC_HISTORICAL_STATE_UNAVAILABLE",
        "RPC_REQUEST_FAILED" => "RPC_REQUEST_FAILED",
        "RPC_STALE_READ" => "RPC_STALE_READ",
        "RPC_EVIDENCE_UNAVAILABLE" => "RPC_EVIDENCE_UNAVAILABLE",
        "RPC_UNSUPPORTED_CAPABILITY" => "RPC_UNSUPPORTED_CAPABILITY",
        "RPC_INCOMPATIBLE_BLOCK" => "RPC_INCOMPATIBLE_BLOCK",
        _ => "RPC_UNAVAILABLE",
    }
}

pub(super) fn reason(error: &FinError) -> &'static str {
    if error
        .details
        .as_deref()
        .and_then(|d| d.get("http_status"))
        .and_then(Value::as_u64)
        == Some(429)
    {
        return "rate_limited";
    }
    match error.code.as_str() {
        "RPC_READ_TIMEOUT" | "RPC_ENDPOINT_TIMEOUT" => "timeout",
        "RPC_RATE_LIMITED" => "rate_limited",
        "RPC_HISTORICAL_STATE_UNAVAILABLE" => "historical_unavailable",
        "RPC_UNSUPPORTED_CAPABILITY" => "unsupported",
        "RPC_STALE_READ" | "RPC_EVIDENCE_UNAVAILABLE" => "lagging",
        "RPC_INCOMPATIBLE_BLOCK" | "WRONG_CHAIN" | "RPC_DATA_INVALID" => "incompatible",
        _ if endpoint_access_failure(error) => "access_denied",
        _ => "unavailable",
    }
}

fn state_tag_index(method: &str) -> Option<usize> {
    match method {
        "eth_getBalance" | "eth_getCode" | "eth_call" | "eth_getTransactionCount" => Some(1),
        "eth_getStorageAt" | "eth_getProof" => Some(2),
        _ => None,
    }
}

pub(super) fn capability(method: &str, params: &Value) -> String {
    if method == "eth_getBlockByNumber"
        && let Some(tag @ ("finalized" | "safe")) = params.get(0).and_then(Value::as_str)
    {
        return format!("{method}:{tag}");
    }
    if state_tag_index(method)
        .and_then(|i| params.get(i))
        .is_some_and(Value::is_object)
    {
        return format!("{method}:canonical");
    }
    method.to_owned()
}

pub(super) fn unsupported_error(method: &str, params: &Value, details: &Value) -> bool {
    if method == "eth_sendRawTransaction" {
        return false;
    }
    let code = details.get("code").and_then(Value::as_i64);
    if code == Some(-32601) {
        return true;
    }
    let contextual = capability(method, params) != method;
    let message = details
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    contextual
        && code == Some(-32602)
        && !message.contains("revert")
        && [
            "not supported",
            "unsupported",
            "cannot unmarshal object",
            "invalid block tag",
            "unknown block tag",
        ]
        .iter()
        .any(|text| message.contains(text))
}

pub(super) struct ReadTrace<'a> {
    rpc: &'a HttpRpc,
    method: &'a str,
    attempts: Vec<Value>,
    finished: bool,
}
impl<'a> ReadTrace<'a> {
    pub(super) fn new(rpc: &'a HttpRpc, method: &'a str) -> Self {
        Self {
            rpc,
            method,
            attempts: vec![],
            finished: false,
        }
    }
    pub(super) fn attempt(&mut self, index: usize, reason: &str) {
        let id = format!("rpc-{}", index + 1);
        if let Some(last) = self.attempts.last_mut()
            && last["endpoint_id"] == id
        {
            last["reason"] = json!(reason);
        } else {
            self.attempts
                .push(json!({"endpoint_id":id,"reason":reason}));
        }
    }
    pub(super) fn finish(&mut self, outcome: &str, selected: Option<usize>) {
        if self.method != "eth_sendRawTransaction"
            && let Ok(mut state) = self.rpc.selection.lock()
        {
            let observation = json!({"method":self.method,"outcome":outcome,"selected_endpoint":selected.map(|i|format!("rpc-{}",i+1)),"attempts":self.attempts});
            if outcome == "ok"
                && selected.is_some()
                && self.attempts.iter().any(|a| a["reason"] != "ok")
            {
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

impl HttpRpc {
    fn state(
        &self,
    ) -> std::result::Result<std::sync::MutexGuard<'_, SelectionState>, RpcAttemptFailure> {
        self.selection
            .lock()
            .map_err(|_| failure("RPC_UNAVAILABLE", "RPC selection state is unavailable."))
    }

    pub(super) fn record_health(&self, index: usize, error: Option<&FinError>) {
        if let Ok(mut state) = self.selection.lock() {
            let health = error.map_or(("healthy", None), |error| {
                let status = match reason(error) {
                    "rate_limited" => "cooling_down",
                    "lagging" => "lagging",
                    "unsupported" => "unsupported",
                    "incompatible" => "incompatible",
                    _ => "unavailable",
                };
                (status, Some(safe_code(&error.code)))
            });
            state.health.insert(index, health);
        }
    }

    pub(super) fn unsupported_remaining(&self, index: usize, key: &str) -> Option<Duration> {
        let mut state = self.selection.lock().ok()?;
        let entry = (index, key.to_owned());
        let remaining = state
            .unsupported
            .get(&entry)
            .and_then(|until| until.checked_duration_since(Instant::now()));
        if remaining.is_none() {
            state.unsupported.remove(&entry);
        }
        remaining
    }

    pub(super) fn mark_unsupported(&self, index: usize, key: &str) {
        if let Ok(mut state) = self.selection.lock() {
            state
                .unsupported
                .insert((index, key.to_owned()), Instant::now() + CAPABILITY_RECHECK);
        }
    }

    fn observe_header(
        &self,
        index: usize,
        header: &Value,
        fresh: bool,
    ) -> std::result::Result<(), RpcAttemptFailure> {
        let number = rpc_u64(&header["number"]).map_err(|error| RpcAttemptFailure {
            error,
            retryable: false,
        })?;
        let block_hash = hash(header["hash"].as_str().unwrap_or_default()).map_err(|error| {
            RpcAttemptFailure {
                error,
                retryable: false,
            }
        })?;
        if fresh {
            let timestamp = rpc_u64(&header["timestamp"]).map_err(|error| RpcAttemptFailure {
                error,
                retryable: false,
            })?;
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if timestamp > now.saturating_add(30)
                || now.saturating_sub(timestamp) > MAX_HEAD_AGE.as_secs()
            {
                return Err(failure(
                    "RPC_STALE_READ",
                    "RPC latest block is stale or future-dated; trying another eligible endpoint.",
                ));
            }
        }
        let mut state = self.state()?;
        if state
            .anchors
            .get(&number)
            .is_some_and(|expected| *expected != block_hash)
        {
            return Err(incompatible());
        }
        if fresh
            && state
                .heads
                .get(&index)
                .is_some_and(|(_, previous)| number < *previous)
        {
            return Err(failure(
                "RPC_STALE_READ",
                "RPC head regressed below its observed height; trying another eligible endpoint.",
            ));
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = state.anchors.entry(number) {
            entry.insert(block_hash);
            state.anchor_order.push_back(number);
            if state.anchor_order.len() > MAX_ANCHORS
                && let Some(old) = state.anchor_order.pop_front()
            {
                state.anchors.remove(&old);
            }
        }
        if fresh {
            state.heads.insert(index, (Instant::now(), number));
        }
        Ok(())
    }

    async fn fresh_head(
        &self,
        index: usize,
        url: &str,
    ) -> std::result::Result<u64, RpcAttemptFailure> {
        if let Some(height) = self
            .state()?
            .heads
            .get(&index)
            .filter(|(at, _)| at.elapsed() < HEAD_CACHE_AGE)
            .map(|(_, height)| *height)
        {
            return Ok(height);
        }
        let header = self
            .at(
                url,
                "eth_getBlockByNumber",
                json!(["latest", false]),
                MAX_READ_RETRIES_PER_ENDPOINT,
                false,
            )
            .await?;
        self.observe_header(index, &header, true)?;
        rpc_u64(&header["number"]).map_err(|error| RpcAttemptFailure {
            error,
            retryable: false,
        })
    }

    async fn verify_numeric_anchor(
        &self,
        url: &str,
        height: u64,
        expected: B256,
    ) -> std::result::Result<(), RpcAttemptFailure> {
        let header = self
            .at(
                url,
                "eth_getBlockByNumber",
                json!([format!("0x{height:x}"), false]),
                MAX_READ_RETRIES_PER_ENDPOINT,
                false,
            )
            .await?;
        if header.is_null() {
            return Err(failure(
                "RPC_EVIDENCE_UNAVAILABLE",
                "RPC cannot observe the pinned block.",
            ));
        }
        if rpc_u64(&header["number"]).ok() != Some(height)
            || hash(header["hash"].as_str().unwrap_or_default()).ok() != Some(expected)
        {
            return Err(incompatible());
        }
        Ok(())
    }

    pub(super) fn cached_anchor(&self, method: &str, params: &Value) -> Option<(B256, u64)> {
        let height = state_tag_index(method)
            .and_then(|i| params.get(i))
            .and_then(Value::as_str)
            .and_then(|s| s.strip_prefix("0x"))
            .and_then(|s| u64::from_str_radix(s, 16).ok())?;
        self.selection
            .lock()
            .ok()?
            .anchors
            .get(&height)
            .copied()
            .map(|hash| (hash, height))
    }

    pub(super) async fn eligible_read(
        &self,
        index: usize,
        url: &str,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
        request_anchor: &Mutex<Option<(B256, u64)>>,
    ) -> std::result::Result<Value, RpcAttemptFailure> {
        let tag_index = state_tag_index(method);
        let numeric_height = tag_index
            .and_then(|i| params.get(i))
            .and_then(Value::as_str)
            .filter(|s| s.starts_with("0x"))
            .and_then(|s| u64::from_str_radix(&s[2..], 16).ok());
        let mut anchor = *request_anchor
            .lock()
            .map_err(|_| failure("RPC_UNAVAILABLE", "Pinned request identity is unavailable."))?;
        if let Some(height) = requirements.pin_block {
            if numeric_height != Some(height) {
                return Err(incompatible());
            }
            if anchor.is_none() {
                let header = self
                    .at(
                        url,
                        "eth_getBlockByNumber",
                        json!([format!("0x{height:x}"), false]),
                        MAX_READ_RETRIES_PER_ENDPOINT,
                        false,
                    )
                    .await?;
                if header.is_null() {
                    return Err(failure(
                        "RPC_EVIDENCE_UNAVAILABLE",
                        "RPC cannot bind the requested historical state.",
                    ));
                }
                if rpc_u64(&header["number"]).ok() != Some(height) {
                    return Err(incompatible());
                }
                self.observe_header(index, &header, false)?;
                let block_hash =
                    hash(header["hash"].as_str().unwrap_or_default()).map_err(|error| {
                        RpcAttemptFailure {
                            error,
                            retryable: false,
                        }
                    })?;
                anchor = Some((block_hash, height));
                *request_anchor.lock().map_err(|_| {
                    failure("RPC_UNAVAILABLE", "Pinned request identity is unavailable.")
                })? = anchor;
            }
        }
        let mut effective = params.clone();
        if let Some((block_hash, _)) = anchor {
            effective[tag_index.unwrap()] =
                json!({"blockHash":format!("{block_hash:#x}"),"requireCanonical":true});
        }
        let key = capability(method, &effective);
        let unsupported_canonical =
            anchor.is_some() && self.unsupported_remaining(index, &key).is_some();
        let mut result = if unsupported_canonical {
            Err(failure(
                "RPC_UNSUPPORTED_CAPABILITY",
                "RPC does not support canonical hash state reads.",
            ))
        } else {
            self.at(
                url,
                method,
                effective.clone(),
                MAX_READ_RETRIES_PER_ENDPOINT,
                is_historical_state_read(method, &effective),
            )
            .await
        };
        if let Some((block_hash, height)) = anchor
            && result
                .as_ref()
                .err()
                .is_some_and(|failure| failure.error.code == "RPC_UNSUPPORTED_CAPABILITY")
        {
            self.mark_unsupported(index, &key);
            // A numeric-only provider must prove the SAME block before and after
            // the state read. The verification never consults a different URL.
            self.verify_numeric_anchor(url, height, block_hash).await?;
            result = self
                .at(
                    url,
                    method,
                    params.clone(),
                    MAX_READ_RETRIES_PER_ENDPOINT,
                    true,
                )
                .await;
            if result.is_ok() {
                self.verify_numeric_anchor(url, height, block_hash).await?;
            }
        }
        let value = result?;
        if method == "eth_getBlockByNumber" && !value.is_null() {
            if let Some(requested) = params
                .get(0)
                .and_then(Value::as_str)
                .filter(|s| s.starts_with("0x"))
                && rpc_u64(&value["number"]).ok() != u64::from_str_radix(&requested[2..], 16).ok()
            {
                return Err(incompatible());
            }
            self.observe_header(index, &value, params.get(0) == Some(&json!("latest")))?;
        }
        let latest_state = tag_index
            .and_then(|i| params.get(i))
            .and_then(Value::as_str)
            .is_some_and(|tag| matches!(tag, "latest" | "pending"));
        if method == "eth_blockNumber" || latest_state {
            let height = self.fresh_head(index, url).await?;
            if method == "eth_blockNumber" {
                let reported = rpc_u64(&value).map_err(|error| RpcAttemptFailure {
                    error,
                    retryable: false,
                })?;
                // A rollup can legitimately produce blocks between the number
                // and header reads. Compare a small network-aware race margin,
                // then anchor the actual reported height on THIS endpoint.
                let race_margin = if matches!(self.expected_chain_id, 8453 | 42161 | 4663) {
                    16
                } else {
                    2
                };
                if reported.saturating_add(race_margin) < height {
                    return Err(failure(
                        "RPC_STALE_READ",
                        "RPC block number trails its observed fresh head.",
                    ));
                }
                if reported != height {
                    let observed = self
                        .at(
                            url,
                            "eth_getBlockByNumber",
                            json!([format!("0x{reported:x}"), false]),
                            MAX_READ_RETRIES_PER_ENDPOINT,
                            false,
                        )
                        .await?;
                    if observed.is_null() {
                        return Err(failure(
                            "RPC_EVIDENCE_UNAVAILABLE",
                            "RPC cannot anchor its reported head.",
                        ));
                    }
                    if rpc_u64(&observed["number"]).ok() != Some(reported) {
                        return Err(incompatible());
                    }
                    self.observe_header(index, &observed, false)?;
                }
            }
        }
        requirements
            .validate(method, &value)
            .map_err(|error| RpcAttemptFailure {
                retryable: error.code == "RPC_EVIDENCE_UNAVAILABLE",
                error,
            })?;
        Ok(value)
    }

    pub(super) fn selection_diagnostics(&self) -> Value {
        let Ok(state) = self.selection.lock() else {
            return Value::Null;
        };
        let verified = self.verified.lock().ok();
        let endpoints = self.urls.iter().enumerate().map(|(index,url)| {
            let (mut status,code)=state.health.get(&index).copied().unwrap_or(("unobserved",None));
            let remaining=self.read_cooldown_remaining(url).ok().flatten();
            if remaining.is_some() { status="cooling_down"; } else if status=="cooling_down" { status="unobserved"; }
            let unsupported=state.unsupported.iter().filter(|((i,_),until)|*i==index && **until>Instant::now()).map(|((_,key),_)|key.clone()).collect::<Vec<_>>();
            json!({"endpoint_id":format!("rpc-{}",index+1),"identity_verified":verified.as_ref().is_some_and(|v|v.contains(url)),"status":status,"last_error_code":code,"retry_after_ms":remaining.map(|d|d.as_nanos().div_ceil(1_000_000)),"unsupported_methods":unsupported})
        }).collect::<Vec<_>>();
        json!({"chain_id":self.expected_chain_id,"read_timeout_ms":self.read_timeout.as_millis(),"endpoints":endpoints,"last_read":state.last_read,"last_failover":state.last_failover})
    }
}
