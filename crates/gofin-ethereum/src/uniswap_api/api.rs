//! Keyless provider transport and quote-metadata trust boundary.
//!
//! Financial intent is checked against structured quotes. Provider transaction and
//! order encodings remain opaque; this module deliberately does not decode them.
use async_trait::async_trait;
use gofin_domain::{GoFinError, Quantity, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub const BASE_URL: &str = "https://gofin-provider-uniswap-fj4qllww7q-ew.a.run.app";
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum UniswapApiRequest {
    Post {
        path: String,
        body: Value,
    },
    Get {
        path: String,
        query: Vec<(String, String)>,
    },
}
#[async_trait]
pub trait UniswapApiTransport: Send + Sync {
    async fn request(&self, request: UniswapApiRequest) -> Result<Value>;
}
struct HttpTransport;
#[async_trait]
impl UniswapApiTransport for HttpTransport {
    async fn request(&self, request: UniswapApiRequest) -> Result<Value> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| unavailable("Provider transport initialization failed."))?;
        let (builder, submitting, quoting) = match request {
            UniswapApiRequest::Post { path, body } => {
                if !matches!(
                    path.as_str(),
                    "/quote" | "/check_approval" | "/swap" | "/order"
                ) {
                    return Err(invalid("Unsupported provider resource."));
                }
                (
                    client.post(format!("{BASE_URL}{path}")).json(&body),
                    path == "/order",
                    path == "/quote",
                )
            }
            UniswapApiRequest::Get { path, query } => {
                if !matches!(path.as_str(), "/orders" | "/swaps") {
                    return Err(invalid("Unsupported provider resource."));
                }
                (
                    client.get(format!("{BASE_URL}{path}")).query(&query),
                    false,
                    false,
                )
            }
        };
        // Never retry: /order may have accepted an order even when the response
        // is lost. The caller journals its identity and reconciles via /orders.
        let mut response = builder
            .header("accept", "application/json")
            .header("x-universal-router-version", "2.1.2")
            .send()
            .await
            .map_err(|_| transport_error(submitting))?;
        let status = response.status();
        if !status.is_success() && (submitting || !quoting || !status.is_client_error()) {
            return Err(provider_http_error(
                status.as_u16(),
                quoting,
                submitting,
                &[],
            ));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(if submitting {
                transport_error(true)
            } else {
                invalid("Provider response exceeds its size bound.")
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| transport_error(submitting))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(if submitting {
                    transport_error(true)
                } else {
                    invalid("Provider response exceeds its size bound.")
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(provider_http_error(
                status.as_u16(),
                quoting,
                submitting,
                &bytes,
            ));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            if submitting {
                transport_error(true)
            } else {
                invalid("Provider returned invalid JSON.")
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPreference {
    #[default]
    Automatic,
    Amm,
    Uniswapx,
}

#[derive(Clone, Debug)]
pub struct QuoteIntent {
    pub chain_id: u64,
    pub wallet: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: String,
    pub minimum_out: String,
    pub slippage_bps: u16,
    pub max_price_impact_bps: u32,
    pub maximum_fee: String,
    pub maximum_deadline: u64,
    pub routing: RoutingPreference,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidatedQuote {
    // Provider execution metadata includes fractional JSON numbers. Bind its
    // opaque serialized bytes, while financial bounds remain exact typed fields.
    // Restore the original JSON value before sending it back to the provider.
    #[serde(with = "provider_json")]
    pub raw: Value,
    pub routing: String,
    pub quote_id: String,
    pub amount_out: String,
    pub minimum_out: String,
    pub gas_fee: String,
    pub permit_data: Option<Value>,
    pub order_hash: Option<String>,
    pub deadline: Option<u64>,
    /// Absent for UniswapX: priceDifference is not AMM price impact.
    pub price_impact_bps: Option<u32>,
}
mod provider_json {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::Value;

    pub fn serialize<S: Serializer>(value: &Value, serializer: S) -> Result<S::Ok, S::Error> {
        let json = serde_json::to_string(value).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&json)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(json) => serde_json::from_str(&json).map_err(serde::de::Error::custom),
            // Preserve previously stored bindings that used an object. New
            // bindings always serialize provider JSON as an opaque string.
            value @ Value::Object(_) => Ok(value),
            _ => Err(serde::de::Error::custom("Invalid stored provider JSON.")),
        }
    }
}
#[derive(Clone)]
pub struct UniswapApiClient {
    transport: Arc<dyn UniswapApiTransport>,
}
impl Default for UniswapApiClient {
    fn default() -> Self {
        Self::production()
    }
}
impl UniswapApiClient {
    pub fn production() -> Self {
        Self {
            transport: Arc::new(HttpTransport),
        }
    }
    /// Internal fixture injection. Never expose transport selection in CLI input.
    pub fn with_transport(transport: Arc<dyn UniswapApiTransport>) -> Self {
        Self { transport }
    }
    pub async fn quote(&self, intent: &QuoteIntent) -> Result<ValidatedQuote> {
        let body = intent.request()?;
        let raw = self.post("/quote", body).await?;
        validate_quote(raw, intent)
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.transport
            .request(UniswapApiRequest::Post {
                path: path.into(),
                body,
            })
            .await
    }
    pub async fn check_approval(&self, body: Value) -> Result<Value> {
        self.post("/check_approval", body).await
    }
    pub async fn swap(&self, body: Value) -> Result<Value> {
        self.post("/swap", body).await
    }
    pub async fn order(&self, body: Value) -> Result<Value> {
        self.post("/order", body).await
    }
    pub async fn orders(&self, order_id: &str) -> Result<Value> {
        hash(order_id)?;
        self.transport
            .request(UniswapApiRequest::Get {
                path: "/orders".into(),
                query: vec![("orderId".into(), order_id.into())],
            })
            .await
    }
}
impl QuoteIntent {
    pub fn request(&self) -> Result<Value> {
        if !matches!(self.chain_id, 1 | 8453 | 42161 | 4663) || self.slippage_bps > 10_000 {
            return Err(invalid("Unsupported chain or slippage."));
        }
        for address in [&self.wallet, &self.token_in, &self.token_out] {
            normalized_address(address)?;
        }
        if Quantity::new(&self.amount_in)?.is_zero() || Quantity::new(&self.minimum_out)?.is_zero()
        {
            return Err(invalid("Positive input and minimum output are required."));
        }
        Quantity::new(&self.maximum_fee)?;
        let native_input =
            normalized_address(&self.token_in)? == "0x0000000000000000000000000000000000000000";
        if native_input && self.routing == RoutingPreference::Uniswapx {
            return Err(invalid(
                "UniswapX native input requires a supported smart wallet; use wrapped native tokens or AMM routing.",
            ));
        }
        let x_protocol = if self.chain_id == 1 {
            "UNISWAPX_V2"
        } else {
            "UNISWAPX_V3"
        };
        let protocols = match self.routing {
            RoutingPreference::Automatic if !native_input => json!(["V2", "V3", "V4", x_protocol]),
            RoutingPreference::Automatic | RoutingPreference::Amm => json!(["V2", "V3", "V4"]),
            RoutingPreference::Uniswapx => json!([x_protocol]),
        };
        let slippage: Value = serde_json::from_str(&format!(
            "{}.{:02}",
            self.slippage_bps / 100,
            self.slippage_bps % 100
        ))
        .map_err(|_| invalid("Invalid slippage."))?;
        Ok(json!({"type":"EXACT_INPUT", "amount":self.amount_in,
            "tokenInChainId":self.chain_id,"tokenOutChainId":self.chain_id,
            "tokenIn":self.token_in,"tokenOut":self.token_out,"swapper":self.wallet,"recipient":self.wallet,
            "routingPreference":"BEST_PRICE","protocols":protocols,"slippageTolerance":slippage,
            "permitAmount":"EXACT","generatePermitAsTransaction":false,"integratorFees":[]}))
    }
}

pub fn validate_quote(raw: Value, intent: &QuoteIntent) -> Result<ValidatedQuote> {
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    validate_quote_at(raw, intent, now)
}
fn validate_quote_at(raw: Value, intent: &QuoteIntent, now: u64) -> Result<ValidatedQuote> {
    intent.request()?;
    let routing = string(&raw, "routing")?.to_string();
    let is_x = matches!(routing.as_str(), "DUTCH_V2" | "DUTCH_V3");
    if (!is_x && routing != "CLASSIC")
        || (intent.routing == RoutingPreference::Amm && is_x)
        || (intent.routing == RoutingPreference::Uniswapx && !is_x)
    {
        return Err(invalid("Provider selected an unauthorized routing type."));
    }
    if is_x
        && ((intent.chain_id == 1 && routing != "DUTCH_V2")
            || (intent.chain_id != 1 && routing != "DUTCH_V3")
            || normalized_address(&intent.token_in)?
                == "0x0000000000000000000000000000000000000000")
    {
        return Err(invalid(
            "Provider returned an UniswapX protocol not requested for this chain or native input.",
        ));
    }
    let q = &raw["quote"];
    let info = if is_x { &q["orderInfo"] } else { q };
    if uint(info, "chainId")? != intent.chain_id {
        return Err(invalid("Quote chain differs from the intent."));
    }
    same_address(string(info, "swapper")?, &intent.wallet)?;
    if !is_x && string(q, "tradeType")? != "EXACT_INPUT" {
        return Err(invalid("Only exact-input quotes are supported."));
    }
    for key in ["txFailureReasons", "txFailureReason"] {
        if q.get(key).is_some_and(|v| {
            !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()) && v.as_str() != Some("")
        }) {
            return Err(invalid(
                "Provider reports a transaction simulation failure.",
            ));
        }
    }
    let input = &q["input"];
    same_address(string(input, "token")?, &intent.token_in)?;
    let amount_in = Quantity::new(&intent.amount_in)?;
    if quantity(input, "amount")? != amount_in || quantity(input, "maximumAmount")? != amount_in {
        return Err(invalid(
            "Quote input differs from the exact authorized amount.",
        ));
    }
    let output = &q["output"];
    same_address(string(output, "token")?, &intent.token_out)?;
    same_address(string(output, "recipient")?, &intent.wallet)?;
    let mut amount_out = quantity(output, "amount")?;
    let mut minimum_out = quantity(output, "minimumAmount")?;
    // Configured integrator fees are explicitly disabled. Do not silently accept
    // an API-key-level fee that the user has not authorized.
    for key in ["portionBips", "portionAmount"] {
        if let Some(value) = q.get(key).filter(|v| !v.is_null())
            && !decimal(value)?.is_zero()
        {
            return Err(invalid("Unexpected integrator fee in quote."));
        }
    }
    if let Some(outputs) = q.get("aggregatedOutputs") {
        let outputs = outputs
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| invalid("Invalid aggregate outputs."))?;
        let mut aggregate_min = Quantity::zero();
        let mut aggregate_amount = Quantity::zero();
        for output in outputs {
            same_address(string(output, "token")?, &intent.token_out)?;
            same_address(string(output, "recipient")?, &intent.wallet)?;
            if output
                .get("fee")
                .is_some_and(|v| !v.is_null() && v.as_str() != Some("0"))
            {
                return Err(invalid("Unexpected aggregate output fee."));
            }
            aggregate_min = aggregate_min.checked_add(&quantity(output, "minAmount")?)?;
            aggregate_amount = aggregate_amount.checked_add(&quantity(output, "amount")?)?;
        }
        minimum_out = minimum_out.min(aggregate_min);
        amount_out = amount_out.min(aggregate_amount);
    }
    let slip = q
        .get(if is_x {
            "slippageTolerance"
        } else {
            "slippage"
        })
        .ok_or_else(|| invalid("Missing quote slippage."))?;
    if decimal(slip)?
        > Quantity::from_u64(u64::from(intent.slippage_bps))
            .mul_div_floor(&Quantity::new("10000000000000000")?, &Quantity::from_u64(1))?
    {
        return Err(invalid("Quote slippage exceeds authorized tolerance."));
    }
    let price_impact_bps = if let Some(impact) = q.get("priceImpact") {
        let impact = decimal(impact)?;
        let allowed = Quantity::from_u64(u64::from(intent.max_price_impact_bps))
            .mul_div_floor(&Quantity::new("10000000000000000")?, &Quantity::from_u64(1))?;
        if impact > allowed {
            return Err(invalid("Quote price impact exceeds the authorized limit."));
        }
        Some(
            impact
                .mul_div_ceil(&Quantity::from_u64(1), &Quantity::new("10000000000000000")?)?
                .as_str()
                .parse::<u32>()
                .map_err(|_| invalid("Price impact exceeds supported range."))?,
        )
    } else if is_x {
        None
    } else {
        return Err(invalid("Missing AMM price impact."));
    };
    let gas_fee = if is_x {
        Quantity::zero()
    } else {
        quantity(q, "gasFee")?
    };
    if gas_fee > Quantity::new(&intent.maximum_fee)? {
        return Err(invalid("Quote gas fee exceeds the authorized limit."));
    }
    let (order_hash, deadline) = if is_x {
        if normalized_address(string(info, "reactor")?)?
            == "0x0000000000000000000000000000000000000000"
        {
            return Err(invalid("Order reactor must be nonzero for fill recovery."));
        }
        let nonce = info
            .get("nonce")
            .ok_or_else(|| invalid("Missing order nonce."))?;
        let nonce = Quantity::new(
            nonce
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| nonce.to_string()),
        )?;
        if nonce
            > Quantity::new(
                "115792089237316195423570985008687907853269984665640564039457584007913129639935",
            )?
        {
            return Err(invalid("Order nonce exceeds uint256."));
        }
        let deadline = uint(info, "deadline")?;
        if deadline <= now || deadline > intent.maximum_deadline {
            return Err(invalid("Order expiry is outside the authorized window."));
        }
        let order_input = &info["input"];
        same_address(string(order_input, "token")?, &intent.token_in)?;
        if quantity(order_input, "startAmount")? != amount_in
            || quantity(
                order_input,
                if routing == "DUTCH_V2" {
                    "endAmount"
                } else {
                    "maxAmount"
                },
            )? != amount_in
        {
            return Err(invalid("Order input exceeds exact-input intent."));
        }
        let outputs = info["outputs"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| invalid("Missing order outputs."))?;
        let mut floor = Quantity::zero();
        for output in outputs {
            same_address(string(output, "token")?, &intent.token_out)?;
            same_address(string(output, "recipient")?, &intent.wallet)?;
            let start = quantity(output, "startAmount")?;
            // V3 minAmount is a hard contract floor, including nonlinear decay
            // and base-fee changes. Exclusivity overrides only improve output.
            let end = quantity(
                output,
                if routing == "DUTCH_V2" {
                    "endAmount"
                } else {
                    "minAmount"
                },
            )?;
            floor = floor.checked_add(&start.min(end))?;
        }
        minimum_out = minimum_out.min(floor);
        let id = string(q, "orderId")?;
        hash(id)?;
        if raw.get("permitData").is_none_or(|v| !v.is_object()) {
            return Err(invalid("Missing order signing data."));
        }
        (Some(id.to_owned()), Some(deadline))
    } else {
        (None, None)
    };
    if minimum_out.is_zero()
        || minimum_out > amount_out
        || minimum_out < Quantity::new(&intent.minimum_out)?
    {
        return Err(invalid(
            "Quote worst-case output fails the authorized minimum.",
        ));
    }
    let quote_id = string(q, "quoteId")?.to_owned();
    let permit_data = raw.get("permitData").filter(|v| !v.is_null()).cloned();
    Ok(ValidatedQuote {
        raw,
        routing,
        quote_id,
        amount_out: amount_out.to_string(),
        minimum_out: minimum_out.to_string(),
        gas_fee: gas_fee.to_string(),
        permit_data,
        order_hash,
        deadline,
        price_impact_bps,
    })
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(&format!("Missing or invalid {key}.")))
}
fn quantity(v: &Value, key: &str) -> Result<Quantity> {
    Quantity::new(string(v, key)?)
}
fn uint(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .ok_or_else(|| invalid(&format!("Missing or invalid {key}.")))
}
fn decimal(v: &Value) -> Result<Quantity> {
    let s = if let Some(s) = v.as_str() {
        s.to_owned()
    } else if v.is_number() {
        v.to_string()
    } else {
        return Err(invalid("Invalid decimal metadata."));
    };
    Quantity::from_decimal(&s, 18)
}
fn normalized_address(s: &str) -> Result<String> {
    if s.len() != 42 || !s.starts_with("0x") || !s[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("Invalid address metadata."));
    }
    Ok(s.to_ascii_lowercase())
}
fn same_address(a: &str, b: &str) -> Result<()> {
    if normalized_address(a)? != normalized_address(b)? {
        return Err(invalid("Quote address differs from authorized intent."));
    }
    Ok(())
}
fn hash(s: &str) -> Result<()> {
    if s.len() != 66 || !s.starts_with("0x") || !s[2..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("Invalid order identity."));
    }
    Ok(())
}
fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "UNISWAP_API_INVALID",
        "validation",
        message,
        "Refresh the quote within the approved financial limits.",
    )
}
fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "UNISWAP_API_UNAVAILABLE",
        "provider",
        message,
        "Retry the read; reconcile any existing submission before another write.",
    )
}
fn provider_http_error(status: u16, quoting: bool, submitting: bool, body: &[u8]) -> GoFinError {
    if submitting {
        return transport_error(true);
    }
    // Match a known code only; upstream details can contain untrusted text or
    // sensitive request data and must never be reflected in CLI errors.
    if quoting
        && (400..500).contains(&status)
        && serde_json::from_slice::<Value>(body)
            .is_ok_and(|value| value["errorCode"] == "QuoteAmountTooLowError")
    {
        return GoFinError::new(
            "UNISWAP_QUOTE_AMOUNT_TOO_LOW",
            "validation",
            "The requested amount is below Uniswap's minimum quote size for the selected routing protocols on this chain.",
            "Use AMM routing, or request a larger quote only within the human-approved amount and financial limits.",
        );
    }
    unavailable(&format!("Uniswap provider returned HTTP {status}."))
}
fn transport_error(submitting: bool) -> GoFinError {
    if submitting {
        GoFinError::new(
            "UNISWAP_ORDER_UNKNOWN",
            "submission",
            "Order submission outcome is uncertain.",
            "Reconcile the persisted order identity; do not submit a replacement.",
        )
    } else {
        unavailable("Uniswap provider request failed.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    const WALLET: &str = "0x1111111111111111111111111111111111111111";
    const INPUT: &str = "0x2222222222222222222222222222222222222222";
    const OUTPUT: &str = "0x3333333333333333333333333333333333333333";
    fn intent() -> QuoteIntent {
        QuoteIntent {
            chain_id: 1,
            wallet: WALLET.into(),
            token_in: INPUT.into(),
            token_out: OUTPUT.into(),
            amount_in: "1000".into(),
            minimum_out: "990".into(),
            slippage_bps: 100,
            max_price_impact_bps: 50,
            maximum_fee: "100".into(),
            maximum_deadline: 2000,
            routing: RoutingPreference::Automatic,
        }
    }
    fn classic() -> Value {
        json!({"routing":"CLASSIC","permitData":null,"quote":{
            "chainId":1,"swapper":WALLET,"tradeType":"EXACT_INPUT","quoteId":"fixture",
            "input":{"token":INPUT,"amount":"1000","maximumAmount":"1000"},
            "output":{"token":OUTPUT,"recipient":WALLET,"amount":"1000","minimumAmount":"990"},
            "aggregatedOutputs":[{"token":OUTPUT,"recipient":WALLET,"amount":"1000","minAmount":"990"}],
            "slippage":1,"priceImpact":0.5,"gasFee":"100","txFailureReasons":[]}})
    }
    fn dutch(v3: bool) -> Value {
        let mut value = classic();
        value["routing"] = json!(if v3 { "DUTCH_V3" } else { "DUTCH_V2" });
        value["permitData"] = json!({"domain":{},"values":{},"types":{}});
        let q = value["quote"].as_object_mut().unwrap();
        q.remove("priceImpact");
        q.remove("gasFee");
        q.insert("slippageTolerance".into(), json!(1));
        q.insert("orderId".into(), json!(format!("0x{}", "ab".repeat(32))));
        q.insert("orderInfo".into(),json!({"chainId":1,"swapper":WALLET,"deadline":1500,"reactor":INPUT,"nonce":"1",
            "input":{"token":INPUT,"startAmount":"1000","endAmount":"1000","maxAmount":"1000"},
            "outputs":[{"token":OUTPUT,"recipient":WALLET,"startAmount":"1000","endAmount":"990","minAmount":"990"}]}));
        if v3 {
            value["quote"]["orderInfo"]["chainId"] = json!(42161);
        }
        value
    }
    fn matching_intent(quote: &Value) -> QuoteIntent {
        let mut i = intent();
        if quote["routing"] == "DUTCH_V3" {
            i.chain_id = 42161;
        }
        i
    }
    #[test]
    fn binding_keeps_provider_fractions_opaque_and_restores_numeric_api_payload() {
        let mut raw = classic();
        raw["quote"]["slippage"] = json!(0.5);
        raw["quote"]["priceDifference"] = json!(-0.012345);
        raw["quote"]["gasEstimates"] = json!([{"strategy":{
            "limitInflationFactor":1.15,"maxPriorityFeeGwei":0.1,
            "baseFeeMultiplier":1.2,"minPriorityFeeRatioOfBaseFee":0.1
        }}]);
        let quote = validate_quote_at(raw.clone(), &intent(), 1000).unwrap();
        assert!(gofin_domain::binding_hash(&raw).is_err());
        let binding = serde_json::to_value(&quote).unwrap();
        let hash = gofin_domain::binding_hash(&binding).unwrap();
        assert!(binding["raw"].is_string());
        let restored: ValidatedQuote = serde_json::from_value(binding.clone()).unwrap();
        assert_eq!(restored, quote);
        let request = json!({"quote":restored.raw["quote"]});
        assert_eq!(request["quote"], raw["quote"]);
        assert!(request["quote"]["slippage"].is_f64());
        let mut changed = restored;
        changed.raw["quote"]["gasEstimates"][0]["strategy"]["baseFeeMultiplier"] = json!(1.3);
        assert_ne!(
            gofin_domain::binding_hash(&serde_json::to_value(changed).unwrap()).unwrap(),
            hash
        );
        let mut legacy = binding;
        legacy["raw"] = raw;
        assert_eq!(
            serde_json::from_value::<ValidatedQuote>(legacy).unwrap(),
            quote
        );
    }
    #[test]
    fn validates_each_execution_model_and_uses_order_floor() {
        for quote in [classic(), dutch(false), dutch(true)] {
            let validated =
                validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).unwrap();
            assert_eq!(validated.minimum_out, "990");
        }
        for v3 in [false, true] {
            let mut quote = dutch(v3);
            quote["quote"]["orderInfo"]["outputs"][0][if v3 { "minAmount" } else { "endAmount" }] =
                json!("989");
            assert!(validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).is_err());
        }
    }
    #[test]
    fn rejects_intent_changes_and_excess_limits_without_decoding() {
        for (pointer, replacement) in [
            ("/quote/chainId", json!(8453)),
            ("/quote/swapper", json!(OUTPUT)),
            ("/quote/input/token", json!(OUTPUT)),
            ("/quote/output/recipient", json!(INPUT)),
            ("/quote/input/maximumAmount", json!("1001")),
            ("/quote/slippage", json!(1.01)),
            ("/quote/priceImpact", json!(0.5001)),
            ("/quote/gasFee", json!("101")),
            ("/quote/output/minimumAmount", json!("989")),
            ("/quote/txFailureReasons", json!(["FAILED"])),
            ("/quote/aggregatedOutputs/0/recipient", json!(INPUT)),
        ] {
            let mut quote = classic();
            *quote.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).is_err(),
                "{pointer}"
            );
        }
        let mut quote = classic();
        quote["quote"]["portionAmount"] = json!("1");
        assert!(validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).is_err());
        let mut quote = classic();
        quote["quote"]["arbitraryCalldata"] = json!("0xdeadbeef");
        assert!(validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).is_ok());
    }
    #[test]
    fn dutch_expiry_input_and_preference_are_bound() {
        for v3 in [false, true] {
            for (field, value) in [
                (
                    "reactor",
                    json!("0x0000000000000000000000000000000000000000"),
                ),
                ("nonce", json!("-1")),
            ] {
                let mut quote = dutch(v3);
                quote["quote"]["orderInfo"][field] = value;
                assert!(validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).is_err());
            }
            for deadline in [999, 1000, 2001] {
                let mut quote = dutch(v3);
                quote["quote"]["orderInfo"]["deadline"] = json!(deadline);
                assert!(validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).is_err());
            }
            let mut quote = dutch(v3);
            quote["quote"]["orderInfo"]["input"][if v3 { "maxAmount" } else { "endAmount" }] =
                json!("1001");
            assert!(validate_quote_at(quote.clone(), &matching_intent(&quote), 1000).is_err());
        }
        let mut intent = intent();
        intent.routing = RoutingPreference::Amm;
        assert!(validate_quote_at(dutch(false), &intent, 1000).is_err());
        intent.routing = RoutingPreference::Uniswapx;
        assert!(validate_quote_at(classic(), &intent, 1000).is_err());
    }
    #[test]
    fn minimum_quote_error_is_actionable_without_reflecting_provider_details() {
        let body = br#"{"errorCode":"QuoteAmountTooLowError","detail":"secret request data"}"#;
        let error = provider_http_error(404, true, false, body);
        assert_eq!(error.code, "UNISWAP_QUOTE_AMOUNT_TOO_LOW");
        assert_eq!(error.stage, "validation");
        assert!(error.next_action.contains("AMM routing"));
        assert!(error.next_action.contains("human-approved"));
        assert!(!format!("{error:?}").contains("secret request data"));
        for (status, quoting, body) in [
            (404, false, body.as_slice()),
            (503, true, body.as_slice()),
            (
                404,
                true,
                br#"{"errorCode":"secret request data"}"#.as_slice(),
            ),
            (404, true, b"secret request data".as_slice()),
        ] {
            let error = provider_http_error(status, quoting, false, body);
            assert_eq!(error.code, "UNISWAP_API_UNAVAILABLE");
            assert!(!format!("{error:?}").contains("secret request data"));
        }
        let error = provider_http_error(404, false, true, body);
        assert_eq!(error.code, "UNISWAP_ORDER_UNKNOWN");
        assert!(error.next_action.contains("do not submit a replacement"));
    }
    struct Fixture {
        calls: Mutex<Vec<UniswapApiRequest>>,
        response: Value,
        fail: bool,
    }
    #[async_trait]
    impl UniswapApiTransport for Fixture {
        async fn request(&self, request: UniswapApiRequest) -> Result<Value> {
            self.calls.lock().unwrap().push(request);
            if self.fail {
                Err(transport_error(true))
            } else {
                Ok(self.response.clone())
            }
        }
    }
    #[tokio::test]
    async fn exact_permissions_best_price_and_no_write_retries() {
        let fixture = Arc::new(Fixture {
            calls: Mutex::new(vec![]),
            response: classic(),
            fail: false,
        });
        let client = UniswapApiClient::with_transport(fixture.clone());
        client.quote(&intent()).await.unwrap();
        match &fixture.calls.lock().unwrap()[0] {
            UniswapApiRequest::Post { path, body } => {
                assert_eq!(path, "/quote");
                assert_eq!(body["permitAmount"], "EXACT");
                assert_eq!(body["routingPreference"], "BEST_PRICE");
                assert_eq!(body["protocols"], json!(["V2", "V3", "V4", "UNISWAPX_V2"]));
            }
            _ => panic!("wrong request"),
        }
        let fixture = Arc::new(Fixture {
            calls: Mutex::new(vec![]),
            response: Value::Null,
            fail: true,
        });
        let client = UniswapApiClient::with_transport(fixture.clone());
        assert_eq!(
            client.order(json!({})).await.unwrap_err().code,
            "UNISWAP_ORDER_UNKNOWN"
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    }
}
