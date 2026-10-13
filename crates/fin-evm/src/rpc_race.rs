//! At most two concurrent, validated read attempts; dropping the pool cancels them.
use super::*;
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    task::Poll,
};

type ReadFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<Value, RpcAttemptFailure>> + Send + 'a>>;
struct Attempt<'a> {
    index: usize,
    future: Option<ReadFuture<'a>>,
}

fn can_try_another(rejection: &RpcAttemptFailure) -> bool {
    rejection.retryable
        || endpoint_access_failure(&rejection.error)
        || matches!(
            rejection.error.code.as_str(),
            "RPC_UNSUPPORTED_CAPABILITY" | "RPC_DATA_INVALID" | "WRONG_CHAIN"
        )
}

impl HttpRpc {
    pub(super) async fn raced_read(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + self.read_timeout;
        let anchor = Mutex::new(self.cached_anchor(method, &params));
        let mut trace = ReadTrace::new(self, method);
        let mut active = BTreeSet::new();
        let attempt = async {
            let key = rpc_selection::capability(method, &params);
            let mut eligible = Vec::new();
            let mut retry_after: Option<Duration> = None;
            let mut blocked = false;
            for (index, url) in self.urls.iter().enumerate() {
                if let Some(remaining) = self.read_cooldown_remaining(url)? {
                    retry_after = Some(retry_after.map_or(remaining, |old| old.min(remaining)));
                    trace.attempt(index, "rate_limited");
                } else if self.unsupported_remaining(index, &key).is_some() {
                    blocked = true;
                    trace.attempt(index, "unsupported");
                } else {
                    eligible.push((index, url));
                }
            }
            if eligible.is_empty() {
                return Err(if blocked {
                    error(
                        "RPC_UNSUPPORTED_CAPABILITY",
                        "rpc",
                        "Configured read endpoints do not support this capability yet; retry after capability recheck.",
                    )
                } else if let Some(remaining) = retry_after {
                    FinError::new("RPC_RATE_LIMITED", "rpc", "All configured read RPC endpoints are temporarily cooling down after rate limits.", "Retry this read after the reported cooldown.")
                        .with_details(json!({"chain_id":self.expected_chain_id,"retry_after_ms":remaining.as_nanos().div_ceil(1_000_000)}))
                } else {
                    error(
                        "RPC_UNAVAILABLE",
                        "rpc",
                        "No eligible RPC endpoint is available.",
                    )
                });
            }
            let mut next = 0;
            let mut pending = Vec::<Attempt<'_>>::new();
            let mut missing = false;
            let mut failure = error(
                "RPC_UNAVAILABLE",
                "rpc",
                "No configured RPC returned a usable response.",
            );
            loop {
                while pending.len() < 2 && next < eligible.len() {
                    let (index, url) = eligible[next];
                    // Keep a later provider reachable if both initial providers
                    // stall. With two providers each can use the full deadline.
                    let waves = (eligible.len() - next + pending.len()).div_ceil(2);
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    let budget = remaining / u32::try_from(waves).unwrap_or(u32::MAX);
                    let request = params.clone();
                    let anchor = &anchor;
                    let future = async move {
                        let read = async {
                            self.ensure_endpoint(url).await?;
                            self.eligible_read(index, url, method, request, requirements, anchor)
                                .await
                        };
                        if waves == 1 {
                            return read.await;
                        }
                        tokio::time::timeout(budget, read).await.unwrap_or_else(|_| Err(RpcAttemptFailure {
                            retryable: true,
                            error: error("RPC_ENDPOINT_TIMEOUT", "rpc", "RPC endpoint exceeded its allotted read time; trying another eligible endpoint.")
                                .with_details(json!({"chain_id":self.expected_chain_id,"method":method,"timeout_ms":budget.as_nanos().div_ceil(1_000_000)})),
                        }))
                    };
                    active.insert(index);
                    trace.attempt(index, "cancelled");
                    pending.push(Attempt {
                        index,
                        future: Some(Box::pin(future)),
                    });
                    next += 1;
                }
                if pending.is_empty() {
                    break;
                }
                // Poll both attempts before choosing a completed one. No spawned
                // task outlives this read, its deadline or caller cancellation.
                let completed = poll_fn(|context| {
                    let mut completed = Vec::new();
                    for attempt in &mut pending {
                        if let Some(future) = &mut attempt.future
                            && let Poll::Ready(result) = future.as_mut().poll(context)
                        {
                            attempt.future = None;
                            completed.push((attempt.index, result));
                        }
                    }
                    if completed.is_empty() {
                        Poll::Pending
                    } else {
                        Poll::Ready(completed)
                    }
                })
                .await;
                // Record the whole completed batch before picking a winner.
                // A semantic rejection already observed in the same poll must
                // not be hidden by another ready result's configured order.
                for (index, result) in &completed {
                    active.remove(index);
                    match result {
                        Ok(value) => {
                            self.record_health(*index, None);
                            trace.attempt(*index, if value.is_null() { "missing" } else { "ok" });
                        }
                        Err(rejection) => {
                            self.record_health(*index, Some(&rejection.error));
                            trace.attempt(*index, rpc_selection::reason(&rejection.error));
                            if rejection.error.code == "RPC_UNSUPPORTED_CAPABILITY" {
                                self.mark_unsupported(*index, &key);
                            }
                        }
                    }
                }
                if let Some((_, Err(rejection))) = completed.iter().find(|(_, result)| {
                    result
                        .as_ref()
                        .is_err_and(|rejection| !can_try_another(rejection))
                }) {
                    return Err(rejection.error.clone());
                }
                for (index, result) in completed {
                    pending.retain(|attempt| attempt.index != index);
                    match result {
                        Ok(value)
                            if value.is_null()
                                && matches!(
                                    method,
                                    "eth_getTransactionReceipt"
                                        | "eth_getTransactionByHash"
                                        | "eth_getBlockByNumber"
                                ) =>
                        {
                            missing = true;
                        }
                        Ok(value) => {
                            return Ok((value, Some(index)));
                        }
                        Err(rejection) => {
                            failure = rejection.error;
                        }
                    }
                }
            }
            if missing {
                Ok((Value::Null, None))
            } else {
                Err(failure)
            }
        };
        let result = match tokio::time::timeout_at(deadline, attempt).await {
            Ok(result) => result,
            Err(_) => {
                let failure = self.read_timeout_error(method);
                for index in active {
                    self.record_health(index, Some(&failure));
                    trace.attempt(index, "timeout");
                }
                Err(failure)
            }
        };
        match result {
            Ok((value, selected)) => {
                trace.finish(if selected.is_some() { "ok" } else { "missing" }, selected);
                Ok(value)
            }
            Err(failure) => {
                trace.finish("error", None);
                Err(failure)
            }
        }
    }
}
