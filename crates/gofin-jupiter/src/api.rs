//! Bounded Jupiter HTTP access. Keyless operation is the default; a protected
//! key can be supplied by the broker without ever being read from the CLI.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::{Client, Method, header::HeaderValue, redirect::Policy};
use serde_json::Value;
use tokio::{sync::Mutex, time::Instant};
use zeroize::Zeroizing;

use gofin_domain::{GoFinError, Result};
use gofin_solana::transaction::AsyncDispatchGuard;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JupiterHost {
    Platform,
    Perps,
}

impl JupiterHost {
    fn base_url(self) -> &'static str {
        match self {
            Self::Platform => "https://api.jup.ag",
            Self::Perps => "https://perps-api.jup.ag/v1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateBucket {
    Main,
    Execute,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestPriority {
    Normal,
    Financial,
    Recovery,
}

#[async_trait]
pub trait JupiterApi: Send + Sync {
    /// The maximum wait must be supplied by the caller and derived from the
    /// current read/approval/consent deadline. No unbounded retry is allowed.
    // Keep the transport's authorization deadline and dispatch guard explicit
    // at every call site; bundling these into a generic request object obscures
    // which fields bound financial writes.
    #[allow(clippy::too_many_arguments)]
    async fn request(
        &self,
        host: JupiterHost,
        method: Method,
        path_and_query: &str,
        body: Option<Value>,
        bucket: RateBucket,
        priority: RequestPriority,
        max_wait: Duration,
    ) -> Result<Value>;

    /// Checked dispatch variant. HTTP implementations must run `before_send`
    /// after any rate-limit wait and directly before handing bytes to the
    /// transport; test clients retain a conservative default.
    // See `request`: each argument is part of the no-replay/deadline contract.
    #[allow(clippy::too_many_arguments)]
    async fn request_checked(
        &self,
        host: JupiterHost,
        method: Method,
        path_and_query: &str,
        body: Option<Value>,
        bucket: RateBucket,
        priority: RequestPriority,
        max_wait: Duration,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
        dispatch_guard: Option<&dyn AsyncDispatchGuard>,
    ) -> Result<Value> {
        if let Some(guard) = dispatch_guard {
            guard.check().await?;
        }
        before_send()?;
        self.request(
            host,
            method,
            path_and_query,
            body,
            bucket,
            priority,
            max_wait,
        )
        .await
    }
}

/// Fixed-host, redirect-disabled API client with bounded bodies and a shared
/// process-local sliding-window limiter. API keys are optional and redacted.
pub struct HttpJupiterApi {
    client: Client,
    api_key: Option<Zeroizing<String>>,
    main_calls: Mutex<VecDeque<Instant>>,
    execute_calls: Mutex<VecDeque<Instant>>,
    main_server_limit: Mutex<Option<ServerLimit>>,
    execute_server_limit: Mutex<Option<ServerLimit>>,
    main_retry_not_before: Mutex<Option<Instant>>,
    execute_retry_not_before: Mutex<Option<Instant>>,
}

#[derive(Clone, Copy, Debug)]
struct ServerLimit {
    current: usize,
    remaining: usize,
    limit: usize,
    reset_epoch: u64,
}

impl std::fmt::Debug for HttpJupiterApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpJupiterApi")
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl HttpJupiterApi {
    pub fn new(api_key: Option<Zeroizing<String>>) -> Result<Arc<Self>> {
        if api_key.as_ref().is_some_and(|key| !valid_api_key(key)) {
            return Err(GoFinError::validation(
                "The optional Jupiter API key has an invalid format.",
            ));
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| api_error("JUPITER_TRANSPORT", "Jupiter could not be reached."))?;
        Ok(Arc::new(Self {
            client,
            api_key,
            main_calls: Mutex::new(VecDeque::new()),
            execute_calls: Mutex::new(VecDeque::new()),
            main_server_limit: Mutex::new(None),
            execute_server_limit: Mutex::new(None),
            main_retry_not_before: Mutex::new(None),
            execute_retry_not_before: Mutex::new(None),
        }))
    }

    async fn acquire(
        &self,
        bucket: RateBucket,
        priority: RequestPriority,
        deadline: Instant,
    ) -> Result<()> {
        let (queue, cap, window, remote, retry_not_before) = match bucket {
            RateBucket::Main => (
                &self.main_calls,
                30usize,
                Duration::from_secs(60),
                &self.main_server_limit,
                &self.main_retry_not_before,
            ),
            RateBucket::Execute => (
                &self.execute_calls,
                20usize,
                Duration::from_secs(1),
                &self.execute_server_limit,
                &self.execute_retry_not_before,
            ),
        };
        loop {
            if Instant::now() >= deadline {
                return Err(api_error(
                    "JUPITER_RATE_LIMIT_WAIT",
                    "Jupiter rate limits would exceed the operation's remaining time.",
                ));
            }
            let delay = {
                let retry_gate = {
                    let mut retry_gate = retry_not_before.lock().await;
                    let current = *retry_gate;
                    if current.is_some_and(|gate| gate <= Instant::now()) {
                        *retry_gate = None;
                    }
                    current
                };
                if let Some(retry_gate) = retry_gate {
                    let now = Instant::now();
                    if retry_gate > now {
                        let delay = retry_gate.duration_since(now);
                        if delay > deadline.saturating_duration_since(now) {
                            return Err(api_error(
                                "JUPITER_RATE_LIMIT_WAIT",
                                "Jupiter rate limits would exceed the operation's remaining time.",
                            ));
                        }
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                }
                let remote_state = { *remote.lock().await };
                if let Some(remote) = remote_state
                    && (remote.remaining == 0 || remote.current >= remote.limit)
                {
                    let now_epoch = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if remote.reset_epoch > now_epoch {
                        let delay = Duration::from_secs(remote.reset_epoch - now_epoch);
                        if delay > deadline.saturating_duration_since(Instant::now()) {
                            return Err(api_error(
                                "JUPITER_RATE_LIMIT_WAIT",
                                "Jupiter rate limits would exceed the operation's remaining time.",
                            ));
                        }
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                }
                let mut calls = queue.lock().await;
                let now = Instant::now();
                while calls
                    .front()
                    .is_some_and(|time| now.duration_since(*time) >= window)
                {
                    calls.pop_front();
                }
                let effective_cap = match priority {
                    RequestPriority::Normal => cap.saturating_sub(1),
                    RequestPriority::Financial | RequestPriority::Recovery => cap,
                };
                if calls.len() < effective_cap {
                    calls.push_back(now);
                    return Ok(());
                }
                window.saturating_sub(
                    now.duration_since(*calls.front().expect("full queue has an entry")),
                )
            };
            if Instant::now() + delay > deadline {
                return Err(api_error(
                    "JUPITER_RATE_LIMIT_WAIT",
                    "Jupiter rate limits would exceed the operation's remaining time.",
                ));
            }
            tokio::time::sleep(delay).await;
        }
    }
}

#[async_trait]
impl JupiterApi for HttpJupiterApi {
    async fn request(
        &self,
        host: JupiterHost,
        method: Method,
        path_and_query: &str,
        body: Option<Value>,
        bucket: RateBucket,
        priority: RequestPriority,
        max_wait: Duration,
    ) -> Result<Value> {
        self.request_checked(
            host,
            method,
            path_and_query,
            body,
            bucket,
            priority,
            max_wait,
            &|| Ok(()),
            None,
        )
        .await
    }

    async fn request_checked(
        &self,
        host: JupiterHost,
        method: Method,
        path_and_query: &str,
        body: Option<Value>,
        bucket: RateBucket,
        priority: RequestPriority,
        max_wait: Duration,
        before_send: &(dyn Fn() -> Result<()> + Send + Sync),
        dispatch_guard: Option<&dyn AsyncDispatchGuard>,
    ) -> Result<Value> {
        if !valid_path(path_and_query) {
            return Err(GoFinError::validation("The Jupiter API path is invalid."));
        }
        let required_bucket =
            if host == JupiterHost::Platform && path_and_query == "/swap/v2/execute" {
                RateBucket::Execute
            } else {
                RateBucket::Main
            };
        if bucket != required_bucket {
            return Err(GoFinError::validation(
                "The selected Jupiter rate bucket is not permitted for this endpoint.",
            ));
        }
        let deadline = Instant::now() + max_wait.min(Duration::from_secs(30));
        let mut backoff = Duration::from_millis(150);
        loop {
            if Instant::now() >= deadline {
                return Err(api_error(
                    "JUPITER_RATE_LIMIT_WAIT",
                    "The operation's remaining time expired before a Jupiter request could be sent.",
                ));
            }
            self.acquire(bucket, priority, deadline).await?;
            let url = format!("{}{path_and_query}", host.base_url());
            let mut request = self.client.request(method.clone(), url);
            if host == JupiterHost::Platform
                && let Some(key) = &self.api_key
            {
                let value = HeaderValue::from_str(key.as_str()).map_err(|_| {
                    api_error(
                        "JUPITER_AUTH_CONFIG",
                        "The protected Jupiter API key could not be used.",
                    )
                })?;
                request = request.header("x-api-key", value);
            }
            if let Some(value) = body.clone() {
                request = request.json(&value);
            }
            if let Some(guard) = dispatch_guard {
                guard.check().await?;
            }
            if Instant::now() >= deadline {
                return Err(api_error(
                    "JUPITER_DEADLINE_EXPIRED",
                    "The operation's remaining time expired before the request was sent.",
                ));
            }
            before_send()?;
            if Instant::now() >= deadline {
                return Err(api_error(
                    "JUPITER_DEADLINE_EXPIRED",
                    "The operation's remaining time expired before the request was sent.",
                ));
            }
            let response = match tokio::time::timeout_at(deadline, request.send()).await {
                Err(_) => {
                    return Err(api_error(
                        "JUPITER_DEADLINE_EXPIRED",
                        "The Jupiter request exceeded the operation's remaining time.",
                    ));
                }
                Ok(Ok(response)) => response,
                Ok(Err(_)) if method == Method::GET && Instant::now() + backoff <= deadline => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                    continue;
                }
                Ok(Err(_)) => return Err(uncertain_or_transport(&method)),
            };
            self.update_rate_headers(bucket, response.headers()).await;
            let response = match response.status().as_u16() {
                429 | 500..=599 => {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                        .and_then(parse_retry_after)
                        .unwrap_or(backoff);
                    self.set_server_retry_gate(bucket, retry_after).await;
                    // Any non-read request may have reached application processing;
                    // never replay it automatically, including after 429/5xx.
                    if method != Method::GET {
                        return Err(uncertain_or_transport(&method));
                    }
                    if Instant::now() + backoff > deadline {
                        return Err(api_error(
                            "JUPITER_TEMPORARY_FAILURE",
                            "Jupiter remained unavailable within the allowed wait.",
                        ));
                    }
                    if retry_after > deadline.saturating_duration_since(Instant::now()) {
                        return Err(api_error(
                            "JUPITER_TEMPORARY_FAILURE",
                            "Jupiter remained unavailable within the allowed wait.",
                        ));
                    }
                    tokio::time::sleep(retry_after).await;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                    continue;
                }
                _ => response,
            };
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let sent_key = if host == JupiterHost::Platform {
                    self.api_key.as_ref().map(|key| key.as_str())
                } else {
                    None
                };
                let header_code = response
                    .headers()
                    .get("x-error-code")
                    .and_then(|v| v.to_str().ok())
                    .filter(|s| safe_provider_code(s, sent_key))
                    .map(str::to_owned);
                let mut bytes = Vec::new();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
                    .await
                    .map_err(|_| {
                        api_error(
                            "JUPITER_DEADLINE_EXPIRED",
                            "Jupiter returned an error after the operation deadline.",
                        )
                    })?
                {
                    let chunk = chunk.map_err(|_| {
                        api_error(
                            "JUPITER_TRANSPORT",
                            "The Jupiter error response could not be read.",
                        )
                    })?;
                    if bytes.len().saturating_add(chunk.len()) > 4096 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let provider_code = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|value| {
                        ["errorCode", "error_code", "code", "error"]
                            .into_iter()
                            .find_map(|field| {
                                value
                                    .get(field)
                                    .and_then(Value::as_str)
                                    .filter(|s| safe_provider_code(s, sent_key))
                                    .map(str::to_owned)
                            })
                    })
                    .or(header_code);
                let access_mode = if sent_key.is_some() {
                    "protected_api_key"
                } else {
                    "keyless"
                };
                return Err(GoFinError::new(
                    "JUPITER_REQUEST_REJECTED",
                    "jupiter_api",
                    "Jupiter rejected the request.",
                    "Use the sanitized HTTP status and provider code to correct the request or review API access; do not repeat an uncertain write.",
                ).with_details(serde_json::json!({"http_status":status,"provider_error_code":provider_code,"credential_mode":access_mode})));
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
            {
                return Err(api_error(
                    "JUPITER_RESPONSE_INVALID",
                    "Jupiter returned an oversized response.",
                ));
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            loop {
                let next = tokio::time::timeout_at(deadline, stream.next())
                    .await
                    .map_err(|_| {
                        api_error(
                            "JUPITER_DEADLINE_EXPIRED",
                            "The Jupiter response exceeded the operation's remaining time.",
                        )
                    })?;
                let Some(chunk) = next else {
                    break;
                };
                let chunk = chunk.map_err(|_| {
                    api_error(
                        "JUPITER_TRANSPORT",
                        "The Jupiter response could not be read.",
                    )
                })?;
                if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(api_error(
                        "JUPITER_RESPONSE_INVALID",
                        "Jupiter returned an oversized response.",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            return serde_json::from_slice(&bytes).map_err(|_| {
                api_error("JUPITER_RESPONSE_INVALID", "Jupiter returned invalid JSON.")
            });
        }
    }
}

impl HttpJupiterApi {
    async fn set_server_retry_gate(&self, bucket: RateBucket, retry_after: Duration) {
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let requested_deadline = Instant::now()
            .checked_add(retry_after.max(Duration::from_secs(1)))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(60 * 60 * 24 * 365 * 100));
        let (state, retry_gate, limit) = match bucket {
            RateBucket::Main => (
                &self.main_server_limit,
                &self.main_retry_not_before,
                30usize,
            ),
            RateBucket::Execute => (
                &self.execute_server_limit,
                &self.execute_retry_not_before,
                20usize,
            ),
        };
        let mut gate = retry_gate.lock().await;
        *gate = Some(gate.map_or(requested_deadline, |active| active.max(requested_deadline)));
        drop(gate);
        let reset_epoch = now_epoch.saturating_add(retry_after.as_secs().max(1));
        let mut state = state.lock().await;
        let reset_epoch = state
            .map(|active| active.reset_epoch.max(reset_epoch))
            .unwrap_or(reset_epoch);
        *state = Some(ServerLimit {
            current: limit,
            remaining: 0,
            limit,
            reset_epoch,
        });
    }

    async fn update_rate_headers(&self, bucket: RateBucket, headers: &reqwest::header::HeaderMap) {
        let current = headers
            .get("x-ratelimit-current")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .map(|v| v.max(0) as usize);
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .map(|v| v.max(0) as usize);
        let reset_epoch = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let limit = headers
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .map(|v| v as usize);
        if let (Some(remaining), Some(reset_epoch)) = (remaining, reset_epoch) {
            let default_limit = match bucket {
                RateBucket::Main => 30usize,
                RateBucket::Execute => 20usize,
            };
            let current = current.unwrap_or(0);
            let limit = limit.unwrap_or_else(|| {
                if current > 0 {
                    current.saturating_add(remaining)
                } else {
                    remaining.max(default_limit)
                }
            });
            let state = match bucket {
                RateBucket::Main => &self.main_server_limit,
                RateBucket::Execute => &self.execute_server_limit,
            };
            let mut state = state.lock().await;
            let incoming = ServerLimit {
                current,
                remaining,
                limit,
                reset_epoch,
            };
            // Responses can complete out of order. Preserve a stricter active
            // retry gate until its later reset rather than letting an older
            // successful response reopen the shared bucket.
            if !state.is_some_and(|active| {
                active.remaining == 0 && active.reset_epoch >= incoming.reset_epoch
            }) {
                *state = Some(incoming);
            }
        }
    }
}

pub fn valid_api_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 1024 && key.bytes().all(|b| (b'!'..=b'~').contains(&b))
}

fn safe_provider_code(value: &str, sent_key: Option<&str>) -> bool {
    value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !sent_key.is_some_and(|key| value.contains(key))
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let reset = DateTime::parse_from_rfc2822(value).ok()?.timestamp();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(
        u64::try_from(reset).ok()?.saturating_sub(now),
    ))
}

/// Remaining preview window for a sender. Call immediately before requesting
/// network capacity so the queue and complete HTTP response stay inside the
/// immutable prepared-operation expiry.
pub fn remaining_until_deadline(expires_at: &str) -> Result<Duration> {
    let deadline = DateTime::parse_from_rfc3339(expires_at).map_err(|_| {
        GoFinError::validation("The prepared operation has an invalid expiry timestamp.")
    })?;
    deadline
        .signed_duration_since(Utc::now())
        .to_std()
        .map_err(|_| {
            GoFinError::new(
                "JUPITER_PREVIEW_EXPIRED",
                "dispatch",
                "The prepared Jupiter operation expired before dispatch.",
                "Prepare a fresh exact operation and request fresh approval.",
            )
        })
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= 4096
        && !path.bytes().any(|b| b.is_ascii_control() || b == b'#')
}

fn api_error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "jupiter_api",
        message,
        "Retry a read within its deadline. Reconcile an uncertain financial submission before considering another write.",
    )
}

fn uncertain_or_transport(method: &Method) -> GoFinError {
    if *method == Method::GET {
        api_error("JUPITER_TRANSPORT", "The Jupiter API request failed.")
    } else {
        GoFinError::new(
            "JUPITER_SUBMISSION_UNCERTAIN",
            "submission",
            "The Jupiter request outcome is uncertain; GoFin did not retry it.",
            "Reconcile the persisted submission identity before attempting another write.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_untrusted_paths_and_invalid_keys() {
        assert!(!valid_path("//evil.example/path"));
        assert!(!valid_path("/x#fragment"));
        assert!(!valid_api_key("bad key"));
        assert!(valid_api_key("jup_example-123"));
    }

    #[test]
    fn provider_error_codes_never_echo_a_sent_credential() {
        assert!(!safe_provider_code(
            "jup_example-123",
            Some("jup_example-123")
        ));
        assert!(!safe_provider_code(
            "ERR_jup_example-123_denied",
            Some("jup_example-123")
        ));
        assert!(safe_provider_code("RATE_LIMITED", Some("jup_example-123")));
        assert!(safe_provider_code("API_KEY_REQUIRED", None));
    }

    #[test]
    fn dispatch_deadline_is_bounded_by_prepared_expiry() {
        let future = (Utc::now() + chrono::Duration::seconds(3)).to_rfc3339();
        let remaining = remaining_until_deadline(&future).unwrap();
        assert!(remaining <= Duration::from_secs(3));
        let expired = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        assert_eq!(
            remaining_until_deadline(&expired).unwrap_err().code,
            "JUPITER_PREVIEW_EXPIRED"
        );
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("tomorrow"), None);
    }

    #[tokio::test]
    async fn process_limiter_enforces_main_and_execute_buckets_separately() {
        let api = HttpJupiterApi::new(None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        for _ in 0..29 {
            api.acquire(RateBucket::Main, RequestPriority::Normal, deadline)
                .await
                .unwrap();
        }
        api.acquire(RateBucket::Main, RequestPriority::Financial, deadline)
            .await
            .unwrap();
        assert!(
            api.acquire(RateBucket::Main, RequestPriority::Normal, Instant::now())
                .await
                .is_err()
        );
        for _ in 0..20 {
            api.acquire(RateBucket::Execute, RequestPriority::Financial, deadline)
                .await
                .unwrap();
        }
        assert!(
            api.acquire(
                RateBucket::Execute,
                RequestPriority::Financial,
                Instant::now()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn server_retry_after_blocks_the_shared_bucket_without_blocking_other_bucket() {
        let api = HttpJupiterApi::new(None).unwrap();
        api.set_server_retry_gate(RateBucket::Main, Duration::from_secs(60))
            .await;
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(
            api.acquire(RateBucket::Main, RequestPriority::Financial, deadline)
                .await
                .is_err()
        );
        api.acquire(RateBucket::Execute, RequestPriority::Recovery, deadline)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn remaining_and_reset_headers_gate_without_current_header() {
        let api = HttpJupiterApi::new(None).unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset", "4102444800".parse().unwrap());
        api.update_rate_headers(RateBucket::Main, &headers).await;
        assert!(
            api.acquire(
                RateBucket::Main,
                RequestPriority::Recovery,
                Instant::now() + Duration::from_millis(5)
            )
            .await
            .is_err()
        );
        api.acquire(
            RateBucket::Execute,
            RequestPriority::Recovery,
            Instant::now() + Duration::from_millis(50),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn owned_key_server_limit_does_not_use_keyless_local_cap_as_remote_limit() {
        let api = HttpJupiterApi::new(None).unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-current", "31".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "469".parse().unwrap());
        headers.insert("x-ratelimit-reset", "4102444800".parse().unwrap());
        api.update_rate_headers(RateBucket::Main, &headers).await;

        // The independent local 30/minute limiter still applies. The remote
        // key-owned tier, however, must not be treated as exhausted at 31/500.
        api.acquire(
            RateBucket::Main,
            RequestPriority::Financial,
            Instant::now() + Duration::from_millis(20),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stale_success_headers_cannot_clear_a_newer_shared_retry_gate() {
        let api = HttpJupiterApi::new(None).unwrap();
        api.set_server_retry_gate(RateBucket::Main, Duration::from_secs(60))
            .await;
        let active = api.main_server_limit.lock().await.unwrap();
        let retry_gate = *api.main_retry_not_before.lock().await;

        for reset in [active.reset_epoch, active.reset_epoch.saturating_add(20)] {
            let mut stale = reqwest::header::HeaderMap::new();
            stale.insert("x-ratelimit-current", "1".parse().unwrap());
            stale.insert("x-ratelimit-remaining", "29".parse().unwrap());
            stale.insert("x-ratelimit-limit", "30".parse().unwrap());
            stale.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());
            api.update_rate_headers(RateBucket::Main, &stale).await;

            assert_eq!(*api.main_retry_not_before.lock().await, retry_gate);
            assert!(
                api.acquire(
                    RateBucket::Main,
                    RequestPriority::Recovery,
                    Instant::now() + Duration::from_millis(10),
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn equal_reset_success_cannot_reopen_exhausted_server_window() {
        let api = HttpJupiterApi::new(None).unwrap();
        let reset = 4_102_444_800u64;
        let mut exhausted = reqwest::header::HeaderMap::new();
        exhausted.insert("x-ratelimit-current", "30".parse().unwrap());
        exhausted.insert("x-ratelimit-remaining", "0".parse().unwrap());
        exhausted.insert("x-ratelimit-limit", "30".parse().unwrap());
        exhausted.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());
        api.update_rate_headers(RateBucket::Main, &exhausted).await;

        let mut stale_success = reqwest::header::HeaderMap::new();
        stale_success.insert("x-ratelimit-current", "1".parse().unwrap());
        stale_success.insert("x-ratelimit-remaining", "29".parse().unwrap());
        stale_success.insert("x-ratelimit-limit", "30".parse().unwrap());
        stale_success.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());
        api.update_rate_headers(RateBucket::Main, &stale_success)
            .await;

        assert_eq!(api.main_server_limit.lock().await.unwrap().remaining, 0);
        assert!(
            api.acquire(
                RateBucket::Main,
                RequestPriority::Recovery,
                Instant::now() + Duration::from_millis(10),
            )
            .await
            .is_err()
        );
    }
}
