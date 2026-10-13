//! Exact-byte signing for legacy Spot, current Funding and Derivatives REST.
//! Reviewed with Ogi (glm-5.3-flash); golden vectors are verified locally.
use super::{KrakenEndpoint, NonceAllocator, failure};
use crate::ccxt::{CcxtError, CcxtErrorKind, Credentials};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256, Sha512};
use zeroize::Zeroizing;

const PATH_SEGMENT_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub(super) struct WireRequest {
    pub method: reqwest::Method,
    pub url: String,
    pub headers: reqwest::header::HeaderMap,
    pub body: Option<String>,
}
fn invalid() -> CcxtError {
    failure(CcxtErrorKind::InvalidRequest)
}
fn text(value: &Value) -> Result<String, CcxtError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
        _ => Err(invalid()),
    }
}
fn flatten(name: &str, value: &Value, out: &mut Vec<(String, String)>) -> Result<(), CcxtError> {
    match value {
        Value::Object(fields) => {
            for (k, v) in fields {
                flatten(&format!("{name}[{k}]"), v, out)?;
            }
        }
        Value::Array(values) => {
            for (i, v) in values.iter().enumerate() {
                flatten(&format!("{name}[{i}]"), v, out)?;
            }
        }
        Value::Null => (),
        _ => out.push((name.into(), text(value)?)),
    }
    Ok(())
}
fn form(fields: &Map<String, Value>) -> Result<String, CcxtError> {
    let mut pairs = vec![];
    for (k, v) in fields {
        flatten(k, v, &mut pairs)?;
    }
    let mut url = reqwest::Url::parse("https://fin.invalid").map_err(|_| invalid())?;
    {
        let mut query = url.query_pairs_mut();
        for (k, v) in pairs {
            query.append_pair(&k, &v);
        }
    }
    Ok(url.query().unwrap_or("").into())
}
fn futures_form(fields: &Map<String, Value>) -> Result<String, CcxtError> {
    // Futures array parameters use form/explode (the default for its OpenAPI
    // query parameters). Preserve every array member with a repeated field.
    let mut url = reqwest::Url::parse("https://fin.invalid").map_err(|_| invalid())?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in fields {
            match value {
                Value::Array(values) => {
                    for value in values {
                        query.append_pair(key, &text(value)?);
                    }
                }
                Value::Null => (),
                _ => {
                    query.append_pair(key, &text(value)?);
                }
            }
        }
    }
    Ok(url.query().unwrap_or("").into())
}
fn mac(secret: &str, data: &[u8]) -> Result<String, CcxtError> {
    let key = Zeroizing::new(STANDARD.decode(secret).map_err(|_| invalid())?);
    if key.is_empty() {
        return Err(invalid());
    }
    let mut hmac = Hmac::<Sha512>::new_from_slice(&key).map_err(|_| invalid())?;
    hmac.update(data);
    Ok(STANDARD.encode(hmac.finalize().into_bytes()))
}
fn spot_signature(secret: &str, path: &str, nonce: &str, body: &str) -> Result<String, CcxtError> {
    let mut digest = Sha256::new();
    digest.update(nonce.as_bytes());
    digest.update(body.as_bytes());
    let mut message = path.as_bytes().to_vec();
    message.extend_from_slice(&digest.finalize());
    mac(secret, &message)
}
fn header(
    headers: &mut reqwest::header::HeaderMap,
    key: &'static str,
    value: &str,
) -> Result<(), CcxtError> {
    headers.insert(
        key,
        reqwest::header::HeaderValue::from_str(value).map_err(|_| invalid())?,
    );
    Ok(())
}
pub(super) fn request(
    endpoint: KrakenEndpoint,
    parameters: Value,
    credentials: Option<&Credentials>,
    nonces: &dyn NonceAllocator,
    base: Option<&str>,
) -> Result<WireRequest, CcxtError> {
    let mut fields = parameters.as_object().cloned().ok_or_else(invalid)?;
    if fields.contains_key("nonce") || fields.contains_key("otp") {
        return Err(invalid());
    }
    if !endpoint.mutating() && fields.get("new").is_some_and(|v| v == true || v == "true") {
        return Err(invalid());
    }
    let mut path = endpoint.path().to_owned();
    while let Some(start) = path.find('{') {
        let end = path[start..]
            .find('}')
            .map(|i| start + i)
            .ok_or_else(invalid)?;
        let key = path[start + 1..end].to_owned();
        let value = text(&fields.remove(&key).ok_or_else(invalid)?)?;
        if value.is_empty() || value == "." || value == ".." {
            return Err(invalid());
        }
        // Keep RFC 3986 unreserved characters literal in path parameters.
        // Kraken's Funding endpoints use UUID path segments; percent-encoding
        // their hyphens changes the URI path used by the server to verify the
        // API-Sign HMAC even though the HTTP router may otherwise accept it.
        let encoded =
            percent_encoding::utf8_percent_encode(&value, PATH_SEGMENT_ENCODE_SET).to_string();
        path.replace_range(start..=end, &encoded);
    }
    let method = reqwest::Method::from_bytes(endpoint.verb().as_bytes()).map_err(|_| invalid())?;
    let mut headers = reqwest::header::HeaderMap::new();
    header(&mut headers, "accept", "application/json")?;
    let mut body = None;
    if endpoint.futures() {
        let post_data = if endpoint.method().ends_with("batchorder") {
            // Encode the JSON exactly once; sign the same bytes that are sent.
            let mut query_fields = Map::new();
            query_fields.insert(
                "json".into(),
                Value::String(serde_json::to_string(&fields).map_err(|_| invalid())?),
            );
            form(&query_fields)?
        } else {
            futures_form(&fields)?
        };
        if endpoint.private() {
            let credentials = credentials.ok_or_else(invalid)?;
            let base_url = reqwest::Url::parse(endpoint.base()).map_err(|_| invalid())?;
            // Derivatives removes /derivatives; history keeps /api/history/version.
            let signing_path = format!(
                "{}{}",
                base_url.path().trim_start_matches("/derivatives"),
                path
            );
            let digest = Sha256::digest(format!("{post_data}{signing_path}").as_bytes());
            header(&mut headers, "apikey", &credentials.api_key)?;
            header(&mut headers, "authent", &mac(&credentials.secret, &digest)?)?;
        }
        if method == reqwest::Method::GET {
            if !post_data.is_empty() {
                path.push('?');
                path.push_str(&post_data);
            }
        } else {
            body = Some(post_data);
            header(
                &mut headers,
                "content-type",
                "application/x-www-form-urlencoded",
            )?;
        }
    } else if path.starts_with("/funding/") {
        // Preserve OpenAPI parameter locations. In particular account_id is a
        // query parameter even on POST; Funding signs this complete query path.
        let mut query_fields = Map::new();
        for key in endpoint.query_parameters() {
            if let Some(value) = fields.remove(*key) {
                query_fields.insert((*key).into(), value);
            }
        }
        if !matches!(method, reqwest::Method::GET | reqwest::Method::DELETE) {
            let query = form(&query_fields)?;
            if !query.is_empty() {
                path.push('?');
                path.push_str(&query);
            }
        } else {
            fields.extend(query_fields);
        }
        if matches!(method, reqwest::Method::GET | reqwest::Method::DELETE) {
            let query = form(&fields)?;
            if !query.is_empty() {
                path.push('?');
                path.push_str(&query);
            }
        } else {
            body = Some(serde_json::to_string(&fields).map_err(|_| invalid())?);
            header(&mut headers, "content-type", "application/json")?;
        }
        let credentials = credentials.ok_or_else(invalid)?;
        let clock = chrono::Utc::now()
            .timestamp_millis()
            .try_into()
            .map_err(|_| invalid())?;
        let nonce = nonces
            .allocate(
                &hex::encode(Sha256::digest(credentials.api_key.as_bytes())),
                clock,
            )?
            .to_string();
        header(&mut headers, "api-key", &credentials.api_key)?;
        header(&mut headers, "api-nonce", &nonce)?;
        header(
            &mut headers,
            "api-sign",
            &spot_signature(
                &credentials.secret,
                &path,
                &nonce,
                body.as_deref().unwrap_or(""),
            )?,
        )?;
    } else if endpoint.private() {
        let mut query_fields = Map::new();
        for key in endpoint.query_parameters() {
            if let Some(value) = fields.remove(*key) {
                query_fields.insert((*key).into(), value);
            }
        }
        let credentials = credentials.ok_or_else(invalid)?;
        let clock = chrono::Utc::now()
            .timestamp_millis()
            .try_into()
            .map_err(|_| invalid())?;
        let nonce = nonces
            .allocate(
                &hex::encode(Sha256::digest(credentials.api_key.as_bytes())),
                clock,
            )?
            .to_string();
        fields.insert("nonce".into(), Value::String(nonce.clone()));
        let json = endpoint.method().contains("order_batch")
            || fields
                .get("price")
                .and_then(Value::as_str)
                .is_some_and(|v| v.ends_with('%'));
        let encoded = if json {
            serde_json::to_string(&fields).map_err(|_| invalid())?
        } else {
            form(&fields)?
        };
        header(
            &mut headers,
            "content-type",
            if json {
                "application/json"
            } else {
                "application/x-www-form-urlencoded"
            },
        )?;
        header(&mut headers, "api-key", &credentials.api_key)?;
        header(
            &mut headers,
            "api-sign",
            &spot_signature(&credentials.secret, &format!("/0{path}"), &nonce, &encoded)?,
        )?;
        body = Some(encoded);
        // Legacy authentication signs the URI path and POST payload. Query
        // parameters use their documented location, outside that URI path.
        let query = form(&query_fields)?;
        if !query.is_empty() {
            path.push('?');
            path.push_str(&query);
        }
    } else {
        let query = form(&fields)?;
        if !query.is_empty() {
            path.push('?');
            path.push_str(&query);
        }
    }
    let base = if let Some(loopback) = base {
        // Test replacement changes only the origin, preserving the production
        // /0 or /derivatives/api/version path that participates in signing.
        let original = reqwest::Url::parse(endpoint.base()).map_err(|_| invalid())?;
        format!(
            "{}{}",
            loopback.trim_end_matches('/'),
            original.path().trim_end_matches('/')
        )
    } else {
        endpoint.base().trim_end_matches('/').to_owned()
    };
    Ok(WireRequest {
        method,
        url: format!("{base}{path}"),
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixed;
    impl NonceAllocator for Fixed {
        fn allocate(&self, _: &str, _: u64) -> Result<u64, CcxtError> {
            Ok(1616492376594)
        }
    }
    fn credentials() -> Credentials {
        Credentials {
            api_key: Zeroizing::new("synthetic-api-key".into()),
            secret: Zeroizing::new("c3ludGhldGljLW5vbi1saXZlLWtleQ==".into()),
        }
    }
    #[test]
    fn official_spot_signature_vector() {
        let actual=spot_signature("kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==","/0/private/AddOrder","1616492376594","nonce=1616492376594&ordertype=limit&pair=XBTUSD&price=37500&type=buy&volume=1.25").unwrap();
        assert_eq!(
            actual,
            "4/dpxb3iT4tp/ZCVEwSnEsLxx0bqyhLpdfOpc6fn7OR8+UClSV5n9E6aSS8MPtnRfp32bAb0nmbRn6H8ndwLUQ=="
        );
    }
    #[test]
    fn funding_query_and_nonce_are_bound_exactly() {
        let c = credentials();
        let w = request(
            KrakenEndpoint::KrakenFundingGetFundingV1MethodsDirection,
            serde_json::json!({"direction":"deposit","asset":{"class":"currency","name":"USDC"}}),
            Some(&c),
            &Fixed,
            None,
        )
        .unwrap();
        assert!(w.url.contains("asset%5Bclass%5D=currency"));
        assert_eq!(w.headers["api-nonce"], "1616492376594");
        assert_eq!(
            w.headers["api-sign"],
            spot_signature(
                &c.secret,
                w.url.strip_prefix("https://api.kraken.com").unwrap(),
                "1616492376594",
                ""
            )
            .unwrap()
        );
    }
    #[test]
    fn funding_uuid_path_parameters_keep_uri_unreserved_bytes_literal() {
        let wire = request(
            KrakenEndpoint::KrakenFundingGetFundingV1FeesMethodId,
            serde_json::json!({"method_id":"742656ee-ecc8-4287-a301-5a472ca16c24","amount":"3.50","fee_included":true}),
            Some(&credentials()),
            &Fixed,
            None,
        )
        .unwrap();
        assert!(
            wire.url
                .contains("/funding/v1/fees/742656ee-ecc8-4287-a301-5a472ca16c24?")
        );
        assert!(!wire.url.contains("%2D"));
    }
    #[test]
    fn financial_decimals_never_pass_through_floats() {
        let c = credentials();
        let amount = "0.123456789012345678901234567890123456";
        let w = request(
            KrakenEndpoint::KrakenPrivatePostAddOrder,
            serde_json::json!({"volume":amount,"pair":"XBTUSD","type":"sell","ordertype":"market"}),
            Some(&c),
            &Fixed,
            None,
        )
        .unwrap();
        assert!(w.body.unwrap().contains(amount));
    }
    #[test]
    fn futures_batches_hash_encoded_wire_and_omit_nonce() {
        let c = credentials();
        let w = request(
            KrakenEndpoint::KrakenfuturesPrivatePostBatchorder,
            serde_json::json!({"orders":[{"size":"1.000000000000000001","cliOrdId":"test λ"}]}),
            Some(&c),
            &Fixed,
            None,
        )
        .unwrap();
        let body = w.body.unwrap();
        assert!(body.starts_with("json=%7B"));
        assert!(body.contains("%CE%BB"));
        assert!(!w.headers.contains_key("nonce"));
        let digest = Sha256::digest(format!("{body}/api/v3/batchorder").as_bytes());
        assert_eq!(w.headers["authent"], mac(&c.secret, &digest).unwrap());
    }
    #[test]
    fn reads_cannot_generate_addresses_or_override_authentication() {
        let c = credentials();
        assert!(
            request(
                KrakenEndpoint::KrakenPrivatePostDepositAddresses,
                serde_json::json!({"new":true}),
                Some(&c),
                &Fixed,
                None
            )
            .is_err()
        );
        assert!(
            request(
                KrakenEndpoint::KrakenPrivatePostBalance,
                serde_json::json!({"nonce":"1"}),
                Some(&c),
                &Fixed,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn derivatives_auth_check_signs_its_exact_api_auth_path_without_a_nonce() {
        let credentials = Credentials {
            api_key: "synthetic-api-key".to_string().into(),
            secret: "c3ludGhldGljLXNpZ25pbmcta2V5".to_string().into(),
        };
        let wire = request(
            KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check,
            serde_json::json!({}),
            Some(&credentials),
            &Fixed,
            None,
        )
        .unwrap();
        assert_eq!(
            wire.url,
            "https://futures.kraken.com/api/auth/v1/api-keys/v3/check"
        );
        assert_eq!(
            wire.headers["authent"],
            "tCmi6EZdxl5SmpfTFV2xZrIKteOTcUd7JRwBCpnFxfQ6bn9TPlEh3lhhqS9nOFobfs0qG2+88lUKJOjYIREx5w=="
        );
        assert!(!wire.headers.contains_key("nonce"));
        assert!(wire.body.is_none());
    }
}
