//! Closed Kraken REST transport. CCXT owns venue throttling and error interpretation.
//! Reviewed signing extensions retain exact request/response bytes, including archives.
mod endpoints;
pub use endpoints::KrakenEndpoint;
mod signing;
#[cfg(all(test, feature = "test-support"))]
mod tests;
use crate::ccxt::{CcxtError, CcxtErrorKind, Credentials};
use ccxt::exchange::DerivedExchange;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

pub trait NonceAllocator: Send + Sync {
    fn allocate(&self, key_digest: &str, clock_ms: u64) -> Result<u64, CcxtError>;
}
pub type DispatchGuard = Arc<dyn Fn() -> bool + Send + Sync>;

/// Response bytes are bounded and never coerced through CCXT floating-point values.
#[derive(Clone)]
pub enum KrakenResponse {
    Json(Value),
    /// Sealed whole-request no-mutation receipt, constructed only by transport.
    Rejected {
        kind: CcxtErrorKind,
        evidence: Value,
    },
    Artifact {
        content_type: String,
        bytes: Vec<u8>,
    },
}
impl std::fmt::Debug for KrakenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(_) => f.write_str("KrakenResponse::Json(<private>)"),
            Self::Rejected { kind, .. } => write!(f, "KrakenResponse::Rejected({kind:?})"),
            Self::Artifact { bytes, .. } => {
                write!(f, "KrakenResponse::Artifact({} bytes)", bytes.len())
            }
        }
    }
}
struct Job {
    endpoint: KrakenEndpoint,
    credentials: Option<Credentials>,
    parameters: Value,
    guard: Option<DispatchGuard>,
    reply: oneshot::Sender<Result<KrakenResponse, CcxtError>>,
}
#[derive(Clone)]
pub struct KrakenClient {
    sender: mpsc::Sender<Job>,
}
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
fn failure(kind: CcxtErrorKind) -> CcxtError {
    CcxtError::new(kind)
}

