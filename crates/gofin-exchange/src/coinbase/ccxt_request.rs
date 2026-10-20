//! Pinned CCXT registry, pacing and signers behind the closed Coinbase routes.
//! HTTP remains in the bounded GoFin transport. The pinned JWT runtime ignores
//! EdDSA/ES256 options, so CDP token generation stays local. An explicit HMAC
//! shim covers bodies CCXT cannot serialize byte-for-byte. Pre-resolving reviewed
//! paths preserves scoped DELETE queries and International HMAC input.
use super::{CoinbaseAuth, CoinbaseError, CoinbaseRoute, DispatchGuard, Host, auth_headers};
use base64::{Engine, engine::general_purpose::STANDARD};
use ccxt::exchanges::{
    coinbase::CoinbaseCore, coinbaseexchange::CoinbaseexchangeCore,
    coinbaseinternational::CoinbaseinternationalCore,
};
use ccxt::{Value, exchange::Exchange, exchange_generated::ExchangeBase};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

pub(super) struct Wire {
    pub url: String,
    pub headers: reqwest::header::HeaderMap,
    pub body: String,
}
struct Job {
    route: CoinbaseRoute,
    body: String,
    guard: Option<DispatchGuard>,
    reply: oneshot::Sender<Result<Wire, CoinbaseError>>,
}
pub(super) struct Client {
    sender: mpsc::Sender<Job>,
}
impl Client {
    pub fn new(auth: Arc<CoinbaseAuth>) -> Result<Self, CoinbaseError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| CoinbaseError::Transport)?;
        let (sender, mut receiver) = mpsc::channel::<Job>(32);
        crate::ccxt::install_panic_redaction();
        std::thread::Builder::new()
            .name("gofin-coinbase-ccxt".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let _redaction = crate::ccxt::PanicRedaction::enter();
                    let mut core = Core::new(&auth);
                    while let Some(job) = receiver.recv().await {
                        if job.reply.is_closed() {
                            continue;
                        }
                        let result = ccxt::runtime::call_typed(async {
                            Ok::<_, ccxt::ExchangeError>(
                                async {
                                    let binding = core.binding(&job.route)?;
                                    let cost = core.cost(&binding, job.route.method());
                                    core.exchange().throttle(&[cost]).await;
                                    if job.reply.is_closed() {
                                        return Err(CoinbaseError::GuardRejected);
                                    }
                                    if job.route.mutating()
                                        && !job.guard.as_ref().is_some_and(|guard| guard())
                                    {
                                        return Err(CoinbaseError::GuardRejected);
                                    }
                                    core.construct(&auth, &job.route, &job.body, &binding)
                                }
                                .await,
                            )
                        })
                        .await;
                        let result = match result {
                            Ok(Ok(result)) => result,
                            _ => Err(CoinbaseError::InvalidRequest),
                        };
                        let _ = job.reply.send(result);
                    }
                })
            })
            .map_err(|_| CoinbaseError::Transport)?;
        Ok(Self { sender })
    }
    pub async fn request(
        &self,
        route: CoinbaseRoute,
        body: String,
        guard: Option<DispatchGuard>,
    ) -> Result<Wire, CoinbaseError> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(Job {
                route,
                body,
                guard,
                reply,
            })
            .await
            .map_err(|_| CoinbaseError::Transport)?;
        receiver.await.map_err(|_| CoinbaseError::Transport)?
    }
}

