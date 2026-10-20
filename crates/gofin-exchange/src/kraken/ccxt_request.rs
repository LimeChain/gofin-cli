//! CCXT endpoint construction/signing with GoFin's durable nonce admission.
//!
//! The pinned runtime has no bounded/raw HTTP hook. Keep dispatch in our guarded
//! transport; never use CCXT's mock response machinery to capture a request.
use super::{KrakenEndpoint, NonceAllocator, failure, signing};
use crate::ccxt::{CcxtError, CcxtErrorKind, Credentials};
use base64::{Engine, engine::general_purpose::STANDARD};
use ccxt::exchanges::{kraken::KrakenCore, krakenfutures::KrakenfuturesCore};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const MAX_EXACT_NONCE: u64 = (1 << 53) - 1;
const NONCE_ATTEMPTS: usize = 3;
fn invalid() -> CcxtError {
    failure(CcxtErrorKind::InvalidRequest)
}

/// Only known missing APIs and pinned signer limitations use custom signing.
pub(super) fn needs_extension(endpoint: KrakenEndpoint, parameters: &Value) -> bool {
    endpoint.path().starts_with("/funding/")
        || endpoint == KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check
        || endpoint == KrakenEndpoint::KrakenfuturesPrivatePostBatchorder
        || (endpoint.futures()
            && parameters
                .as_object()
                .is_some_and(|fields| fields.iter().any(|(k, v)| v.is_array() && k != "orderIds")))
}

pub(super) async fn request(
    endpoint: KrakenEndpoint,
    parameters: Value,
    credentials: Option<&Credentials>,
    nonces: &dyn NonceAllocator,
    base: Option<&str>,
) -> Result<signing::WireRequest, CcxtError> {
    let fields = parameters.as_object().ok_or_else(invalid)?;
    if fields.contains_key("nonce")
        || fields.contains_key("otp")
        || fields.contains_key("version")
        || endpoint.private() != credentials.is_some()
        || (!endpoint.mutating() && fields.get("new").is_some_and(|v| v == true || v == "true"))
    {
        return Err(invalid());
    }
    if let Some(c) = credentials {
        // The pinned CCXT base64 helper tolerates malformed secrets. Preserve
        // strict local failure before either nonce commitment or dispatch.
        let key = Zeroizing::new(
            STANDARD
                .decode(c.secret.as_bytes())
                .map_err(|_| invalid())?,
        );
        if key.is_empty() {
            return Err(invalid());
        }
    }
    validate_numbers(&parameters)?;
    if needs_extension(endpoint, &parameters) {
        return signing::extension_request(endpoint, parameters, credentials, nonces, base);
    }
    // All CCXT panics (including credential errors) stay behind redaction and
    // become sanitized errors; no HTTP is performed by this future.
    ccxt::runtime::call_typed(async {
        Ok::<_, ccxt::ExchangeError>(construct(
            endpoint,
            fields.clone(),
            credentials,
            nonces,
            base,
        ))
    })
    .await
    .map_err(crate::ccxt::sanitized)?
    .map_err(crate::ccxt::sanitized)?
}