impl KrakenClient {
    pub fn new(nonces: Arc<dyn NonceAllocator>) -> Result<Self, CcxtError> {
        Self::start(nonces, None)
    }
    #[cfg(feature = "test-support")]
    pub fn loopback(nonces: Arc<dyn NonceAllocator>, base: String) -> Result<Self, CcxtError> {
        let url = reqwest::Url::parse(&base).map_err(|_| failure(CcxtErrorKind::InvalidRequest))?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
            || url.path() != "/"
            || url.query().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(failure(CcxtErrorKind::InvalidRequest));
        }
        Self::start(nonces, Some(base))
    }
    fn start(nonces: Arc<dyn NonceAllocator>, base: Option<String>) -> Result<Self, CcxtError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| failure(CcxtErrorKind::Unavailable))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| failure(CcxtErrorKind::Unavailable))?;
        let (sender, mut receiver) = mpsc::channel::<Job>(32);
        crate::ccxt::install_panic_redaction();
        std::thread::Builder::new().name("gofin-kraken-ccxt".into()).spawn(move||runtime.block_on(async move {
            let _redaction=crate::ccxt::PanicRedaction::enter();
            let config=ccxt::Value::from(serde_json::json!({"enableRateLimit":true,"verbose":false,"maxRetriesOnFailure":0}));
            let spot=ccxt::exchanges::kraken::KrakenCore::new(Some(config.clone()));
            let futures=ccxt::exchanges::krakenfutures::KrakenfuturesCore::new(Some(config));
            while let Some(job)=receiver.recv().await {
                if job.reply.is_closed() { continue; }
                let cost=job.endpoint.cost();
                if job.endpoint.futures(){futures.throttle(&[ccxt::Value::Float(cost)]).await;}else{spot.throttle(&[ccxt::Value::Float(cost)]).await;}
                let result=async {
                    let parameter_digest=hex::encode(Sha256::digest(serde_json::to_vec(&job.parameters).map_err(|_|failure(CcxtErrorKind::InvalidRequest))?));
                    if job.endpoint.mutating() && !job.guard.as_ref().is_some_and(|g|g()) { return Ok(local_no_dispatch(job.endpoint,&parameter_digest,"before_signing")); }
                    let wire=signing::request(job.endpoint,job.parameters,job.credentials.as_ref(),nonces.as_ref(),base.as_deref())?;
                    // Recheck after signing/nonce commit and immediately before dispatch.
                    if job.endpoint.mutating() && !job.guard.as_ref().is_some_and(|g|g()) { return Ok(local_no_dispatch(job.endpoint,&parameter_digest,"before_http_dispatch")); }
                    let mut request=http.request(wire.method,&wire.url).headers(wire.headers);
                    if let Some(body)=wire.body { request=request.body(body); }
                    let mut response=request.send().await.map_err(|e|failure(if e.is_timeout(){CcxtErrorKind::Timeout}else{CcxtErrorKind::Unavailable}))?;
                    let status=response.status();
                    let content_type=response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v|v.to_str().ok()).unwrap_or("application/octet-stream").to_owned();
                    if response.content_length().is_some_and(|n|n>MAX_RESPONSE_BYTES as u64) { return Err(failure(CcxtErrorKind::InvalidResponse)); }
                    let mut bytes=Vec::new();
                    while let Some(chunk)=response.chunk().await.map_err(|_|failure(CcxtErrorKind::Unavailable))? {
                        if bytes.len().saturating_add(chunk.len())>MAX_RESPONSE_BYTES { return Err(failure(CcxtErrorKind::InvalidResponse)); }
                        bytes.extend_from_slice(&chunk);
                    }
                    let parsed=serde_json::from_slice::<Value>(&bytes).ok();
                    // Funding extensions use structured error objects rather
                    // than the legacy Spot error-array contract CCXT parses.
                    if job.endpoint.path().starts_with("/funding/")
                        && parsed.as_ref().and_then(|v|v.get("error")).is_some_and(|error|
                            !error.is_null() && !error.as_array().is_some_and(Vec::is_empty)) {
                        let kind=match status.as_u16(){401=>CcxtErrorKind::Authentication,403=>CcxtErrorKind::PermissionDenied,429=>CcxtErrorKind::RateLimited,_=>CcxtErrorKind::Rejected};
                        let code=parsed.as_ref().and_then(|v|v["error"]["code"].as_str());
                        if job.endpoint.mutating() && (matches!(status.as_u16(),401|403)||matches!(code,Some("invalid_argument"|"insufficient_funds"|"permission_denied"|"unauthorized"))) {
                            return Ok(rejection(job.endpoint,kind,status.as_u16(),&parameter_digest,&bytes));
                        }
                        return Err(failure(kind));
                    }
                    // Futures can assess a request successfully yet reject its
                    // financial action. Only reviewed singleton no-fill statuses
                    // can seal failure; batch member statuses remain independent.
                    if job.endpoint.mutating() && parsed.as_ref().is_some_and(|v|no_fill_futures_rejection(job.endpoint,v)) {
                        return Ok(rejection(job.endpoint,CcxtErrorKind::Rejected,status.as_u16(),&parameter_digest,&bytes));
                    }
                    let text=std::str::from_utf8(&bytes).unwrap_or("");
                    // CCXT's venue error parser runs under its panic-to-error boundary.
                    let error_result=ccxt::runtime::call_typed(async {
                        let args=[ccxt::Value::Int(status.as_u16() as i64),ccxt::Value::Str("".into()),ccxt::Value::Str("".into()),ccxt::Value::Str("".into()),ccxt::Value::Null,ccxt::Value::Str(text.to_owned().into()),parsed.as_ref().map(ccxt::Value::from_json).unwrap_or_default(),ccxt::Value::Null,ccxt::Value::Null];
                        if job.endpoint.futures() { futures.handle_errors(args[0].clone(),args[1].clone(),args[2].clone(),args[3].clone(),args[4].clone(),args[5].clone(),args[6].clone(),args[7].clone(),args[8].clone()); }
                        else { DerivedExchange::handle_errors(&spot,args[0].clone(),args[1].clone(),args[2].clone(),args[3].clone(),args[4].clone(),args[5].clone(),args[6].clone(),args[7].clone(),args[8].clone()); }
                        Ok::<_,ccxt::ExchangeError>(ccxt::Value::Null)
                    }).await;
                    if let Ok(Err(e))|Err(e)=error_result {
                        let whole_rejection=parsed.as_ref().is_some_and(|v|
                            v["error"].as_array().is_some_and(|errors|!errors.is_empty()) || (job.endpoint.futures() && v["result"]=="error"));
                        if job.endpoint.mutating() && whole_rejection && ["InsufficientFunds","AuthenticationError","PermissionDenied","InvalidOrder","BadSymbol","ArgumentsRequired","BadRequest","InvalidNonce"].iter().any(|class|e.is(class)) {
                            let safe=crate::ccxt::sanitized(e);
                            return Ok(rejection(job.endpoint,safe.kind,status.as_u16(),&parameter_digest,&bytes));
                        }
                        return Err(crate::ccxt::sanitized(e));
                    }
                    if !status.is_success() { return Err(failure(match status.as_u16(){401=>CcxtErrorKind::Authentication,403=>CcxtErrorKind::PermissionDenied,429=>CcxtErrorKind::RateLimited,_=>CcxtErrorKind::Rejected})); }
                    if job.endpoint.method().contains("retrieve_export") || job.endpoint.method().contains("accountlogcsv") { Ok(KrakenResponse::Artifact{content_type,bytes}) }
                    else if let Some(json)=parsed { Ok(KrakenResponse::Json(json)) }
                    else { Err(failure(CcxtErrorKind::InvalidResponse)) }
                }.await;
                let _=job.reply.send(result);
            }
        })).map_err(|_|failure(CcxtErrorKind::Unavailable))?;
        Ok(Self { sender })
    }
    pub async fn request(
        &self,
        endpoint: KrakenEndpoint,
        credentials: Option<Credentials>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse, CcxtError> {
        if !parameters.is_object()
            || endpoint.private() != credentials.is_some()
            || endpoint.mutating() && guard.is_none()
            || parameters.get("nonce").is_some()
            || parameters.get("otp").is_some()
        {
            return Err(failure(CcxtErrorKind::InvalidRequest));
        }
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Job {
                endpoint,
                credentials,
                parameters,
                guard,
                reply,
            })
            .await
            .map_err(|_| failure(CcxtErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| failure(CcxtErrorKind::Unavailable))?
    }
}