pub(super) enum Core {
    App(Box<CoinbaseCore>),
    Exchange(Box<CoinbaseexchangeCore>),
    International(Box<CoinbaseinternationalCore>),
}
pub(super) struct Binding {
    path: String,
    scope: Vec<String>,
    config: Value,
    /// Only Exchange's legacy GET-address lookup is absent from the pin. It
    /// shares the registered POST route cost but cannot become a creation call.
    #[cfg(test)]
    pub missing_address_read: bool,
}
impl Binding {
    fn api(&self) -> Value {
        if self.scope.len() == 1 {
            Value::from(self.scope[0].as_str())
        } else {
            Value::from(
                self.scope
                    .iter()
                    .map(|s| Value::from(s.as_str()))
                    .collect::<Vec<_>>(),
            )
        }
    }
    fn private(&self) -> bool {
        self.scope.last().is_some_and(|s| s == "private")
    }
}
impl Core {
    pub fn new(auth: &CoinbaseAuth) -> Self {
        let mut config =
            serde_json::json!({"enableRateLimit":true,"verbose":false,"maxRetriesOnFailure":0});
        match auth {
            CoinbaseAuth::CdpEd25519 { .. } | CoinbaseAuth::CdpEcdsa { .. } => {
                Self::App(Box::new(CoinbaseCore::new(Some(Value::from_json(&config)))))
            }
            CoinbaseAuth::ExchangeHmac {
                api_key,
                secret,
                passphrase,
            }
            | CoinbaseAuth::InternationalHmac {
                api_key,
                secret,
                passphrase,
            } => {
                config["apiKey"] = api_key.clone().into();
                config["secret"] = secret.clone().into();
                config["password"] = passphrase.clone().into();
                if matches!(auth, CoinbaseAuth::ExchangeHmac { .. }) {
                    Self::Exchange(Box::new(CoinbaseexchangeCore::new(Some(Value::from_json(
                        &config,
                    )))))
                } else {
                    Self::International(Box::new(CoinbaseinternationalCore::new(Some(
                        Value::from_json(&config),
                    ))))
                }
            }
        }
    }
    fn exchange(&self) -> &Exchange {
        match self {
            Self::App(core) => &core.exchange,
            Self::Exchange(core) => &core.exchange,
            Self::International(core) => &core.exchange,
        }
    }
    fn cost(&self, binding: &Binding, verb: &str) -> Value {
        let api = binding.api();
        let method = Value::from(verb);
        let path = Value::from(binding.path.as_str());
        let params = Value::from(serde_json::json!({}));
        let config = [binding.config.clone()];
        match self {
            Self::App(core) => core.calculate_rate_limiter_cost(api, method, path, params, &config),
            Self::Exchange(core) => {
                core.calculate_rate_limiter_cost(api, method, path, params, &config)
            }
            Self::International(core) => {
                core.calculate_rate_limiter_cost(api, method, path, params, &config)
            }
        }
    }
    pub fn binding(&self, route: &CoinbaseRoute) -> Result<Binding, CoinbaseError> {
        if !matches!(
            (self, route.host()),
            (Self::App(_), Host::App)
                | (Self::Exchange(_), Host::Exchange)
                | (Self::International(_), Host::International)
        ) {
            return Err(CoinbaseError::InvalidCredential);
        }
        let full = route.path()?;
        let path_only = full
            .split('?')
            .next()
            .ok_or(CoinbaseError::InvalidRequest)?;
        let missing_address_read = matches!(route, CoinbaseRoute::ExchangeDepositAddresses { .. });
        let method = if missing_address_read {
            "POST"
        } else {
            route.method()
        };
        let mut found = None;
        let mut best_specificity = 0;
        let mut ambiguous = false;
        for (path, scope, verb, config) in self.exchange().internals.implicit_api.values() {
            if verb != method {
                continue;
            }
            let prefix = match self {
                Self::App(_) if scope.first().is_some_and(|s| s == "v3") => "/api/v3/",
                Self::App(_) => "/v2/",
                Self::Exchange(_) => "/",
                Self::International(_) => "/api/v1/",
            };
            let template = format!("{prefix}{path}");
            if !matches_path(&template, path_only) {
                continue;
            }
            let specificity = template
                .split('{')
                .map(|part| {
                    part.split_once('}')
                        .map_or(part.len(), |(_, suffix)| suffix.len())
                })
                .sum::<usize>();
            if specificity < best_specificity {
                continue;
            }
            if specificity > best_specificity {
                found = None;
                best_specificity = specificity;
                ambiguous = false;
            }
            if let Some(previous) = &found {
                let previous: &Binding = previous;
                if previous.path != *path || previous.scope != *scope {
                    ambiguous = true;
                }
            } else {
                found = Some(Binding {
                    path: path.clone(),
                    scope: scope.clone(),
                    config: config.clone(),
                    #[cfg(test)]
                    missing_address_read,
                });
            }
        }
        if ambiguous {
            return Err(CoinbaseError::InvalidRequest);
        }
        found.ok_or(CoinbaseError::InvalidRequest)
    }
    pub fn construct(
        &mut self,
        auth: &CoinbaseAuth,
        route: &CoinbaseRoute,
        body: &str,
        binding: &Binding,
    ) -> Result<Wire, CoinbaseError> {
        let path = route.path()?;
        let method = route.method();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| CoinbaseError::Transport)?
            .as_secs();
        if let CoinbaseAuth::ExchangeHmac { secret, .. }
        | CoinbaseAuth::InternationalHmac { secret, .. } = auth
        {
            let decoded = Zeroizing::new(
                STANDARD
                    .decode(secret)
                    .map_err(|_| CoinbaseError::InvalidCredential)?,
            );
            if decoded.is_empty() {
                return Err(CoinbaseError::InvalidCredential);
            }
        }
        // Exact-safe conversion only. Even valid JSON numeric lexemes must not
        // silently become floating point. The body is never rewritten on wire.
        let parameters = if body.is_empty() {
            Some(Value::from(serde_json::json!({})))
        } else {
            let parsed: serde_json::Value =
                serde_json::from_str(body).map_err(|_| CoinbaseError::InvalidRequest)?;
            if !parsed.is_object() {
                return Err(CoinbaseError::InvalidRequest);
            }
            exact_value(&parsed).then(|| Value::from_json(&parsed))
        };
        let prefix = match self {
            Self::App(_) if binding.scope.first().is_some_and(|s| s == "v3") => "/api/v3/",
            Self::App(_) => "/v2/",
            Self::Exchange(_) => "/",
            Self::International(_) => "/api/v1/",
        };
        let resolved = path
            .strip_prefix(prefix)
            .ok_or(CoinbaseError::InvalidRequest)?;
        // CCXT's native signer receives the already bound/encoded path. This
        // keeps reviewed repeated queries and DELETE scope in the URL, and lets
        // International include those exact query bytes in its signed path.
        if let Self::App(core) = self {
            core.headers = Value::from(serde_json::json!({}));
            if binding.private() {
                let headers = auth_headers(auth, route, method, &path, body, now)?;
                let token = headers
                    .first()
                    .ok_or(CoinbaseError::InvalidCredential)?
                    .1
                    .as_str();
                core.headers = Value::from(serde_json::json!({"Authorization":token}));
            }
        }
        let args = [
            binding.api(),
            Value::from(method),
            parameters
                .clone()
                .unwrap_or_else(|| Value::from(serde_json::json!({}))),
            Value::Null,
            Value::from(body),
        ];
        let signed = match self {
            Self::App(core) => core.sign(Value::from(resolved), &args),
            Self::Exchange(core) => core.sign(Value::from(resolved), &args),
            Self::International(core) => core.sign(Value::from(resolved), &args),
        }
        .to_json();
        let url = signed["url"]
            .as_str()
            .ok_or(CoinbaseError::InvalidRequest)?
            .to_owned();
        if url != route.url()? || signed["method"].as_str() != Some(method) {
            return Err(CoinbaseError::InvalidRequest);
        }
        let signed_body = signed["body"].as_str().unwrap_or("");
        let body_extension = !matches!(self, Self::App(_))
            && binding.private()
            && (!body.is_empty()
                && (parameters.is_none()
                    || signed_body != body
                    || parameters
                        .as_ref()
                        .and_then(Value::as_map)
                        .is_some_and(|fields| fields.is_empty())));
        let mut headers = reqwest::header::HeaderMap::new();
        if body_extension {
            // These are explicit compatibility cases, never a response-error
            // fallback or authorization bypass. CCXT has already bound route,
            // access, method and pacing; only exact signed bytes differ.
            for (name, value) in auth_headers(auth, route, method, &path, body, now)? {
                insert_header(&mut headers, name, &value)?;
            }
        } else if let Some(fields) = signed["headers"].as_object() {
            for (name, value) in fields {
                insert_header(
                    &mut headers,
                    name,
                    value.as_str().ok_or(CoinbaseError::InvalidRequest)?,
                )?;
            }
        }
        if let Self::App(core) = self {
            core.headers = Value::from(serde_json::json!({}));
        }
        Ok(Wire {
            url,
            headers,
            body: body.to_owned(),
        })
    }
}
fn insert_header(
    headers: &mut reqwest::header::HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), CoinbaseError> {
    headers.insert(
        reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| CoinbaseError::InvalidRequest)?,
        reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| CoinbaseError::InvalidCredential)?,
    );
    Ok(())
}
fn matches_path(template: &str, actual: &str) -> bool {
    let template: Vec<_> = template.split('/').collect();
    let actual: Vec<_> = actual.split('/').collect();
    template.len() == actual.len()
        && template.into_iter().zip(actual).all(|(template, actual)| {
            if let Some((prefix, rest)) = template.split_once('{') {
                let Some((_, suffix)) = rest.split_once('}') else {
                    return false;
                };
                actual.starts_with(prefix)
                    && actual.ends_with(suffix)
                    && actual.len() > prefix.len() + suffix.len()
            } else {
                template == actual
            }
        })
}
fn exact_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(n) => n.is_i64(),
        serde_json::Value::Array(values) => values.iter().all(exact_value),
        serde_json::Value::Object(fields) => fields.values().all(exact_value),
        _ => true,
    }
}
