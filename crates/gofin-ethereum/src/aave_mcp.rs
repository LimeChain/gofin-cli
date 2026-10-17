//! Strict client for the official Aave MCP transaction builder.
//!
//! MCP output is treated only as a source of candidate calldata. The local
//! Pool ABI decoder below is the authority for the call GoFin may simulate and
//! freeze. The HTTP transport always targets Aave's fixed HTTPS endpoint.

use crate::abi::Pool;
use async_trait::async_trait;
use gofin_evm::alloy::{
    primitives::{Address, Bytes, U256},
    sol_types::SolCall,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;

const ENDPOINT: &str = "https://mcp.aave.com/";
const PROTOCOL_VERSION: &str = "2025-11-25";
const MAX_RESPONSE_BYTES: usize = 1_048_576;
const MAX_TOOL_CATALOG_BYTES: usize = 1_048_576;
const MAX_TOOL_COUNT: usize = 256;
const MAX_TOOL_PAGES: usize = 8;
const MAX_READ_ARGUMENT_BYTES: usize = 16_384;
const MAX_SCHEMA_DEPTH: usize = 12;
const MAX_JSON_DEPTH: usize = 24;
const MAX_PAGE_SIZE: u64 = 100;
const READ_CATALOG_TTL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AaveMcpAction {
    Supply,
    Borrow,
    Repay,
    Withdraw,
    Collateral,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AaveMcpAmount {
    Exact(U256),
    /// The transaction uses Aave's max-uint sentinel. `cap` is retained by
    /// the caller's existing state/risk checks and is not delegated upstream.
    Max {
        cap: U256,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AaveMcpIntent {
    pub chain_id: u64,
    pub pool: Address,
    pub owner: Address,
    pub token: Address,
    pub action: AaveMcpAction,
    pub amount: Option<AaveMcpAmount>,
    pub recipient: Option<Address>,
    pub enabled: Option<bool>,
    pub decimals: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AaveMcpCall {
    pub calldata: Bytes,
}

/// An MCP tool definition that passed GoFin's local read-only classifier.
/// Descriptions are upstream text and must be treated as untrusted metadata.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AaveMcpReadTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// Read tools are discovered from the live official catalog. The provenance
/// explicitly says that returned protocol data has not been verified by GoFin.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AaveMcpReadCatalog {
    pub tools: Vec<AaveMcpReadTool>,
    pub provenance: AaveMcpReadProvenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AaveMcpReadProvenance {
    UnverifiedOfficialMcp,
}

/// The complete MCP `result` object is retained, including content,
/// structuredContent, warnings, coverage, pagination cursors, and `isError`.
/// Any transaction-like values in a read response are inert data only.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AaveMcpReadResult {
    pub tool_name: String,
    pub result: Value,
    pub provenance: AaveMcpReadProvenance,
}

#[derive(Debug, Error)]
pub enum AaveMcpError {
    #[error("Aave MCP read arguments were invalid: {0}")]
    InvalidReadArguments(String),
    #[error("Aave MCP transport failed: {0}")]
    Transport(String),
    #[error("Aave MCP response was invalid: {0}")]
    Protocol(String),
    #[error("Aave MCP did not return a single direct Pool transaction: {0}")]
    UnsupportedPlan(String),
    #[error("Aave MCP transaction did not match the requested Aave operation: {0}")]
    Mismatch(String),
}

#[derive(Clone, Debug)]
pub struct McpHttpRequest {
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct McpHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[async_trait]
pub trait AaveMcpTransport: Send + Sync {
    /// POST to the configured Aave MCP endpoint. The request intentionally
    /// carries no URL so callers cannot redirect the production client.
    async fn post(&self, request: McpHttpRequest) -> Result<McpHttpResponse, AaveMcpError>;
}

#[derive(Clone)]
pub struct AaveMcpClient {
    transport: Arc<dyn AaveMcpTransport>,
    read_catalog_cache: Arc<Mutex<Option<(Instant, AaveMcpReadCatalog)>>>,
}

impl std::fmt::Debug for AaveMcpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AaveMcpClient")
            .finish_non_exhaustive()
    }
}

impl Default for AaveMcpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AaveMcpClient {
    pub fn new() -> Self {
        Self {
            transport: Arc::new(HttpAaveMcpTransport::new()),
            read_catalog_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// Dependency injection for deterministic tests and adapter composition.
    /// The operation still passes through local ABI and intent validation.
    pub fn with_transport(transport: Arc<dyn AaveMcpTransport>) -> Self {
        Self {
            transport,
            read_catalog_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// Discover tools approved by the local read-only policy. Server
    /// annotations are necessary but never sufficient: GoFin also requires a
    /// safe name family, a bounded closed input schema, and no mutating-family
    /// tokens. The 30-second cache is only for discovery; calls re-fetch and
    /// classify the catalog in their own MCP session.
    pub async fn discover_read_tools(&self) -> Result<AaveMcpReadCatalog, AaveMcpError> {
        if let Ok(cache) = self.read_catalog_cache.lock()
            && let Some((created, catalog)) = cache.as_ref()
            && created.elapsed() < READ_CATALOG_TTL
        {
            return Ok(catalog.clone());
        }
        self.refresh_read_tools().await
    }

    /// Force a live catalog request, bypassing the short discovery cache.
    /// Failed refreshes clear the old entry so callers cannot mistake stale
    /// availability or schemas for current MCP capabilities.
    pub async fn refresh_read_tools(&self) -> Result<AaveMcpReadCatalog, AaveMcpError> {
        if let Ok(mut cache) = self.read_catalog_cache.lock() {
            *cache = None;
        }
        let catalog = self.fetch_read_catalog().await?;
        if let Ok(mut cache) = self.read_catalog_cache.lock() {
            *cache = Some((Instant::now(), catalog.clone()));
        }
        Ok(catalog)
    }

    /// Call one dynamically discovered read tool. A fresh tools/list exchange
    /// is made in the same initialized session as the call so a server-side
    /// catalog change cannot bypass the local classifier. No pagination is
    /// followed automatically; cursors and coverage remain visible to caller.
    pub async fn call_read_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<AaveMcpReadResult, AaveMcpError> {
        if tool_name.len() > 128 || !allowed_read_tool_name(tool_name) {
            return Err(AaveMcpError::UnsupportedPlan(
                "tool name is outside the local read-only policy".into(),
            ));
        }
        let arg_bytes = serde_json::to_vec(&arguments).map_err(|_| {
            AaveMcpError::InvalidReadArguments("read arguments are not JSON".into())
        })?;
        if arg_bytes.len() > MAX_READ_ARGUMENT_BYTES || json_depth(&arguments) > MAX_JSON_DEPTH {
            return Err(AaveMcpError::InvalidReadArguments(
                "read arguments exceed the local size or nesting bound".into(),
            ));
        }
        enforce_pagination_bounds(&arguments)?;

        let mut session = McpSession::new(self.transport.clone());
        session.initialize().await?;
        let catalog = session.list_read_tools().await?;
        let tool = catalog
            .tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .ok_or_else(|| {
                AaveMcpError::UnsupportedPlan(
                    "tool is missing or is not classified as read-only".into(),
                )
            })?;
        validate_schema_definition(&tool.input_schema)?;
        validate_arguments(&tool.input_schema, &arguments)?;
        let result = session.call_tool_result(tool_name, arguments).await?;
        validate_read_call_tool_result(&result)?;
        Ok(AaveMcpReadResult {
            tool_name: tool_name.to_owned(),
            result,
            provenance: AaveMcpReadProvenance::UnverifiedOfficialMcp,
        })
    }

    async fn fetch_read_catalog(&self) -> Result<AaveMcpReadCatalog, AaveMcpError> {
        let mut session = McpSession::new(self.transport.clone());
        session.initialize().await?;
        session.list_read_tools().await
    }

    pub async fn prepare(&self, intent: &AaveMcpIntent) -> Result<AaveMcpCall, AaveMcpError> {
        validate_intent(intent)?;
        let mut session = McpSession::new(self.transport.clone());
        session.initialize().await?;
        let params = tool_arguments(intent)?;
        let data = session.call_tool(tool_name(intent.action), params).await?;
        let transaction = extract_single_transaction(&data)?;
        let calldata = decode_calldata_field(&transaction["data"])?;
        validate_mcp_transaction(intent, &transaction, &calldata)?;
        Ok(AaveMcpCall { calldata })
    }
}

/// Revalidate frozen or received calldata locally, without contacting Aave.
/// Use this again at the final signing boundary.
pub fn validate_calldata(intent: &AaveMcpIntent, calldata: &Bytes) -> Result<(), AaveMcpError> {
    validate_intent(intent)?;
    let mismatch = |why: &str| AaveMcpError::Mismatch(why.to_owned());
    match intent.action {
        AaveMcpAction::Supply => {
            let call = Pool::supplyCall::abi_decode_validate(calldata)
                .map_err(|_| mismatch("supply calldata failed strict Pool ABI decoding"))?;
            require_canonical(calldata, call.abi_encode())?;
            expect_address(call.asset, intent.token, "asset")?;
            expect_amount(call.amount, intent.amount, "amount")?;
            expect_address(call.onBehalfOf, intent.owner, "onBehalfOf")?;
            if call.referralCode != 0 {
                return Err(mismatch("referralCode must be zero"));
            }
        }
        AaveMcpAction::Borrow => {
            let call = Pool::borrowCall::abi_decode_validate(calldata)
                .map_err(|_| mismatch("borrow calldata failed strict Pool ABI decoding"))?;
            require_canonical(calldata, call.abi_encode())?;
            expect_address(call.asset, intent.token, "asset")?;
            expect_amount(call.amount, intent.amount, "amount")?;
            if call.interestRateMode != U256::from(2) {
                return Err(mismatch("interestRateMode must be variable rate (2)"));
            }
            if call.referralCode != 0 {
                return Err(mismatch("referralCode must be zero"));
            }
            expect_address(call.onBehalfOf, intent.owner, "onBehalfOf")?;
        }
        AaveMcpAction::Repay => {
            let call = Pool::repayCall::abi_decode_validate(calldata)
                .map_err(|_| mismatch("repay calldata failed strict Pool ABI decoding"))?;
            require_canonical(calldata, call.abi_encode())?;
            expect_address(call.asset, intent.token, "asset")?;
            expect_amount(call.amount, intent.amount, "amount")?;
            if call.interestRateMode != U256::from(2) {
                return Err(mismatch("interestRateMode must be variable rate (2)"));
            }
            expect_address(call.onBehalfOf, intent.owner, "onBehalfOf")?;
        }
        AaveMcpAction::Withdraw => {
            let call = Pool::withdrawCall::abi_decode_validate(calldata)
                .map_err(|_| mismatch("withdraw calldata failed strict Pool ABI decoding"))?;
            require_canonical(calldata, call.abi_encode())?;
            expect_address(call.asset, intent.token, "asset")?;
            expect_amount(call.amount, intent.amount, "amount")?;
            expect_address(
                call.to,
                intent
                    .recipient
                    .ok_or_else(|| mismatch("withdraw recipient is missing"))?,
                "recipient",
            )?;
        }
        AaveMcpAction::Collateral => {
            let call = Pool::setUserUseReserveAsCollateralCall::abi_decode_validate(calldata)
                .map_err(|_| mismatch("collateral calldata failed strict Pool ABI decoding"))?;
            require_canonical(calldata, call.abi_encode())?;
            expect_address(call.asset, intent.token, "asset")?;
            if Some(call.useAsCollateral) != intent.enabled {
                return Err(mismatch("useAsCollateral differs from the requested state"));
            }
        }
    }
    Ok(())
}

fn require_canonical(calldata: &Bytes, encoded: Vec<u8>) -> Result<(), AaveMcpError> {
    if calldata.as_ref() == encoded.as_slice() {
        Ok(())
    } else {
        Err(AaveMcpError::Mismatch(
            "calldata contains noncanonical or trailing ABI bytes".into(),
        ))
    }
}

/// Validate every frozen transaction field and the locally decoded calldata.
/// Adapters should call this immediately before passing a frozen Aave call to
/// the signing runtime.
pub fn validate_transaction_fields(
    intent: &AaveMcpIntent,
    chain_id: u64,
    to: Address,
    sender: Address,
    value: U256,
    calldata: &Bytes,
) -> Result<(), AaveMcpError> {
    validate_intent(intent)?;
    if chain_id != intent.chain_id {
        return Err(AaveMcpError::Mismatch(
            "chainId differs from the configured chain".into(),
        ));
    }
    expect_address(to, intent.pool, "to")?;
    expect_address(sender, intent.owner, "from")?;
    if !value.is_zero() {
        return Err(AaveMcpError::Mismatch(
            "Pool transaction value must be zero".into(),
        ));
    }
    validate_calldata(intent, calldata)
}

fn validate_intent(intent: &AaveMcpIntent) -> Result<(), AaveMcpError> {
    let bad = |s: &str| AaveMcpError::Mismatch(s.to_owned());
    if intent.chain_id == 0
        || intent.pool == Address::ZERO
        || intent.owner == Address::ZERO
        || intent.token == Address::ZERO
    {
        return Err(bad("chain, Pool, owner, and token must be nonzero"));
    }
    if intent.decimals > 77 {
        return Err(bad("token decimals exceed the exact U256 conversion bound"));
    }
    match intent.action {
        AaveMcpAction::Collateral => {
            if intent.amount.is_some() || intent.enabled.is_none() || intent.recipient.is_some() {
                return Err(bad("collateral requires only an enabled state"));
            }
        }
        AaveMcpAction::Withdraw => {
            if intent.amount.is_none() || intent.enabled.is_some() || intent.recipient.is_none() {
                return Err(bad("withdraw requires an amount and recipient"));
            }
        }
        _ => {
            if intent.amount.is_none() || intent.enabled.is_some() || intent.recipient.is_some() {
                return Err(bad(
                    "movement action has inconsistent optional intent fields",
                ));
            }
        }
    }
    if let Some(amount) = intent.amount {
        match amount {
            AaveMcpAmount::Exact(q) if q.is_zero() || q == U256::MAX => {
                return Err(bad(
                    "exact amount must be positive and below the all sentinel",
                ));
            }
            AaveMcpAmount::Max { cap } if cap.is_zero() || cap == U256::MAX => {
                return Err(bad(
                    "max amount needs a finite positive cap below the sentinel",
                ));
            }
            _ => {}
        }
    }
    if matches!(intent.amount, Some(AaveMcpAmount::Max { .. }))
        && !matches!(
            intent.action,
            AaveMcpAction::Repay | AaveMcpAction::Withdraw
        )
    {
        return Err(bad(
            "Aave MCP all semantics are supported only for repay and withdraw",
        ));
    }
    Ok(())
}

fn tool_name(action: AaveMcpAction) -> &'static str {
    match action {
        AaveMcpAction::Collateral => "prepare_set_collateral",
        _ => "prepare_action",
    }
}

fn tool_arguments(intent: &AaveMcpIntent) -> Result<Value, AaveMcpError> {
    let mut args = json!({
        "version": "v3",
        "sender": format!("{:#x}", intent.owner),
        "market": format!("{:#x}", intent.pool),
        "token": format!("{:#x}", intent.token),
        "chainId": intent.chain_id,
    });
    if intent.action == AaveMcpAction::Collateral {
        args["enable"] = json!(intent.enabled.expect("validated"));
    } else {
        args["action"] = json!(match intent.action {
            AaveMcpAction::Supply => "supply",
            AaveMcpAction::Borrow => "borrow",
            AaveMcpAction::Repay => "repay",
            AaveMcpAction::Withdraw => "withdraw",
            AaveMcpAction::Collateral => unreachable!(),
        });
        match intent.amount.expect("validated") {
            AaveMcpAmount::Exact(amount) => {
                args["amount"] = json!(format_units(amount, intent.decimals))
            }
            AaveMcpAmount::Max { .. } => args["max"] = json!(true),
        }
        if intent.action == AaveMcpAction::Supply {
            // Preserve the existing operation's separate collateral toggle.
            args["enableCollateral"] = json!(false);
        }
    }
    Ok(args)
}

fn format_units(amount: U256, decimals: u8) -> String {
    let raw = amount.to_string();
    if decimals == 0 {
        return raw;
    }
    let decimals = decimals as usize;
    let padded = if raw.len() <= decimals {
        format!("{}{}", "0".repeat(decimals + 1 - raw.len()), raw)
    } else {
        raw
    };
    let split = padded.len() - decimals;
    let whole = &padded[..split];
    let fractional = padded[split..].trim_end_matches('0');
    if fractional.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fractional}")
    }
}

fn expect_address(actual: Address, expected: Address, field: &str) -> Result<(), AaveMcpError> {
    if actual == expected {
        Ok(())
    } else {
        Err(AaveMcpError::Mismatch(format!(
            "{field} differs from the requested value"
        )))
    }
}

fn expect_amount(
    actual: U256,
    expected: Option<AaveMcpAmount>,
    field: &str,
) -> Result<(), AaveMcpError> {
    let matches =
        match expected.ok_or_else(|| AaveMcpError::Mismatch(format!("{field} is missing")))? {
            AaveMcpAmount::Exact(value) => actual == value,
            AaveMcpAmount::Max { .. } => actual == U256::MAX,
        };
    if matches {
        Ok(())
    } else {
        Err(AaveMcpError::Mismatch(format!(
            "{field} differs from the requested amount semantics"
        )))
    }
}

fn extract_single_transaction(data: &Value) -> Result<Value, AaveMcpError> {
    match data.get("__typename").and_then(Value::as_str) {
        Some("TransactionRequest") => Ok(data.clone()),
        Some("ApprovalRequired") => Err(AaveMcpError::UnsupportedPlan(
            "Aave returned an approval-first plan; GoFin requires its independently reviewed single Pool call and never signs MCP approval steps".into(),
        )),
        Some(_) => Err(AaveMcpError::UnsupportedPlan(
            "unsupported Aave MCP plan type".into(),
        )),
        None => Err(AaveMcpError::Protocol("transaction result has no typed envelope".into())),
    }
}

fn validate_mcp_transaction(
    intent: &AaveMcpIntent,
    tx: &Value,
    calldata: &Bytes,
) -> Result<(), AaveMcpError> {
    let mismatch = |s: &str| AaveMcpError::Mismatch(s.to_owned());
    let chain = tx
        .get("chainId")
        .and_then(Value::as_u64)
        .ok_or_else(|| mismatch("chainId is missing or invalid"))?;
    if chain != intent.chain_id {
        return Err(mismatch("chainId differs from the configured chain"));
    }
    let to = parse_address_field(tx.get("to"), "to")?;
    expect_address(to, intent.pool, "to")?;
    let sender = parse_address_field(tx.get("from"), "from")?;
    expect_address(sender, intent.owner, "from")?;
    let value = parse_uint_field(tx.get("value"), "value")?;
    if !value.is_zero() {
        return Err(mismatch("Pool transaction value must be zero"));
    }
    // Aave's documented TransactionRequest fields do not define `operation`.
    // Some responses include it as extra metadata, but action authority comes
    // from the locally decoded Pool selector and arguments below.
    validate_transaction_fields(intent, chain, to, sender, value, calldata)
}

fn decode_calldata_field(value: &Value) -> Result<Bytes, AaveMcpError> {
    let text = value
        .as_str()
        .ok_or_else(|| AaveMcpError::Protocol("transaction data is missing".into()))?;
    let raw = text
        .strip_prefix("0x")
        .ok_or_else(|| AaveMcpError::Protocol("transaction data must be 0x-prefixed".into()))?;
    if raw.len() % 2 != 0 || raw.len() > 16_384 {
        return Err(AaveMcpError::Protocol(
            "transaction data has an invalid length".into(),
        ));
    }
    let bytes = hex::decode(raw)
        .map_err(|_| AaveMcpError::Protocol("transaction data is not hexadecimal".into()))?;
    Ok(Bytes::from(bytes))
}

fn parse_address_field(value: Option<&Value>, field: &str) -> Result<Address, AaveMcpError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| AaveMcpError::Protocol(format!("{field} is missing")))?;
    text.parse::<Address>()
        .map_err(|_| AaveMcpError::Protocol(format!("{field} is not an address")))
}

fn parse_uint_field(value: Option<&Value>, field: &str) -> Result<U256, AaveMcpError> {
    let text = match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => return Err(AaveMcpError::Protocol(format!("{field} is missing"))),
    };
    U256::from_str_radix(&text, 10)
        .map_err(|_| AaveMcpError::Protocol(format!("{field} is not an unsigned integer")))
}

struct McpSession {
    transport: Arc<dyn AaveMcpTransport>,
    next_id: u64,
    session_id: Option<String>,
}

impl McpSession {
    fn new(transport: Arc<dyn AaveMcpTransport>) -> Self {
        Self {
            transport,
            next_id: 0,
            session_id: None,
        }
    }

    async fn initialize(&mut self) -> Result<(), AaveMcpError> {
        let id = self.id();
        let response = self.request(json!({
            "jsonrpc":"2.0", "id": id, "method":"initialize",
            "params":{"protocolVersion":PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"gofin-cli","version":env!("CARGO_PKG_VERSION")}}
        }), false).await?;
        self.session_id = header(&response.headers, "mcp-session-id");
        let message = parse_message(&response)?;
        if message.get("error").is_some() {
            return Err(rpc_error(&message));
        }
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(AaveMcpError::Protocol(
                "initialize response id does not match request".into(),
            ));
        }
        let result = message
            .get("result")
            .ok_or_else(|| AaveMcpError::Protocol("initialize result is missing".into()))?;
        if result["protocolVersion"].as_str() != Some(PROTOCOL_VERSION) {
            return Err(AaveMcpError::Protocol(
                "server negotiated an unexpected protocol version".into(),
            ));
        }
        if result["serverInfo"]["name"].as_str() != Some("aave-mcp") {
            return Err(AaveMcpError::Protocol(
                "unexpected MCP server identity".into(),
            ));
        }
        self.notify_initialized().await?;
        Ok(())
    }

    async fn notify_initialized(&self) -> Result<(), AaveMcpError> {
        let mut headers = BTreeMap::from([
            ("content-type".into(), "application/json".into()),
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
            ("mcp-protocol-version".into(), PROTOCOL_VERSION.into()),
        ]);
        if let Some(session) = &self.session_id {
            headers.insert("mcp-session-id".into(), session.clone());
        }
        let body = serde_json::to_vec(&json!({
            "jsonrpc":"2.0", "method":"notifications/initialized"
        }))
        .map_err(|e| AaveMcpError::Protocol(e.to_string()))?;
        let response = self
            .transport
            .post(McpHttpRequest { headers, body })
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(AaveMcpError::Transport(format!(
                "initialized notification HTTP status {}",
                response.status
            )));
        }
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(AaveMcpError::Protocol(
                "response exceeded the one MiB bound".into(),
            ));
        }
        Ok(())
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, AaveMcpError> {
        let result = self.call_tool_result(name, arguments).await?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(AaveMcpError::UnsupportedPlan(
                "Aave MCP tool returned an error result".into(),
            ));
        }
        if let Some(data) = result.get("structuredContent").and_then(|v| v.get("data")) {
            return Ok(data.clone());
        }
        // Some MCP-compatible deployments omit structuredContent and return
        // the documented JSON envelope in one text content item. The official
        // server currently provides both; never interpret arbitrary prose.
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            let mut decoded = None;
            for item in content {
                if item.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let text = item.get("text").and_then(Value::as_str).ok_or_else(|| {
                    AaveMcpError::Protocol("text content item is malformed".into())
                })?;
                let value: Value = serde_json::from_str(text).map_err(|_| {
                    AaveMcpError::Protocol(
                        "text content is not the documented JSON envelope".into(),
                    )
                })?;
                let data = value.get("data").ok_or_else(|| {
                    AaveMcpError::Protocol("text content has no data field".into())
                })?;
                if decoded.replace(data.clone()).is_some() {
                    return Err(AaveMcpError::Protocol(
                        "multiple text results are ambiguous".into(),
                    ));
                }
            }
            if let Some(data) = decoded {
                return Ok(data);
            }
        }
        Err(AaveMcpError::Protocol(
            "MCP result has no structured transaction data".into(),
        ))
    }

    async fn call_tool_result(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, AaveMcpError> {
        let id = self.id();
        let response = self.request(json!({
            "jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":name,"arguments":arguments}
        }), true).await?;
        let message = parse_message(&response)?;
        if message.get("error").is_some() {
            return Err(rpc_error(&message));
        }
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(AaveMcpError::Protocol(
                "JSON-RPC response id does not match request".into(),
            ));
        }
        let result = message
            .get("result")
            .ok_or_else(|| AaveMcpError::Protocol("tools/call result is missing".into()))?;
        Ok(result.clone())
    }

    async fn list_read_tools(&mut self) -> Result<AaveMcpReadCatalog, AaveMcpError> {
        let mut names = BTreeSet::new();
        let mut exposed = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = BTreeSet::new();
        let mut aggregate_bytes = 0usize;
        let mut complete = false;
        for _ in 0..MAX_TOOL_PAGES {
            let id = self.id();
            let mut params = serde_json::Map::new();
            if let Some(cursor) = &cursor {
                params.insert("cursor".into(), Value::String(cursor.clone()));
            }
            let response = self
                .request(
                    json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":params}),
                    true,
                )
                .await?;
            let message = parse_message(&response)?;
            if message.get("error").is_some() {
                return Err(rpc_error(&message));
            }
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                return Err(AaveMcpError::Protocol(
                    "tools/list response id does not match request".into(),
                ));
            }
            let result = message
                .get("result")
                .ok_or_else(|| AaveMcpError::Protocol("tools/list result is missing".into()))?;
            let catalog_bytes = serde_json::to_vec(result)
                .map_err(|_| AaveMcpError::Protocol("tools/list result is invalid JSON".into()))?;
            aggregate_bytes = aggregate_bytes.saturating_add(catalog_bytes.len());
            if aggregate_bytes > MAX_TOOL_CATALOG_BYTES {
                return Err(AaveMcpError::Protocol(
                    "tools/list catalog exceeds the local size bound".into(),
                ));
            }
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| AaveMcpError::Protocol("tools/list has no tools array".into()))?;
            for tool in tools {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    return Err(AaveMcpError::Protocol(
                        "tools/list contains a tool without a name".into(),
                    ));
                };
                if name.len() > 128 || !names.insert(name.to_owned()) {
                    return Err(AaveMcpError::Protocol(
                        "tools/list contains an invalid or duplicate tool name".into(),
                    ));
                }
                if names.len() > MAX_TOOL_COUNT {
                    return Err(AaveMcpError::Protocol(
                        "tools/list exceeds the local tool-count bound".into(),
                    ));
                }
                if tool
                    .get("annotations")
                    .and_then(|annotations| annotations.get("readOnlyHint"))
                    .and_then(Value::as_bool)
                    != Some(true)
                    || !allowed_read_tool_name(name)
                {
                    continue;
                }
                let Some(schema) = tool.get("inputSchema") else {
                    continue;
                };
                if validate_schema_definition(schema).is_err() {
                    continue;
                }
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|description| description.len() <= 4096)
                    .map(str::to_owned);
                exposed.push(AaveMcpReadTool {
                    name: name.to_owned(),
                    description,
                    input_schema: schema.clone(),
                });
            }
            match result.get("nextCursor") {
                None | Some(Value::Null) => {
                    complete = true;
                    break;
                }
                Some(Value::String(next)) if !next.is_empty() && next.len() <= 2048 => {
                    if !seen_cursors.insert(next.clone()) {
                        return Err(AaveMcpError::Protocol(
                            "tools/list returned a repeated pagination cursor".into(),
                        ));
                    }
                    cursor = Some(next.clone());
                }
                _ => {
                    return Err(AaveMcpError::Protocol(
                        "tools/list returned an invalid pagination cursor".into(),
                    ));
                }
            }
        }
        if !complete {
            return Err(AaveMcpError::Protocol(
                "tools/list pagination exceeded the local page bound".into(),
            ));
        }
        exposed.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(AaveMcpReadCatalog {
            tools: exposed,
            provenance: AaveMcpReadProvenance::UnverifiedOfficialMcp,
        })
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    async fn request(
        &self,
        body: Value,
        initialized: bool,
    ) -> Result<McpHttpResponse, AaveMcpError> {
        let mut headers = BTreeMap::from([
            ("content-type".into(), "application/json".into()),
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
        ]);
        if initialized {
            headers.insert("mcp-protocol-version".into(), PROTOCOL_VERSION.into());
            if let Some(session) = &self.session_id {
                headers.insert("mcp-session-id".into(), session.clone());
            }
        }
        let response = self
            .transport
            .post(McpHttpRequest {
                headers,
                body: serde_json::to_vec(&body)
                    .map_err(|e| AaveMcpError::Protocol(e.to_string()))?,
            })
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(AaveMcpError::Transport(format!(
                "HTTP status {}",
                response.status
            )));
        }
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(AaveMcpError::Protocol(
                "response exceeded the one MiB bound".into(),
            ));
        }
        Ok(response)
    }
}