fn rejection(
    endpoint: KrakenEndpoint,
    kind: CcxtErrorKind,
    http_status: u16,
    parameters: &str,
    bytes: &[u8],
) -> KrakenResponse {
    KrakenResponse::Rejected {
        kind,
        evidence: serde_json::json!({"evidenceVersion":1,"classification":"whole_request_no_mutation","endpoint":endpoint.method(),"httpStatus":http_status,"safeKind":format!("{kind:?}"),"requestParametersSha256":parameters,"responseSha256":hex::encode(Sha256::digest(bytes))}),
    }
}

fn local_no_dispatch(endpoint: KrakenEndpoint, parameters: &str, phase: &str) -> KrakenResponse {
    KrakenResponse::Rejected {
        kind: CcxtErrorKind::PermissionDenied,
        evidence: serde_json::json!({
            "evidenceVersion":2,"classification":"whole_request_no_mutation",
            "receiptSource":"local_dispatch_guard","dispatchOccurred":false,
            "endpoint":endpoint.method(),"httpStatus":null,"phase":phase,
            "safeKind":"PermissionDenied","requestParametersSha256":parameters,
            "responseSha256":hex::encode(Sha256::digest(format!("gofin-local-no-dispatch-v1:{phase}"))),
        }),
    }
}

/// Classify a bounded native singleton response with no contradictory fill
/// events. A caller-supplied JSON value is not a sealed transport receipt.
/// Native batch adapters may apply this to one uniquely correlated child.
pub fn no_fill_futures_rejection(endpoint: KrakenEndpoint, response: &Value) -> bool {
    let field = match endpoint {
        KrakenEndpoint::KrakenfuturesPrivatePostSendorder => "sendStatus",
        KrakenEndpoint::KrakenfuturesPrivatePostEditorder => "editStatus",
        _ => return false,
    };
    if response["result"] != "success" {
        return false;
    }
    let state = &response[field];
    if state
        .get("orderEvents")
        .is_some_and(|events| !events.is_array() && !events.is_null())
    {
        return false;
    }
    if state["orderEvents"].as_array().is_some_and(|events| {
        events.iter().any(|event| {
            event["type"] != "REJECT"
                || event["order"].get("filled").is_some_and(|v| match v {
                    Value::Number(n) => gofin_domain::Quantity::from_decimal(&n.to_string(), 36)
                        .map_or(true, |q| !q.is_zero()),
                    Value::String(s) => {
                        gofin_domain::Quantity::from_decimal(s, 36).map_or(true, |q| !q.is_zero())
                    }
                    _ => true,
                })
        })
    }) {
        return false;
    }
    matches!(
        state["status"].as_str(),
        Some(
            "marketSuspended"
                | "marketInactive"
                | "invalidPrice"
                | "invalidSize"
                | "invalidOrderType"
                | "invalidSide"
                | "tooManySmallOrders"
                | "insufficientAvailableFunds"
                | "wouldCauseLiquidation"
                | "clientOrderIdAlreadyExist"
                | "clientOrderIdTooBig"
                | "clientOrderIdTooLong"
                | "maxPositionViolation"
                | "outsidePriceCollar"
                | "wouldIncreasePriceDislocation"
                | "notFound"
                | "orderForEditNotAStop"
                | "orderForEditNotFound"
                | "postWouldExecute"
                | "iocWouldNotExecute"
                | "selfFill"
                | "wouldNotReducePosition"
                | "marketIsPostOnly"
                | "tooManyOrders"
                | "fixedLeverageTooHigh"
                | "clientOrderIdInvalid"
                | "cannotEditTriggerPriceOfTrailingStop"
                | "cannotEditLimitPriceOfTrailingStop"
                | "wouldProcessAfterSpecifiedTime"
                | "invalidDomainConfiguration"
        )
    )
}