fn validate_numbers(value: &Value) -> Result<(), CcxtError> {
    match value {
        Value::Number(n) if !n.is_i64() => return Err(invalid()),
        Value::Array(values) => {
            for v in values {
                validate_numbers(v)?;
            }
        }
        Value::Object(fields) => {
            for v in fields.values() {
                validate_numbers(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn construct(
    endpoint: KrakenEndpoint,
    mut fields: Map<String, Value>,
    credentials: Option<&Credentials>,
    nonces: &dyn NonceAllocator,
    base: Option<&str>,
) -> Result<signing::WireRequest, CcxtError> {
    let expected_path = encode_path_segments(endpoint, &mut fields)?;
    let config = credentials.map(|c| {
        ccxt::Value::from_json(&serde_json::json!({
            "apiKey":c.api_key.as_str(),"secret":c.secret.as_str(),"verbose":false
        }))
    });
    let mut spot = KrakenCore::new(config.clone());
    let futures = KrakenfuturesCore::new(config);
    let exchange = if endpoint.futures() {
        &futures.exchange
    } else {
        &spot.exchange
    };
    let method = if endpoint == KrakenEndpoint::KrakenLegacyCreateDepositAddress {
        "private_post_deposit_addresses"
    } else {
        endpoint.method()
    };
    let (mut path, scope, mut verb, _) = exchange
        .internals
        .implicit_api
        .get(method)
        .cloned()
        .ok_or_else(invalid)?;
    // This implicit method name retains GET although Kraken's current status
    // endpoint is POST. Keep the closed contract verb; CCXT signs the same data.
    if endpoint == KrakenEndpoint::KrakenfuturesPrivateGetOrdersStatus && verb == "GET" {
        verb = "POST".into();
    }
    if verb != endpoint.verb() || scope.len() != 1 {
        return Err(invalid());
    }
    let api = &scope[0];
    // This pinned entry predates Kraken's current route spelling. Retain the
    // reviewed official route while reusing its CCXT signer and access scope.
    if endpoint == KrakenEndpoint::KrakenfuturesPublicGetHistoricalfundingrates {
        if path != "historicalfundingrates" {
            return Err(invalid());
        }
        path = "historical-funding-rates".into();
    }
    let official_path = if endpoint.futures() {
        if endpoint == KrakenEndpoint::KrakenfuturesChartsGetPriceTypeSymbolInterval {
            for (from, to) in [("tick_type", "price_type"), ("resolution", "interval")] {
                if let Some(v) = fields.remove(from)
                    && fields.insert(to.into(), v).is_some()
                {
                    return Err(invalid());
                }
            }
            "/{price_type}/{symbol}/{interval}".to_owned()
        } else if endpoint.path().contains("{subaccountUid}") {
            if let Some(value) = fields.remove("subaccountUid")
                && fields.insert("uid".into(), value).is_some()
            {
                return Err(invalid());
            }
            endpoint.path().replace("{subaccountUid}", "{uid}")
        } else if endpoint.path().contains("{tradeable}") {
            if let Some(value) = fields.remove("tradeable")
                && fields.insert("symbol".into(), value).is_some()
            {
                return Err(invalid());
            }
            endpoint.path().replace("{tradeable}", "{symbol}")
        } else {
            endpoint.path().to_owned()
        }
    } else {
        format!("/{api}/{}", path)
    };
    if endpoint.futures() && format!("/{path}") != official_path
        || !endpoint.futures() && official_path != endpoint.path()
    {
        return Err(invalid());
    }

    // New wallet-account scope is a query parameter even on legacy POSTs.
    // CCXT's legacy signer only knows body fields; retain this location shim.
    let mut query = Map::new();
    if !endpoint.futures() && endpoint.private() {
        for key in endpoint.query_parameters() {
            if let Some(value) = fields.remove(*key) {
                query.insert((*key).into(), value);
            }
        }
    }
    if endpoint.futures() {
        // Pin the reviewed API version, never CCXT's older per-method default
        // and never a caller-controlled override. The signer removes this field.
        let base_url = reqwest::Url::parse(endpoint.base()).map_err(|_| invalid())?;
        let version = base_url.path().rsplit('/').next().ok_or_else(invalid)?;
        fields.insert("version".into(), Value::String(version.into()));
    }
    let parameters = ccxt::Value::from_json(&Value::Object(fields));
    for _ in 0..NONCE_ATTEMPTS {
        let args = [
            ccxt::Value::from(api.as_str()),
            ccxt::Value::from(verb.as_str()),
            parameters.clone(),
        ];
        let signed = if endpoint.futures() {
            futures.sign(ccxt::Value::from(path.as_str()), &args)
        } else {
            spot.sign(ccxt::Value::from(path.as_str()), &args)
        }
        .to_json();
        let mut wire = decode_wire(&signed)?;
        if !endpoint.futures() && endpoint.private() {
            let nonce = signed_nonce(&wire)?;
            if nonce == 0 || nonce > MAX_EXACT_NONCE {
                return Err(invalid());
            }
            let c = credentials.ok_or_else(invalid)?;
            let committed =
                nonces.allocate(&hex::encode(Sha256::digest(c.api_key.as_bytes())), nonce)?;
            if committed < nonce || committed > MAX_EXACT_NONCE {
                return Err(invalid());
            }
            if committed != nonce {
                ccxt::set_value(
                    &mut spot.options,
                    &ccxt::Value::from("lastNonce"),
                    ccxt::Value::Int(committed as i64),
                );
                continue;
            }
        }
        // Futures signs encoded postData but this CCXT version places it in the
        // URL. Moving those identical signed bytes to the documented body does
        // not change authentication. Batch and repeated-array gaps are explicit
        // extensions above, not a second implementation of ordinary signing.
        if endpoint.futures() && wire.method != reqwest::Method::GET {
            let mut url = reqwest::Url::parse(&wire.url).map_err(|_| invalid())?;
            wire.body = Some(url.query().unwrap_or("").to_owned());
            url.set_query(None);
            wire.url = url.to_string();
            wire.headers.insert(
                "content-type",
                reqwest::header::HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
        }
        if !query.is_empty() {
            wire.url.push('?');
            wire.url.push_str(&signing::form(&query)?);
        }
        validate_and_rebase(endpoint, &expected_path, &mut wire, base)?;
        wire.headers.insert(
            "accept",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        return Ok(wire);
    }
    Err(invalid())
}

fn decode_wire(signed: &Value) -> Result<signing::WireRequest, CcxtError> {
    let url = signed["url"].as_str().ok_or_else(invalid)?.to_owned();
    let method =
        reqwest::Method::from_bytes(signed["method"].as_str().ok_or_else(invalid)?.as_bytes())
            .map_err(|_| invalid())?;
    let body = match &signed["body"] {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => return Err(invalid()),
    };
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(fields) = signed["headers"].as_object() {
        for (name, value) in fields {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?,
                reqwest::header::HeaderValue::from_str(value.as_str().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?,
            );
        }
    }
    Ok(signing::WireRequest {
        method,
        url,
        headers,
        body,
    })
}

pub(super) fn signed_nonce(wire: &signing::WireRequest) -> Result<u64, CcxtError> {
    let body = wire.body.as_deref().ok_or_else(invalid)?;
    if wire
        .headers
        .get("content-type")
        .is_some_and(|v| v == "application/json")
    {
        let body: Value = serde_json::from_str(body).map_err(|_| invalid())?;
        body["nonce"]
            .as_str()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())
    } else {
        let url = reqwest::Url::parse(&format!("https://gofin.invalid/?{body}"))
            .map_err(|_| invalid())?;
        let values: Vec<_> = url.query_pairs().filter(|(k, _)| k == "nonce").collect();
        if values.len() != 1 {
            return Err(invalid());
        }
        values[0].1.parse().map_err(|_| invalid())
    }
}

fn validate_and_rebase(
    endpoint: KrakenEndpoint,
    expected_path: &str,
    wire: &mut signing::WireRequest,
    base: Option<&str>,
) -> Result<(), CcxtError> {
    let url = reqwest::Url::parse(&wire.url).map_err(|_| invalid())?;
    let expected = reqwest::Url::parse(endpoint.base()).map_err(|_| invalid())?;
    if url.origin() != expected.origin()
        || wire.method.as_str() != endpoint.verb()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path() != expected_path
    {
        return Err(invalid());
    }
    if let Some(base) = base {
        wire.url = format!(
            "{}{}{}",
            base.trim_end_matches('/'),
            url.path(),
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
    }
    Ok(())
}

// CCXT's implode_params does literal substitution. Preserve each caller value
// as one encoded segment before invoking it, then require the exact closed path.
fn encode_path_segments(
    endpoint: KrakenEndpoint,
    fields: &mut Map<String, Value>,
) -> Result<String, CcxtError> {
    const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    let mut path = endpoint.path().to_owned();
    while let Some(start) = path.find('{') {
        let end = start + path[start..].find('}').ok_or_else(invalid)?;
        let key = path[start + 1..end].to_owned();
        let value = match fields.get(&key).ok_or_else(invalid)? {
            Value::String(s) => s.clone(),
            Value::Number(n) if n.is_i64() => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => return Err(invalid()),
        };
        if value.is_empty() || value == "." || value == ".." {
            return Err(invalid());
        }
        let encoded = percent_encoding::utf8_percent_encode(&value, SEGMENT).to_string();
        fields.insert(key, Value::String(encoded.clone()));
        path.replace_range(start..=end, &encoded);
    }
    Ok(format!(
        "{}{}",
        reqwest::Url::parse(endpoint.base())
            .map_err(|_| invalid())?
            .path()
            .trim_end_matches('/'),
        path
    ))
}