/// Name-family half of the read-only policy, exported so broker routing and
/// capability discovery can enforce exactly the same local gate. Callers must
/// still require the live `readOnlyHint: true`, validated schema and bounds.
pub fn allowed_read_tool_name(name: &str) -> bool {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        || !(name.starts_with("get_")
            || name.starts_with("list_")
            || name.starts_with("search_")
            || name.starts_with("preview_"))
    {
        return false;
    }
    // Exact underscore-delimited families prevent a newly published tool from
    // being exposed merely because its name starts with `get_` or `preview_`.
    // `get_user_rewards` may include an unsigned claim transaction, but it is
    // surfaced only as inert read data and is never decoded or signed here.
    let forbidden_prefixes = [
        "prepare_", "submit_", "cancel_", "approve_", "sign_", "execute_", "deploy_",
    ];
    let denied = [
        "prepare",
        "submit",
        "cancel",
        "approve",
        "approval",
        "permit",
        "sign",
        "signed",
        "signature",
        "write",
        "execute",
        "execution",
        "deploy",
        "secret",
        "credential",
    ];
    !forbidden_prefixes
        .iter()
        .any(|prefix| name.starts_with(prefix))
        && !name.split('_').any(|part| denied.contains(&part))
}

fn validate_schema_definition(schema: &Value) -> Result<(), AaveMcpError> {
    let bytes = serde_json::to_vec(schema)
        .map_err(|_| AaveMcpError::Protocol("tool schema is invalid JSON".into()))?;
    if bytes.len() > MAX_TOOL_CATALOG_BYTES || json_depth(schema) > MAX_SCHEMA_DEPTH {
        return Err(AaveMcpError::Protocol(
            "tool schema exceeds the local size or nesting bound".into(),
        ));
    }
    if !schema.is_object() || schema["type"].as_str() != Some("object") {
        return Err(AaveMcpError::Protocol(
            "tool schema must have an object root".into(),
        ));
    }
    reject_schema_refs_and_sensitive_keys(schema)?;
    jsonschema::validator_for(schema)
        .map(|_| ())
        .map_err(|_| AaveMcpError::Protocol("tool schema could not be compiled locally".into()))
}

fn reject_schema_refs_and_sensitive_keys(schema: &Value) -> Result<(), AaveMcpError> {
    fn visit(value: &Value, depth: usize) -> Result<(), AaveMcpError> {
        if depth > MAX_SCHEMA_DEPTH {
            return Err(AaveMcpError::Protocol(
                "tool schema exceeds the local nesting bound".into(),
            ));
        }
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                        return Err(AaveMcpError::Protocol(
                            "tool schema references are disabled".into(),
                        ));
                    }
                    if key == "properties" {
                        let properties = child.as_object().ok_or_else(|| {
                            AaveMcpError::Protocol("tool schema properties are invalid".into())
                        })?;
                        for name in properties.keys() {
                            let normalized = name.to_ascii_lowercase();
                            if [
                                "signature",
                                "permit",
                                "secret",
                                "privatekey",
                                "private_key",
                                "credential",
                                "apikey",
                                "api_key",
                            ]
                            .iter()
                            .any(|fragment| normalized.contains(fragment))
                            {
                                return Err(AaveMcpError::Protocol(
                                    "tool schema contains a signing or secret input".into(),
                                ));
                            }
                        }
                    }
                    visit(child, depth + 1)?;
                }
            }
            Value::Array(array) => {
                if array.len() > 256 {
                    return Err(AaveMcpError::Protocol(
                        "tool schema array exceeds the local bound".into(),
                    ));
                }
                for child in array {
                    visit(child, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(schema, 0)
}

fn validate_arguments(schema: &Value, arguments: &Value) -> Result<(), AaveMcpError> {
    let bytes = serde_json::to_vec(arguments)
        .map_err(|_| AaveMcpError::Protocol("read arguments are invalid JSON".into()))?;
    if bytes.len() > MAX_READ_ARGUMENT_BYTES || json_depth(arguments) > MAX_JSON_DEPTH {
        return Err(AaveMcpError::InvalidReadArguments(
            "read arguments exceed the local size or nesting bound".into(),
        ));
    }
    if json_contains_sensitive_key(arguments) {
        return Err(AaveMcpError::InvalidReadArguments(
            "read arguments may not contain signing or secret fields".into(),
        ));
    }
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| AaveMcpError::Protocol("tool schema could not be compiled locally".into()))?;
    if !validator.is_valid(arguments) {
        return Err(AaveMcpError::InvalidReadArguments(
            "read arguments do not match the live tool schema".into(),
        ));
    }
    Ok(())
}

fn json_contains_sensitive_key(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, child)| {
            let normalized = key.to_ascii_lowercase();
            [
                "signature",
                "permit",
                "secret",
                "privatekey",
                "private_key",
                "credential",
                "apikey",
                "api_key",
            ]
            .iter()
            .any(|fragment| normalized.contains(fragment))
                || json_contains_sensitive_key(child)
        }),
        Value::Array(array) => array.iter().any(json_contains_sensitive_key),
        _ => false,
    }
}

fn validate_read_call_tool_result(result: &Value) -> Result<(), AaveMcpError> {
    let invalid = || AaveMcpError::Protocol("tools/call returned a malformed read result".into());
    let object = result.as_object().ok_or_else(invalid)?;
    if object
        .get("isError")
        .is_some_and(|is_error| !is_error.is_boolean())
    {
        return Err(invalid());
    }
    if let Some(structured) = object.get("structuredContent")
        && !structured.is_object()
    {
        return Err(invalid());
    }
    if let Some(content) = object.get("content") {
        let content = content.as_array().ok_or_else(invalid)?;
        if content.len() > 4096 {
            return Err(invalid());
        }
        for item in content {
            let item = item.as_object().ok_or_else(invalid)?;
            match item.get("type").and_then(Value::as_str) {
                Some("text") if item.get("text").and_then(Value::as_str).is_some() => {}
                Some("image" | "audio")
                    if item.get("data").and_then(Value::as_str).is_some()
                        && item.get("mimeType").and_then(Value::as_str).is_some() => {}
                Some("resource") if item.get("resource").is_some_and(Value::is_object) => {}
                Some("resource_link")
                    if item.get("uri").and_then(Value::as_str).is_some()
                        && item.get("name").and_then(Value::as_str).is_some() => {}
                _ => return Err(invalid()),
            }
        }
    }
    Ok(())
}

fn enforce_pagination_bounds(arguments: &Value) -> Result<(), AaveMcpError> {
    fn walk(value: &Value, depth: usize) -> Result<(), AaveMcpError> {
        if depth > MAX_JSON_DEPTH {
            return Err(AaveMcpError::InvalidReadArguments(
                "read arguments exceed the local nesting bound".into(),
            ));
        }
        match value {
            Value::Object(values) => {
                for (key, value) in values {
                    if matches!(key.as_str(), "limit" | "pageSize" | "page_size")
                        && value.as_u64().is_none_or(|limit| limit > MAX_PAGE_SIZE)
                    {
                        return Err(AaveMcpError::InvalidReadArguments(
                            "pagination limit exceeds the local cap of 100".into(),
                        ));
                    }
                    if matches!(key.as_str(), "offset" | "page" | "pageNumber")
                        && value.as_u64().is_none_or(|offset| offset > 1_000_000)
                    {
                        return Err(AaveMcpError::InvalidReadArguments(
                            "pagination offset exceeds the local bound".into(),
                        ));
                    }
                    if matches!(key.as_str(), "cursor" | "nextCursor" | "next_cursor")
                        && value.as_str().is_none_or(|cursor| cursor.len() > 2048)
                    {
                        return Err(AaveMcpError::InvalidReadArguments(
                            "pagination cursor exceeds the local size bound".into(),
                        ));
                    }
                    walk(value, depth + 1)?;
                }
            }
            Value::Array(values) => {
                if values.len() > 100 {
                    return Err(AaveMcpError::InvalidReadArguments(
                        "array read argument exceeds the local cap of 100".into(),
                    ));
                }
                for value in values {
                    walk(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(arguments, 0)
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn header(headers: &BTreeMap<String, String>, name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn parse_message(response: &McpHttpResponse) -> Result<Value, AaveMcpError> {
    let content_type = header(&response.headers, "content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let text = std::str::from_utf8(&response.body)
        .map_err(|_| AaveMcpError::Protocol("response is not UTF-8".into()))?;
    let payload = if content_type.starts_with("text/event-stream") {
        let events = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if events.len() != 1 {
            return Err(AaveMcpError::Protocol(
                "SSE response must contain exactly one data event".into(),
            ));
        }
        events[0]
    } else if content_type.starts_with("application/json") {
        text
    } else {
        return Err(AaveMcpError::Protocol(
            "unsupported MCP response content type".into(),
        ));
    };
    let message: Value = serde_json::from_str(payload)
        .map_err(|_| AaveMcpError::Protocol("response body is not a JSON-RPC object".into()))?;
    if !message.is_object() || message["jsonrpc"].as_str() != Some("2.0") {
        return Err(AaveMcpError::Protocol(
            "response is not a JSON-RPC 2.0 object".into(),
        ));
    }
    if message.get("result").is_some() == message.get("error").is_some() {
        return Err(AaveMcpError::Protocol(
            "JSON-RPC response must contain exactly one of result or error".into(),
        ));
    }
    Ok(message)
}

fn rpc_error(message: &Value) -> AaveMcpError {
    let code = message["error"]["code"]
        .as_i64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".into());
    AaveMcpError::Protocol(format!("JSON-RPC error {code}"))
}

struct HttpAaveMcpTransport {
    client: reqwest::Client,
    endpoint: reqwest::Url,
}

impl HttpAaveMcpTransport {
    fn new() -> Self {
        Self::build(ENDPOINT).expect("fixed official Aave MCP endpoint is valid")
    }

    fn build(endpoint: &str) -> Result<Self, AaveMcpError> {
        let endpoint =
            reqwest::Url::parse(endpoint).map_err(|e| AaveMcpError::Transport(e.to_string()))?;
        if endpoint.scheme() != "https" || endpoint.host_str() != Some("mcp.aave.com") {
            return Err(AaveMcpError::Transport(
                "only https://mcp.aave.com is allowed".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AaveMcpError::Transport(e.to_string()))?;
        Ok(Self { client, endpoint })
    }

    #[cfg(test)]
    fn for_local_fixture(endpoint: &str) -> Self {
        let endpoint = reqwest::Url::parse(endpoint).expect("test fixture URL");
        assert!(matches!(
            endpoint.host_str(),
            Some("127.0.0.1" | "localhost")
        ));
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self { client, endpoint }
    }
}

#[async_trait]
impl AaveMcpTransport for HttpAaveMcpTransport {
    async fn post(&self, request: McpHttpRequest) -> Result<McpHttpResponse, AaveMcpError> {
        let mut builder = self.client.post(self.endpoint.clone()).body(request.body);
        for (key, value) in request.headers {
            builder = builder.header(&key, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| AaveMcpError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(key, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (key.as_str().to_ascii_lowercase(), value.to_owned()))
            })
            .collect();
        let mut stream = response;
        let mut body = Vec::new();
        while let Some(chunk) = stream
            .chunk()
            .await
            .map_err(|e| AaveMcpError::Transport(e.to_string()))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(AaveMcpError::Protocol(
                    "response exceeded the one MiB bound".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(McpHttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_evm::alloy::sol_types::SolCall;
    use std::{collections::VecDeque, sync::Mutex};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn address(value: &str) -> Address {
        value.parse().unwrap()
    }
    fn owner() -> Address {
        address("0x1111111111111111111111111111111111111111")
    }
    fn token() -> Address {
        address("0x2222222222222222222222222222222222222222")
    }
    fn pool() -> Address {
        address("0x3333333333333333333333333333333333333333")
    }

    fn intent(action: AaveMcpAction) -> AaveMcpIntent {
        AaveMcpIntent {
            chain_id: 1,
            pool: pool(),
            owner: owner(),
            token: token(),
            action,
            amount: (action != AaveMcpAction::Collateral)
                .then_some(AaveMcpAmount::Exact(U256::from(123_456u64))),
            recipient: (action == AaveMcpAction::Withdraw).then_some(owner()),
            enabled: (action == AaveMcpAction::Collateral).then_some(true),
            decimals: 6,
        }
    }

    fn valid_calldata(intent: &AaveMcpIntent) -> Bytes {
        match intent.action {
            AaveMcpAction::Supply => Pool::supplyCall {
                asset: token(),
                amount: U256::from(123_456),
                onBehalfOf: owner(),
                referralCode: 0,
            }
            .abi_encode()
            .into(),
            AaveMcpAction::Borrow => Pool::borrowCall {
                asset: token(),
                amount: U256::from(123_456),
                interestRateMode: U256::from(2),
                referralCode: 0,
                onBehalfOf: owner(),
            }
            .abi_encode()
            .into(),
            AaveMcpAction::Repay => Pool::repayCall {
                asset: token(),
                amount: U256::from(123_456),
                interestRateMode: U256::from(2),
                onBehalfOf: owner(),
            }
            .abi_encode()
            .into(),
            AaveMcpAction::Withdraw => Pool::withdrawCall {
                asset: token(),
                amount: U256::from(123_456),
                to: owner(),
            }
            .abi_encode()
            .into(),
            AaveMcpAction::Collateral => Pool::setUserUseReserveAsCollateralCall {
                asset: token(),
                useAsCollateral: true,
            }
            .abi_encode()
            .into(),
        }
    }

    fn transaction(intent: &AaveMcpIntent, data: &Bytes) -> Value {
        json!({
            "__typename":"TransactionRequest", "to":format!("{:#x}", intent.pool),
            "from":format!("{:#x}", intent.owner), "data":format!("0x{}", hex::encode(data)),
            "value":"0", "chainId":intent.chain_id
        })
    }

    fn response(id: u64, result: Value) -> McpHttpResponse {
        McpHttpResponse {
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result})).unwrap(),
        }
    }

    fn init_response() -> McpHttpResponse {
        let mut response = response(
            1,
            json!({"protocolVersion":PROTOCOL_VERSION,"serverInfo":{"name":"aave-mcp","version":"1.0.0"},"capabilities":{}}),
        );
        response
            .headers
            .insert("mcp-session-id".into(), "test-session".into());
        response
    }

    fn test_read_tool(name: &str, schema: Value, annotation: bool) -> Value {
        json!({
            "name":name,
            "description":"Untrusted test description",
            "inputSchema":schema,
            "annotations":{"readOnlyHint":annotation,"openWorldHint":false,"destructiveHint":false}
        })
    }

    fn closed_object_schema(required: &[&str], properties: Value) -> Value {
        json!({
            "type":"object",
            "properties":properties,
            "required":required,
            "additionalProperties":false
        })
    }

    fn read_tools_response(id: u64, tools: Vec<Value>) -> McpHttpResponse {
        response(id, json!({"tools":tools}))
    }

    fn read_client_with_result(
        name: &str,
        schema: Value,
        annotation: bool,
        result: Value,
    ) -> (AaveMcpClient, Arc<FakeTransport>) {
        let tools = vec![test_read_tool(name, schema, annotation)];
        let transport = Arc::new(FakeTransport::with_responses(vec![
            init_response(),
            initialized_response(),
            read_tools_response(2, tools),
            response(3, result),
        ]));
        (AaveMcpClient::with_transport(transport.clone()), transport)
    }

    fn initialized_response() -> McpHttpResponse {
        McpHttpResponse {
            status: 202,
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }

    struct FakeTransport {
        responses: Mutex<VecDeque<McpHttpResponse>>,
        requests: Mutex<Vec<McpHttpRequest>>,
    }

    impl FakeTransport {
        fn with_responses(responses: Vec<McpHttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AaveMcpTransport for FakeTransport {
        async fn post(&self, request: McpHttpRequest) -> Result<McpHttpResponse, AaveMcpError> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| AaveMcpError::Transport("unexpected request".into()))
        }
    }

    fn client_for(_intent: &AaveMcpIntent, data: Value) -> (AaveMcpClient, Arc<FakeTransport>) {
        let call = response(
            2,
            json!({"structuredContent":{"data":data},"content":[{"type":"text","text":json!({"data":data}).to_string()}]}),
        );
        let transport = Arc::new(FakeTransport::with_responses(vec![
            init_response(),
            initialized_response(),
            call,
        ]));
        (AaveMcpClient::with_transport(transport.clone()), transport)
    }

    #[test]
    fn strict_embedded_pool_abi_accepts_all_supported_calls() {
        let actions = [
            AaveMcpAction::Supply,
            AaveMcpAction::Borrow,
            AaveMcpAction::Repay,
            AaveMcpAction::Withdraw,
            AaveMcpAction::Collateral,
        ];
        for action in actions {
            let intent = intent(action);
            validate_calldata(&intent, &valid_calldata(&intent)).unwrap();
        }
        for expected in actions {
            for actual in actions {
                if expected == actual {
                    continue;
                }
                let expected_intent = intent(expected);
                assert!(
                    validate_calldata(&expected_intent, &valid_calldata(&intent(actual))).is_err(),
                    "accepted {actual:?} selector for {expected:?} intent"
                );
            }
        }
    }

    #[test]
    fn strict_abi_rejects_unknown_selectors_trailing_bytes_and_wrong_arguments() {
        let intent = intent(AaveMcpAction::Supply);
        let mut unknown = valid_calldata(&intent).to_vec();
        unknown[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert!(validate_calldata(&intent, &unknown.into()).is_err());
        let mut trailing = valid_calldata(&intent).to_vec();
        trailing.push(0);
        assert!(validate_calldata(&intent, &trailing.into()).is_err());
        let wrong_asset: Bytes = Pool::supplyCall {
            asset: owner(),
            amount: U256::from(123_456),
            onBehalfOf: owner(),
            referralCode: 0,
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&intent, &wrong_asset).is_err());
        let wrong_amount: Bytes = Pool::supplyCall {
            asset: token(),
            amount: U256::from(123_457),
            onBehalfOf: owner(),
            referralCode: 0,
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&intent, &wrong_amount).is_err());
        let wrong_owner: Bytes = Pool::supplyCall {
            asset: token(),
            amount: U256::from(123_456),
            onBehalfOf: pool(),
            referralCode: 0,
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&intent, &wrong_owner).is_err());
        let nonzero_referral: Bytes = Pool::supplyCall {
            asset: token(),
            amount: U256::from(123_456),
            onBehalfOf: owner(),
            referralCode: 7,
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&intent, &nonzero_referral).is_err());
    }

    #[test]
    fn borrow_rejects_stable_rate_mode_and_other_owner() {
        let intent = intent(AaveMcpAction::Borrow);
        let stable: Bytes = Pool::borrowCall {
            asset: token(),
            amount: U256::from(123_456),
            interestRateMode: U256::from(1),
            referralCode: 0,
            onBehalfOf: owner(),
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&intent, &stable).is_err());
        let other: Bytes = Pool::borrowCall {
            asset: token(),
            amount: U256::from(123_456),
            interestRateMode: U256::from(2),
            referralCode: 0,
            onBehalfOf: pool(),
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&intent, &other).is_err());
    }

    #[test]
    fn all_semantics_require_finite_positive_cap_and_max_sentinel() {
        let mut repay = intent(AaveMcpAction::Repay);
        repay.amount = Some(AaveMcpAmount::Max {
            cap: U256::from(900),
        });
        let valid: Bytes = Pool::repayCall {
            asset: token(),
            amount: U256::MAX,
            interestRateMode: U256::from(2),
            onBehalfOf: owner(),
        }
        .abi_encode()
        .into();
        validate_calldata(&repay, &valid).unwrap();
        let bypass_cap: Bytes = Pool::repayCall {
            asset: token(),
            amount: U256::from(900),
            interestRateMode: U256::from(2),
            onBehalfOf: owner(),
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&repay, &bypass_cap).is_err());
        repay.amount = Some(AaveMcpAmount::Max { cap: U256::ZERO });
        assert!(validate_calldata(&repay, &valid).is_err());
        repay.amount = Some(AaveMcpAmount::Max { cap: U256::MAX });
        assert!(validate_calldata(&repay, &valid).is_err());
        repay.amount = Some(AaveMcpAmount::Exact(U256::MAX));
        assert!(validate_calldata(&repay, &valid).is_err());
        let mut borrow = intent(AaveMcpAction::Borrow);
        borrow.amount = Some(AaveMcpAmount::Max {
            cap: U256::from(10),
        });
        assert!(
            validate_calldata(&borrow, &valid_calldata(&intent(AaveMcpAction::Borrow))).is_err()
        );
        let mut supply = intent(AaveMcpAction::Supply);
        supply.amount = Some(AaveMcpAmount::Max {
            cap: U256::from(10),
        });
        assert!(
            validate_calldata(&supply, &valid_calldata(&intent(AaveMcpAction::Supply))).is_err()
        );
    }

    #[test]
    fn withdraw_and_collateral_validate_recipient_and_state() {
        let withdraw = intent(AaveMcpAction::Withdraw);
        let wrong_recipient: Bytes = Pool::withdrawCall {
            asset: token(),
            amount: U256::from(123_456),
            to: pool(),
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&withdraw, &wrong_recipient).is_err());
        let collateral = intent(AaveMcpAction::Collateral);
        let disabled: Bytes = Pool::setUserUseReserveAsCollateralCall {
            asset: token(),
            useAsCollateral: false,
        }
        .abi_encode()
        .into();
        assert!(validate_calldata(&collateral, &disabled).is_err());
    }

    #[test]
    fn transaction_envelope_rejects_wrong_target_chain_sender_or_value() {
        for action in [
            AaveMcpAction::Supply,
            AaveMcpAction::Borrow,
            AaveMcpAction::Repay,
            AaveMcpAction::Withdraw,
            AaveMcpAction::Collateral,
        ] {
            let intent = intent(action);
            let data = valid_calldata(&intent);
            validate_transaction_fields(&intent, 1, pool(), owner(), U256::ZERO, &data).unwrap();
            assert!(
                validate_transaction_fields(&intent, 42161, pool(), owner(), U256::ZERO, &data)
                    .is_err(),
                "accepted wrong chain for {action:?}"
            );
            assert!(
                validate_transaction_fields(&intent, 1, token(), owner(), U256::ZERO, &data)
                    .is_err(),
                "accepted wrong target for {action:?}"
            );
            assert!(
                validate_transaction_fields(&intent, 1, pool(), token(), U256::ZERO, &data)
                    .is_err(),
                "accepted wrong sender for {action:?}"
            );
            assert!(
                validate_transaction_fields(&intent, 1, pool(), owner(), U256::from(1), &data)
                    .is_err(),
                "accepted nonzero value for {action:?}"
            );
        }
    }

    #[test]
    fn documented_action_calldata_does_not_depend_on_undocumented_operation_metadata() {
        let intent = intent(AaveMcpAction::Collateral);
        let calldata = valid_calldata(&intent);
        let mut tx = transaction(&intent, &calldata);
        tx["operation"] = json!("SET_USER_USE_RESERVE_AS_COLLATERAL");
        validate_mcp_transaction(&intent, &tx, &calldata).unwrap();
        tx.as_object_mut().unwrap().remove("operation");
        validate_mcp_transaction(&intent, &tx, &calldata).unwrap();
    }

    #[tokio::test]
    async fn client_initializes_calls_official_tool_and_returns_only_validated_data() {
        let intent = intent(AaveMcpAction::Supply);
        let (client, transport) =
            client_for(&intent, transaction(&intent, &valid_calldata(&intent)));
        let call = client.prepare(&intent).await.unwrap();
        assert_eq!(call.calldata, valid_calldata(&intent));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let initialize: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(initialize["params"]["clientInfo"]["name"], "gofin-cli");
        let notification: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(notification["method"], "notifications/initialized");
        assert!(notification.get("id").is_none());
        assert_eq!(requests[1].headers["mcp-session-id"], "test-session");
        let tool: Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(tool["method"], "tools/call");
        assert_eq!(tool["params"]["name"], "prepare_action");
        assert_eq!(tool["params"]["arguments"]["version"], "v3");
        assert_eq!(tool["params"]["arguments"]["amount"], "0.123456");
        assert_eq!(tool["params"]["arguments"]["enableCollateral"], false);
        assert_eq!(
            requests[2].headers["mcp-protocol-version"],
            PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn client_rejects_approval_batches_even_when_original_action_looks_valid() {
        let intent = intent(AaveMcpAction::Supply);
        let approval = json!({
            "__typename":"ApprovalRequired",
            "approval":{"__typename":"TransactionRequest","to":format!("{:#x}",token()),"from":format!("{:#x}",owner()),"data":"0x095ea7b3","value":"0","chainId":1},
            "originalTransaction":transaction(&intent,&valid_calldata(&intent))
        });
        let (client, _) = client_for(&intent, approval);
        assert!(matches!(
            client.prepare(&intent).await,
            Err(AaveMcpError::UnsupportedPlan(_))
        ));
    }

    #[tokio::test]
    async fn client_rejects_tampered_envelopes_and_calldata() {
        let intent = intent(AaveMcpAction::Supply);
        for (field, value) in [
            ("chainId", json!(10)),
            ("to", json!(format!("{:#x}", token()))),
            ("from", json!(format!("{:#x}", token()))),
            ("value", json!("1")),
            ("data", json!("0x095ea7b3")),
        ] {
            let mut tx = transaction(&intent, &valid_calldata(&intent));
            tx[field] = value;
            let (client, _) = client_for(&intent, tx);
            assert!(
                client.prepare(&intent).await.is_err(),
                "accepted tampered {field}"
            );
        }
    }

    #[tokio::test]
    async fn bounded_all_arguments_omit_decimal_amount_and_keep_cap_local() {
        let mut intent = intent(AaveMcpAction::Repay);
        intent.amount = Some(AaveMcpAmount::Max {
            cap: U256::from(987_654),
        });
        let calldata: Bytes = Pool::repayCall {
            asset: token(),
            amount: U256::MAX,
            interestRateMode: U256::from(2),
            onBehalfOf: owner(),
        }
        .abi_encode()
        .into();
        let (client, transport) = client_for(&intent, transaction(&intent, &calldata));
        client.prepare(&intent).await.unwrap();
        let request: Value =
            serde_json::from_slice(&transport.requests.lock().unwrap()[2].body).unwrap();
        let args = &request["params"]["arguments"];
        assert_eq!(args["max"], true);
        assert!(args.get("amount").is_none());
        assert_eq!(
            intent.amount,
            Some(AaveMcpAmount::Max {
                cap: U256::from(987_654)
            })
        );
    }

    #[tokio::test]
    async fn read_catalog_requires_read_annotation_safe_family_and_closed_local_schema() {
        let valid_schema = closed_object_schema(&["user"], json!({"user":{"type":"string"}}));
        let tools = vec![
            test_read_tool("get_user_positions", valid_schema.clone(), true),
            test_read_tool("get_user_rewards", valid_schema.clone(), true),
            test_read_tool("list_reserve_holders", valid_schema.clone(), true),
            test_read_tool("get_swap_quote", valid_schema.clone(), true),
            test_read_tool("get_user_vote", valid_schema.clone(), true),
            test_read_tool("prepare_liquidation", valid_schema.clone(), true),
            test_read_tool("get_user_vote_unannotated", valid_schema.clone(), false),
            test_read_tool("get_permit_status", valid_schema.clone(), true),
            test_read_tool(
                "get_credentials",
                closed_object_schema(&["api_key"], json!({"api_key":{"type":"string"}})),
                true,
            ),
            test_read_tool(
                "get_untrusted_reference",
                json!({"type":"object","$ref":"https://attacker.invalid/schema"}),
                true,
            ),
            test_read_tool("claim_rewards", valid_schema, true),
        ];
        let transport = Arc::new(FakeTransport::with_responses(vec![
            init_response(),
            initialized_response(),
            read_tools_response(2, tools),
        ]));
        let client = AaveMcpClient::with_transport(transport);
        let catalog = client.discover_read_tools().await.unwrap();
        assert_eq!(
            catalog.provenance,
            AaveMcpReadProvenance::UnverifiedOfficialMcp
        );
        assert_eq!(
            catalog
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            [
                "get_swap_quote",
                "get_user_positions",
                "get_user_rewards",
                "get_user_vote",
                "list_reserve_holders"
            ]
        );
        assert_eq!(
            catalog.tools[0].description.as_deref(),
            Some("Untrusted test description")
        );
    }

    #[tokio::test]
    async fn read_call_preserves_rewards_transaction_payload_as_unverified_inert_data() {
        let schema = closed_object_schema(&["user"], json!({"user":{"type":"string"}}));
        let raw_result = json!({
            "structuredContent":{"data":{"rewards":[{"claimTransaction":{"to":"0x1111","data":"0xdeadbeef"}}],"coverage":{"complete":false,"nextCursor":"cursor-2"}}},
            "content":[{"type":"text","text":"claim payload is inert"}],
            "isError":false,
            "_meta":{"warnings":["not verified"]}
        });
        let (client, transport) =
            read_client_with_result("get_user_rewards", schema, true, raw_result.clone());
        let returned = client
            .call_read_tool(
                "get_user_rewards",
                json!({"user":"0x1111111111111111111111111111111111111111"}),
            )
            .await
            .unwrap();
        assert_eq!(returned.result, raw_result);
        assert_eq!(
            returned.provenance,
            AaveMcpReadProvenance::UnverifiedOfficialMcp
        );
        assert_eq!(
            returned.result["structuredContent"]["data"]["rewards"][0]["claimTransaction"]["data"],
            "0xdeadbeef"
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let list: Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(list["method"], "tools/list");
        let call: Value = serde_json::from_slice(&requests[3].body).unwrap();
        assert_eq!(call["method"], "tools/call");
        assert_eq!(call["params"]["name"], "get_user_rewards");
    }

    #[tokio::test]
    async fn read_call_rejects_malformed_call_tool_result_envelopes() {
        let schema = closed_object_schema(&[], json!({}));
        for malformed in [
            json!([]),
            json!("result"),
            json!({"isError":"false","content":[]}),
            json!({"isError":false,"content":{}}),
            json!({"isError":false,"content":["text"]}),
            json!({"isError":false,"content":[{"type":5,"text":"ok"}]}),
            json!({"isError":false,"content":[{"type":"text","text":5}]}),
            json!({"isError":false,"structuredContent":[]}),
        ] {
            let (client, _) =
                read_client_with_result("get_chains", schema.clone(), true, malformed);
            assert!(
                matches!(
                    client.call_read_tool("get_chains", json!({})).await,
                    Err(AaveMcpError::Protocol(_))
                ),
                "accepted malformed MCP CallToolResult"
            );
        }
    }

    #[tokio::test]
    async fn read_calls_reject_unannotated_mutating_unknown_and_invalid_arguments_before_call() {
        let schema = closed_object_schema(&["user"], json!({"user":{"type":"string"}}));
        for (name, annotation, arguments) in [
            (
                "get_user_positions",
                false,
                json!({"user":"0x1111111111111111111111111111111111111111"}),
            ),
            (
                "prepare_liquidation",
                true,
                json!({"user":"0x1111111111111111111111111111111111111111"}),
            ),
            ("get_user_positions", true, json!({"user":7})),
            (
                "get_user_positions",
                true,
                json!({"user":"0x1111111111111111111111111111111111111111","permitSignature":"0x01"}),
            ),
        ] {
            let (client, transport) = read_client_with_result(
                name,
                schema.clone(),
                annotation,
                json!({"structuredContent":{"data":{}}}),
            );
            assert!(
                client.call_read_tool(name, arguments).await.is_err(),
                "accepted {name}"
            );
            let requests = transport.requests.lock().unwrap();
            let expected_before_rejection = if name.starts_with("prepare_") { 0 } else { 3 };
            assert_eq!(
                requests.len(),
                expected_before_rejection,
                "unexpected MCP calls before rejecting {name}"
            );
        }
        let (client, _) = read_client_with_result(
            "get_user_positions",
            schema,
            true,
            json!({"structuredContent":{"data":{}}}),
        );
        assert!(matches!(
            client
                .call_read_tool("get_user_positions", json!({"user":7}))
                .await,
            Err(AaveMcpError::InvalidReadArguments(_))
        ));
    }

    #[tokio::test]
    async fn read_catalog_paginates_boundedly_and_rejects_cursor_loops() {
        let schema = closed_object_schema(&[], json!({}));
        let transport = Arc::new(FakeTransport::with_responses(vec![
            init_response(),
            initialized_response(),
            response(
                2,
                json!({
                    "tools":[test_read_tool("get_chains", schema.clone(), true)],
                    "nextCursor":"page-2"
                }),
            ),
            response(
                3,
                json!({
                    "tools":[test_read_tool("list_markets", schema.clone(), true)]
                }),
            ),
        ]));
        let client = AaveMcpClient::with_transport(transport.clone());
        let catalog = client.discover_read_tools().await.unwrap();
        assert_eq!(
            catalog
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["get_chains", "list_markets"]
        );
        let second_page: Value = {
            let requests = transport.requests.lock().unwrap();
            serde_json::from_slice(&requests[3].body).unwrap()
        };
        assert_eq!(second_page["params"]["cursor"], "page-2");

        let loop_transport = Arc::new(FakeTransport::with_responses(vec![
            init_response(),
            initialized_response(),
            response(2, json!({"tools":[],"nextCursor":"loop"})),
            response(3, json!({"tools":[],"nextCursor":"loop"})),
        ]));
        let loop_client = AaveMcpClient::with_transport(loop_transport);
        assert!(loop_client.discover_read_tools().await.is_err());
    }

    #[tokio::test]
    async fn read_catalog_cache_is_ttl_bounded_and_expiry_fetch_failure_fails_closed() {
        let schema = closed_object_schema(&[], json!({}));
        let transport = Arc::new(FakeTransport::with_responses(vec![
            init_response(),
            initialized_response(),
            read_tools_response(2, vec![test_read_tool("get_chains", schema, true)]),
        ]));
        let client = AaveMcpClient::with_transport(transport.clone());
        let first = client.discover_read_tools().await.unwrap();
        assert_eq!(client.discover_read_tools().await.unwrap(), first);
        assert_eq!(transport.requests.lock().unwrap().len(), 3);

        *client.read_catalog_cache.lock().unwrap() = Some((
            Instant::now() - READ_CATALOG_TTL - Duration::from_secs(1),
            first,
        ));
        assert!(client.discover_read_tools().await.is_err());
        assert!(client.read_catalog_cache.lock().unwrap().is_none());
        assert_eq!(transport.requests.lock().unwrap().len(), 4);
    }

    #[test]
    fn pagination_limits_are_bounded_without_following_cursors() {
        assert!(enforce_pagination_bounds(&json!({"limit":101})).is_err());
        assert!(enforce_pagination_bounds(&json!({"limit":100})).is_ok());
        assert!(enforce_pagination_bounds(&json!({"cursor":"x".repeat(2049)})).is_err());
        assert!(enforce_pagination_bounds(&json!({"cursor":"next-page"})).is_ok());
    }

    #[test]
    fn parses_json_and_single_sse_event_and_rejects_ambiguous_or_unsupported_bodies() {
        let message = json!({"jsonrpc":"2.0","id":2,"result":{}}).to_string();
        let json_response = McpHttpResponse {
            status: 200,
            headers: BTreeMap::from([(
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            )]),
            body: message.as_bytes().to_vec(),
        };
        assert_eq!(parse_message(&json_response).unwrap()["id"], 2);
        let sse = McpHttpResponse {
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "text/event-stream".into())]),
            body: format!("event: message\ndata: {message}\n\n").into_bytes(),
        };
        assert_eq!(parse_message(&sse).unwrap()["id"], 2);
        let two = McpHttpResponse {
            body: format!("data: {message}\n\ndata: {message}\n\n").into_bytes(),
            ..sse.clone()
        };
        assert!(parse_message(&two).is_err());
        let html = McpHttpResponse {
            headers: BTreeMap::from([("content-type".into(), "text/html".into())]),
            ..json_response.clone()
        };
        assert!(parse_message(&html).is_err());
        let malformed = McpHttpResponse {
            body: b"{".to_vec(),
            ..json_response
        };
        assert!(parse_message(&malformed).is_err());
        let conflicting = McpHttpResponse {
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#.to_vec(),
        };
        assert!(parse_message(&conflicting).is_err());
        let no_protocol = McpHttpResponse {
            body: br#"{"id":1,"result":{}}"#.to_vec(),
            ..conflicting
        };
        assert!(parse_message(&no_protocol).is_err());
    }

    #[tokio::test]
    async fn concrete_http_transport_uses_loopback_only_test_fixture_and_mcp_envelopes() {
        let intent = intent(AaveMcpAction::Collateral);
        let expected = transaction(&intent, &valid_calldata(&intent));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    if request
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .is_some_and(|end| {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|n| n.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            request.len() >= end + 4 + length
                        })
                    {
                        break;
                    }
                }
                if index == 1 {
                    socket
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    continue;
                }
                let response_json = if index == 0 {
                    json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":PROTOCOL_VERSION,"serverInfo":{"name":"aave-mcp","version":"1.0.0"},"capabilities":{}}})
                } else {
                    json!({"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"data":expected},"content":[{"type":"text","text":"ignored because structuredContent is authoritative"}]}})
                };
                let body = serde_json::to_vec(&response_json).unwrap();
                let session = if index == 0 {
                    "Mcp-Session-Id: test-session\r\n"
                } else {
                    ""
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let transport = Arc::new(HttpAaveMcpTransport::for_local_fixture(&format!(
            "http://{addr}/"
        )));
        let client = AaveMcpClient::with_transport(transport);
        assert_eq!(
            client.prepare(&intent).await.unwrap().calldata,
            valid_calldata(&intent)
        );
        server.await.unwrap();
    }

    /// Opt-in smoke against the public, non-custodial builder. This only asks
    /// for a one-wei candidate using an unmistakably synthetic owner, signs
    /// nothing, and expects the server's approval-first envelope to be refused.
    #[tokio::test]
    #[ignore = "public Aave MCP network smoke; opt in with --ignored"]
    async fn live_public_builder_smoke_refuses_approval_plan_for_synthetic_owner() {
        let intent = AaveMcpIntent {
            chain_id: 1,
            pool: address("0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2"),
            owner: owner(),
            token: address("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            action: AaveMcpAction::Supply,
            amount: Some(AaveMcpAmount::Exact(U256::from(1))),
            recipient: None,
            enabled: None,
            decimals: 18,
        };
        let result = AaveMcpClient::new().prepare(&intent).await;
        assert!(
            matches!(
                &result,
                Err(AaveMcpError::UnsupportedPlan(message))
                    if message.starts_with("Aave returned an approval-first plan;")
            ),
            "expected the official approval-first plan rejection, got {result:?}"
        );
    }

    #[tokio::test]
    #[ignore = "public Aave MCP read catalog smoke; opt in with --ignored"]
    async fn live_read_catalog_smoke_exposes_position_discovery_and_read_quotes() {
        let client = AaveMcpClient::new();
        let catalog = client.discover_read_tools().await.unwrap();
        let names = catalog
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<BTreeSet<_>>();
        eprintln!(
            "Live Aave MCP read tools ({}): {}",
            names.len(),
            names.iter().copied().collect::<Vec<_>>().join(", ")
        );
        for required in [
            "get_user_positions",
            "get_position_items",
            "get_user_summary",
            "get_wallet_balances",
            "get_swap_quote",
        ] {
            assert!(
                names.contains(required),
                "live read catalog missing {required}"
            );
        }
        assert!(!names.iter().any(|name| {
            name.starts_with("prepare_")
                || name.starts_with("submit_")
                || name.starts_with("cancel_")
                || name.starts_with("approve_")
                || name.starts_with("sign_")
        }));
        let positions = client
            .call_read_tool(
                "get_user_positions",
                json!({"user":"0x1111111111111111111111111111111111111111"}),
            )
            .await
            .unwrap();
        assert_eq!(
            positions.provenance,
            AaveMcpReadProvenance::UnverifiedOfficialMcp
        );
        assert_eq!(positions.tool_name, "get_user_positions");
        assert_ne!(
            positions.result.get("isError"),
            Some(&Value::Bool(true)),
            "Aave MCP position read returned an upstream error"
        );
        let structured_payload_is_nonempty = positions
            .result
            .get("structuredContent")
            .and_then(Value::as_object)
            .is_some_and(|content| !content.is_empty());
        let text_payload_is_nonempty = positions
            .result
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty())
                })
            });
        assert!(
            structured_payload_is_nonempty || text_payload_is_nonempty,
            "Aave MCP position read returned no payload: {:?}",
            positions.result
        );
    }

    #[test]
    fn production_endpoint_cannot_be_changed_to_arbitrary_host() {
        assert!(HttpAaveMcpTransport::build("https://example.com/").is_err());
        assert!(HttpAaveMcpTransport::build("http://mcp.aave.com/").is_err());
        assert!(HttpAaveMcpTransport::build("https://mcp.aave.com.evil.example/").is_err());
    }
}
