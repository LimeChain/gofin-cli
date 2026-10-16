//! Reviewed EVM mechanics. Signing accepts only a frozen, broker-authorized envelope.
//! This crate never loads credentials, exposes signatures, or stores broadcast bytes.
use alloy::{
    consensus::{SignableTransaction, TxEip1559, TxEnvelope, transaction::SignerRecoverable},
    eips::{eip2718::Decodable2718, eip2930::AccessList},
    primitives::{Address, B256, Bytes, Signature, TxKind, U256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::SolCall,
};
use async_trait::async_trait;
use gofin_domain::{
    GoFinError, Quantity, Result, RevalidatedOperation, Submission, SubmissionJournal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

pub use alloy;
mod fees;
mod rpc_race;
mod rpc_selection;
pub use fees::{OperatorFeeModel, RollupFeeBound};
use rpc_selection::{ReadTrace, SelectionState};

fn error(code: &str, stage: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        stage,
        message,
        "Inspect the original operation and its configured network; never replay an uncertain submission.",
    )
}

fn preserve_rpc_cause(mut mapped: GoFinError, source: &GoFinError) -> GoFinError {
    if let Some(details) = source.details.as_deref() {
        mapped.details = Some(Box::new(details.clone()));
    }
    mapped
}

fn ceil_percent(value: U256, percent: u64) -> Option<U256> {
    value
        .checked_mul(U256::from(percent))
        .and_then(|scaled| scaled.checked_add(U256::from(99)))
        .map(|scaled| scaled / U256::from(100))
}

fn validate_frozen_fee_bid(transaction: &FrozenTransaction, base_fee: U256) -> Result<()> {
    let max_fee = u256(&transaction.max_fee_per_gas)?;
    let priority = u256(&transaction.max_priority_fee_per_gas)?;
    let fully_funded_bid = base_fee.checked_add(priority);
    if fully_funded_bid.is_none_or(|bid| bid > max_fee) {
        return Err(error(
            "FEE_BOUND_CHANGED",
            "submission",
            "Current base fee exceeds the approved frozen fee bid and full priority tip.",
        ).with_details(json!({"current_base_fee_wei":quantity(base_fee),"approved_max_fee_per_gas_wei":transaction.max_fee_per_gas,"approved_priority_fee_per_gas_wei":transaction.max_priority_fee_per_gas})));
    }
    Ok(())
}
pub fn address(value: &str) -> Result<Address> {
    Address::from_str(value).map_err(|_| {
        error(
            "INVALID_ADDRESS",
            "validation",
            "Expected an exact Ethereum address.",
        )
    })
}
pub fn hash(value: &str) -> Result<B256> {
    B256::from_str(value).map_err(|_| {
        error(
            "INVALID_HASH",
            "validation",
            "Expected an exact 32-byte Ethereum hash.",
        )
    })
}
pub fn u256(quantity: &Quantity) -> Result<U256> {
    U256::from_str(quantity.as_str()).map_err(|_| {
        error(
            "U256_OVERFLOW",
            "validation",
            "Atomic amount exceeds uint256.",
        )
    })
}
pub fn quantity(value: U256) -> Quantity {
    Quantity::new(value.to_string()).expect("U256 is a canonical unsigned quantity")
}
pub fn rpc_u256(value: &Value) -> Result<U256> {
    let value = value.as_str().ok_or_else(|| {
        error(
            "RPC_DATA_INVALID",
            "read",
            "RPC quantity must be a hexadecimal string.",
        )
    })?;
    if !value.starts_with("0x") || value.len() < 3 || value.len() > 66 {
        return Err(error(
            "RPC_DATA_INVALID",
            "read",
            "RPC quantity has an invalid hexadecimal shape.",
        ));
    }
    U256::from_str_radix(&value[2..], 16).map_err(|_| {
        error(
            "RPC_DATA_INVALID",
            "read",
            "RPC returned an invalid exact quantity.",
        )
    })
}
pub fn rpc_u64(value: &Value) -> Result<u64> {
    rpc_u256(value)?.try_into().map_err(|_| {
        error(
            "RPC_DATA_INVALID",
            "read",
            "RPC integer exceeds its u64 protocol bound.",
        )
    })
}
pub fn rpc_bytes(value: &Value) -> Result<Bytes> {
    let value = value.as_str().ok_or_else(|| {
        error(
            "RPC_DATA_INVALID",
            "read",
            "RPC byte data must be a hexadecimal string.",
        )
    })?;
    let raw = value.strip_prefix("0x").ok_or_else(|| {
        error(
            "RPC_DATA_INVALID",
            "read",
            "RPC bytes lack a hexadecimal prefix.",
        )
    })?;
    hex::decode(raw).map(Bytes::from).map_err(|_| {
        error(
            "RPC_DATA_INVALID",
            "read",
            "RPC returned malformed byte data.",
        )
    })
}
pub fn hex_quantity(value: U256) -> String {
    format!("{value:#x}")
}
pub fn hex_bytes(value: &[u8]) -> String {
    format!("0x{}", hex::encode(value))
}
pub fn lower_address(value: Address) -> String {
    format!("{value:#x}")
}

/// The complete EIP-2612 permit authorization. `domain_separator` must come
/// from the broker's verified on-chain token-domain read; this type never
/// accepts an arbitrary digest to sign.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Eip2612Permit {
    pub chain_id: u64,
    pub token: String,
    pub owner: String,
    pub spender: String,
    pub value: Quantity,
    pub nonce: Quantity,
    pub deadline: u64,
    pub domain_separator: String,
}

/// One Permit2 `PermitSingle`, using its canonical domain (name `Permit2`, no
/// version field) and nested `PermitDetails` struct.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Permit2PermitSingle {
    pub chain_id: u64,
    pub verifying_contract: String,
    pub owner: String,
    pub token: String,
    pub amount: Quantity,
    pub expiration: u64,
    pub nonce: u64,
    pub spender: String,
    pub sig_deadline: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "permit",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[schemars(deny_unknown_fields)]
pub enum AuthorizedPermit {
    Eip2612(Eip2612Permit),
    Permit2Single(Permit2PermitSingle),
}

fn word(value: U256) -> [u8; 32] {
    value.to_be_bytes::<32>()
}
fn address_word(value: Address) -> [u8; 32] {
    let mut out = [0; 32];
    out[12..].copy_from_slice(value.as_slice());
    out
}
fn append_word(encoded: &mut Vec<u8>, value: [u8; 32]) {
    encoded.extend_from_slice(&value);
}
fn permit_error(message: &str) -> GoFinError {
    error("INVALID_AUTHORIZED_PERMIT", "validation", message)
}
fn nonzero_permit_address(value: &str) -> Result<Address> {
    let parsed = address(value).map_err(|_| permit_error("Permit contains an invalid address."))?;
    if parsed.is_zero() {
        return Err(permit_error("Permit addresses must be nonzero."));
    }
    Ok(parsed)
}
fn quantity_in_range(value: &Quantity, maximum: U256, label: &str) -> Result<U256> {
    let parsed = U256::from_str(value.as_str())
        .map_err(|_| permit_error("Permit quantity is not a canonical uint256 value."))?;
    if parsed.is_zero() || parsed >= maximum {
        return Err(permit_error(label));
    }
    Ok(parsed)
}
fn digest(domain_separator: B256, struct_hash: B256) -> B256 {
    let mut bytes = [0u8; 66];
    bytes[0] = 0x19;
    bytes[1] = 0x01;
    bytes[2..34].copy_from_slice(domain_separator.as_slice());
    bytes[34..].copy_from_slice(struct_hash.as_slice());
    keccak256(bytes)
}

impl Eip2612Permit {
    pub fn validate(&self, expected_chain_id: u64, now: u64) -> Result<()> {
        if self.chain_id == 0 || self.chain_id != expected_chain_id {
            return Err(permit_error(
                "Permit chain differs from the configured wallet chain.",
            ));
        }
        nonzero_permit_address(&self.token)?;
        nonzero_permit_address(&self.owner)?;
        nonzero_permit_address(&self.spender)?;
        quantity_in_range(
            &self.value,
            U256::MAX,
            "Permit value must be finite and positive.",
        )?;
        // EIP-2612 nonce is a uint256, and may be zero.
        U256::from_str(self.nonce.as_str())
            .map_err(|_| permit_error("Permit nonce exceeds uint256."))?;
        hash(&self.domain_separator)
            .map_err(|_| permit_error("Permit domain separator must be exactly 32 bytes."))?;
        if self.deadline <= now {
            return Err(permit_error("Permit deadline has expired."));
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<B256> {
        let owner = nonzero_permit_address(&self.owner)?;
        let spender = nonzero_permit_address(&self.spender)?;
        let value = quantity_in_range(
            &self.value,
            U256::MAX,
            "Permit value must be finite and positive.",
        )?;
        let nonce = U256::from_str(self.nonce.as_str())
            .map_err(|_| permit_error("Permit nonce exceeds uint256."))?;
        let domain = hash(&self.domain_separator)
            .map_err(|_| permit_error("Permit domain separator must be exactly 32 bytes."))?;
        let type_hash = keccak256(
            "Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)",
        );
        let mut encoded = Vec::with_capacity(192);
        append_word(&mut encoded, type_hash.0);
        append_word(&mut encoded, address_word(owner));
        append_word(&mut encoded, address_word(spender));
        append_word(&mut encoded, word(value));
        append_word(&mut encoded, word(nonce));
        append_word(&mut encoded, word(U256::from(self.deadline)));
        Ok(digest(domain, keccak256(encoded)))
    }
}

impl Permit2PermitSingle {
    pub fn validate(&self, expected_chain_id: u64, now: u64) -> Result<()> {
        if self.chain_id == 0 || self.chain_id != expected_chain_id {
            return Err(permit_error(
                "Permit chain differs from the configured wallet chain.",
            ));
        }
        nonzero_permit_address(&self.verifying_contract)?;
        nonzero_permit_address(&self.owner)?;
        nonzero_permit_address(&self.token)?;
        nonzero_permit_address(&self.spender)?;
        quantity_in_range(
            &self.amount,
            (U256::from(1) << 160) - U256::from(1),
            "Permit2 amount must be finite, positive, and below uint160 max.",
        )?;
        let max_u48 = (1u64 << 48) - 1;
        if self.expiration > max_u48 || self.nonce > max_u48 || self.sig_deadline <= now {
            return Err(permit_error(
                "Permit2 expiration, nonce, or signature deadline is invalid.",
            ));
        }
        if self.expiration != 0 && self.expiration <= now {
            return Err(permit_error("Permit2 allowance expiration has expired."));
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<B256> {
        let verifying_contract = nonzero_permit_address(&self.verifying_contract)?;
        let token = nonzero_permit_address(&self.token)?;
        let spender = nonzero_permit_address(&self.spender)?;
        let amount = quantity_in_range(
            &self.amount,
            (U256::from(1) << 160) - U256::from(1),
            "Permit2 amount must be finite, positive, and below uint160 max.",
        )?;
        let details_type_hash =
            keccak256("PermitDetails(address token,uint160 amount,uint48 expiration,uint48 nonce)");
        let mut details = Vec::with_capacity(160);
        append_word(&mut details, details_type_hash.0);
        append_word(&mut details, address_word(token));
        append_word(&mut details, word(amount));
        append_word(&mut details, word(U256::from(self.expiration)));
        append_word(&mut details, word(U256::from(self.nonce)));
        let details_hash = keccak256(details);

        let single_type_hash = keccak256(
            "PermitSingle(PermitDetails details,address spender,uint256 sigDeadline)PermitDetails(address token,uint160 amount,uint48 expiration,uint48 nonce)",
        );
        let mut single = Vec::with_capacity(128);
        append_word(&mut single, single_type_hash.0);
        append_word(&mut single, details_hash.0);
        append_word(&mut single, address_word(spender));
        append_word(&mut single, word(U256::from(self.sig_deadline)));
        let struct_hash = keccak256(single);

        let domain_type_hash =
            keccak256("EIP712Domain(string name,uint256 chainId,address verifyingContract)");
        let name_hash = keccak256("Permit2");
        let mut domain = Vec::with_capacity(128);
        append_word(&mut domain, domain_type_hash.0);
        append_word(&mut domain, name_hash.0);
        append_word(&mut domain, word(U256::from(self.chain_id)));
        append_word(&mut domain, address_word(verifying_contract));
        Ok(digest(keccak256(domain), struct_hash))
    }
}

impl AuthorizedPermit {
    pub fn owner(&self) -> Result<Address> {
        match self {
            Self::Eip2612(p) => nonzero_permit_address(&p.owner),
            Self::Permit2Single(p) => nonzero_permit_address(&p.owner),
        }
    }
    pub fn validate(&self, expected_chain_id: u64, now: u64) -> Result<()> {
        match self {
            Self::Eip2612(p) => p.validate(expected_chain_id, now),
            Self::Permit2Single(p) => p.validate(expected_chain_id, now),
        }
    }
    pub fn digest(&self) -> Result<B256> {
        match self {
            Self::Eip2612(p) => p.digest(),
            Self::Permit2Single(p) => p.digest(),
        }
    }
    fn chain_id(&self) -> u64 {
        match self {
            Self::Eip2612(p) => p.chain_id,
            Self::Permit2Single(p) => p.chain_id,
        }
    }
}

#[async_trait]
pub trait EvmRpc: Send + Sync {
    /// Internal runtime boundary, never an agent-visible raw-method endpoint.
    async fn request(&self, method: &str, params: Value) -> Result<Value>;

    /// Caller-owned evidence constraints, without weakening custom transports.
    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        requirements.validate_request(method, &params)?;
        let value = self.request(method, params).await?;
        requirements.validate(method, &value)?;
        Ok(value)
    }

    /// Observed transport health only; no URLs, credentials or active probes.
    fn diagnostics(&self) -> Value {
        Value::Null
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RpcReadRequirements {
    pub minimum_block: Option<u64>,
    /// Bind a typed state read to the canonical hash before any fallback.
    pub pin_block: Option<u64>,
}

impl RpcReadRequirements {
    fn validate_request(self, method: &str, params: &Value) -> Result<()> {
        if method == "eth_sendRawTransaction" {
            return Err(GoFinError::validation(
                "Read requirements cannot authorize a broadcast.",
            ));
        }
        if self.minimum_block.is_some()
            && !matches!(method, "eth_blockNumber" | "eth_getBlockByNumber")
        {
            return Err(GoFinError::validation(
                "Minimum block applies only to block observations.",
            ));
        }
        if let Some(height) = self.pin_block {
            let index = match method {
                "eth_getCode" | "eth_getBalance" | "eth_call" => 1,
                "eth_getStorageAt" | "eth_getProof" => 2,
                _ => {
                    return Err(GoFinError::validation(
                        "Block pinning applies only to state reads.",
                    ));
                }
            };
            if rpc_u64(&params[index]).ok() != Some(height) {
                return Err(GoFinError::validation(
                    "Pinned state parameters must preserve the required block number.",
                ));
            }
        }
        Ok(())
    }
    fn validate(self, method: &str, value: &Value) -> Result<()> {
        if let Some(minimum) = self.minimum_block {
            let height = match method {
                "eth_blockNumber" => rpc_u64(value)?,
                "eth_getBlockByNumber" if !value.is_null() => rpc_u64(&value["number"])?,
                "eth_getBlockByNumber" => return Err(rpc_selection::missing_evidence()),
                _ => {
                    return Err(GoFinError::validation(
                        "Minimum block applies only to block observations.",
                    ));
                }
            };
            if height < minimum {
                return Err(rpc_selection::missing_evidence());
            }
        }
        Ok(())
    }
}

pub struct HttpRpc {
    urls: Vec<String>,
    expected_chain_id: u64,
    client: reqwest::Client,
    write_client: reqwest::Client,
    verified: Mutex<BTreeSet<String>>,
    read_cooldowns: Mutex<BTreeMap<String, Instant>>,
    write_identity_timeout: Duration,
    selection: Mutex<SelectionState>,
}

struct RpcAttemptFailure {
    error: GoFinError,
    retryable: bool,
}

fn endpoint_access_failure(error: &GoFinError) -> bool {
    error
        .details
        .as_deref()
        .and_then(|details| details.get("http_status"))
        .and_then(Value::as_u64)
        .is_some_and(|status| matches!(status, 401 | 403 | 404 | 405))
}

const RPC_WRITE_IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
const RPC_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_RPC_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_READ_RETRIES_PER_ENDPOINT: usize = 2;
const READ_RETRY_DELAYS_MS: [u64; MAX_READ_RETRIES_PER_ENDPOINT] = [100, 250];
const MAX_REVERT_DIAGNOSTIC_BYTES: usize = 4096;

fn retry_after_delay(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let mut values = headers.get_all(reqwest::header::RETRY_AFTER).iter();
    let value = values.next()?.to_str().ok()?.trim_matches([' ', '\t']);
    // Retry-After is a single value; a date's comma is not a list separator.
    if values.next().is_some() || value.is_empty() {
        return None;
    }
    let delay = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        // All-digit overflow is still an excessively long delay, not invalid syntax.
        Duration::from_secs(value.parse::<u64>().unwrap_or(u64::MAX))
    } else {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .unwrap_or_default()
    };
    Some(delay.min(MAX_RPC_RATE_LIMIT_COOLDOWN))
}

fn has_server_retry_delay(error: &GoFinError) -> bool {
    error
        .details
        .as_deref()
        .and_then(|details| details.get("retry_after_ms"))
        .and_then(Value::as_u64)
        .is_some_and(|milliseconds| milliseconds > 0)
}

fn four_byte_selector(bytes: &[u8]) -> Option<String> {
    let selector: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(format!("0x{}", hex::encode(selector)))
}

fn hex_revert_payload(value: &Value) -> Option<Vec<u8>> {
    let text = value.as_str()?.strip_prefix("0x")?;
    if text.len() < 8
        || text.len() % 2 != 0
        || text.len() > MAX_REVERT_DIAGNOSTIC_BYTES.saturating_mul(2)
    {
        return None;
    }
    hex::decode(text).ok()
}

fn universal_router_inner_error_selector(bytes: &[u8]) -> Option<String> {
    // Universal Router's `ExecutionFailed(uint256,bytes)` wraps the original
    // revert payload in a canonical ABI dynamic-bytes field. Bound both the
    // payload and every offset before extracting only its four-byte selector.
    let wrapper = keccak256("ExecutionFailed(uint256,bytes)");
    if bytes.get(..4)? != &wrapper[..4] {
        return None;
    }
    let offset = U256::from_be_slice(bytes.get(36..68)?);
    if offset != U256::from(64) {
        return None;
    }
    let length = usize::try_from(U256::from_be_slice(bytes.get(68..100)?)).ok()?;
    if !(4..=MAX_REVERT_DIAGNOSTIC_BYTES).contains(&length) {
        return None;
    }
    let padded_length = length.checked_add(31)? / 32 * 32;
    let payload_end = 100usize.checked_add(padded_length)?;
    if bytes.len() != payload_end {
        return None;
    }
    let payload = bytes.get(100..100usize.checked_add(length)?)?;
    if bytes
        .get(100 + length..payload_end)?
        .iter()
        .any(|byte| *byte != 0)
    {
        return None;
    }
    four_byte_selector(payload)
}

fn rpc_rejection(details: &Value, historical_state_fallback: bool) -> RpcAttemptFailure {
    // Read only an exact signed integer. Provider messages, data and other
    // fields can contain request parameters or private revert payloads.
    let code = details.get("code").and_then(Value::as_i64);
    let normalized_message = details
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Some Nitro providers include the unavailable state root between the
    // words. Accept only that exact shape; unrelated errors still fail closed.
    let unavailable_state_root = normalized_message
        .strip_prefix("historical state ")
        .and_then(|message| message.strip_suffix(" is not available"))
        .is_some_and(|root| {
            let root = root.strip_prefix("0x").unwrap_or(root);
            root.len() == 64 && root.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    let historical_state_unavailable = historical_state_fallback
        && matches!(code, Some(-32000 | -32001))
        && !normalized_message.contains("execution reverted")
        && !normalized_message.contains("revert reason")
        && (normalized_message.contains("missing trie node")
            || normalized_message.contains("missing historical state")
            || normalized_message.contains("historical state unavailable")
            || normalized_message.contains("historical state is unavailable")
            || normalized_message.contains("historical state is not available")
            || normalized_message.contains("historical state has been pruned")
            || normalized_message.contains("state is not available")
            || normalized_message.contains("state unavailable")
            || unavailable_state_root);
    let message = match code {
        Some(_) if historical_state_unavailable => {
            "RPC endpoint cannot serve the requested historical state; trying the next configured read endpoint.".to_owned()
        }
        Some(code) => format!(
            "RPC request was rejected (JSON-RPC code {code}); external details are suppressed."
        ),
        None => "RPC request was rejected; external details are suppressed.".to_owned(),
    };
    let mut error = error(
        if historical_state_unavailable {
            "RPC_HISTORICAL_STATE_UNAVAILABLE"
        } else {
            "RPC_REQUEST_FAILED"
        },
        "rpc",
        &message,
    );
    let mut diagnostics = serde_json::Map::new();
    if let Some(code) = code {
        diagnostics.insert("json_rpc_code".into(), json!(code));
    }
    if details
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.starts_with("execution reverted"))
    {
        diagnostics.insert("error_class".into(), json!("contract_revert"));
        if let Some(bytes) = details.get("data").and_then(hex_revert_payload) {
            if let Some(selector) = four_byte_selector(&bytes) {
                diagnostics.insert("error_selector".into(), Value::String(selector));
            }
            if let Some(selector) = universal_router_inner_error_selector(&bytes) {
                diagnostics.insert("inner_error_selector".into(), Value::String(selector));
            }
        }
    }
    if !diagnostics.is_empty() {
        error = error.with_details(Value::Object(diagnostics));
    }
    RpcAttemptFailure {
        retryable: historical_state_unavailable
            || code.is_some_and(|code| matches!(code, -32002 | -32005)),
        error,
    }
}

fn has_historical_block_tag(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if let Some(tag) = value.as_str() {
        return tag.strip_prefix("0x").is_some_and(|hex| {
            !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    }
    // Canonical EIP-1898 state reads may use another configured archive endpoint
    // without replacing the block identity or falling back to latest state.
    value.as_object().is_some_and(|object| {
        object.len() == 2
            && object.get("requireCanonical") == Some(&Value::Bool(true))
            && object
                .get("blockHash")
                .and_then(Value::as_str)
                .is_some_and(|value| hash(value).is_ok())
    })
}

fn is_historical_state_read(method: &str, params: &Value) -> bool {
    let params = match params.as_array() {
        Some(params) => params,
        None => return false,
    };
    match method {
        "eth_getCode" | "eth_getBalance" => has_historical_block_tag(params.get(1)),
        "eth_getStorageAt" | "eth_getProof" => has_historical_block_tag(params.get(2)),
        "eth_call" => has_historical_block_tag(params.get(1)),
        _ => false,
    }
}

impl HttpRpc {
    pub fn new(urls: Vec<String>, expected_chain_id: u64) -> Result<Self> {
        Self::construct(urls, expected_chain_id, false)
    }
    /// Isolated fork harness only. Production broker construction must use `new`.
    pub fn isolated_localhost(url: String, expected_chain_id: u64) -> Result<Self> {
        Self::isolated_localhost_pool(vec![url], expected_chain_id)
    }
    /// Isolated synthetic/fork harness only, with multiple loopback endpoints.
    /// Production broker construction must use `new`.
    pub fn isolated_localhost_pool(urls: Vec<String>, expected_chain_id: u64) -> Result<Self> {
        Self::construct(urls, expected_chain_id, true)
    }
    fn construct(urls: Vec<String>, expected_chain_id: u64, local: bool) -> Result<Self> {
        if urls.is_empty() || expected_chain_id == 0 {
            return Err(GoFinError::validation(
                "RPC URLs and an explicit nonzero chain identity are required.",
            ));
        }
        for text in &urls {
            let url = reqwest::Url::parse(text)
                .map_err(|_| GoFinError::validation("Invalid RPC URL."))?;
            let loopback = matches!(
                url.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            );
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || (!local && (url.scheme() != "https" || loopback))
                || (local && (!loopback || url.scheme() != "http"))
            {
                return Err(GoFinError::validation(
                    "Production RPCs require public HTTPS without embedded credentials; isolated fixtures require loopback HTTP.",
                ));
            }
        }
        let transport_error = |_| {
            error(
                "RPC_UNAVAILABLE",
                "configuration",
                "Could not initialize the public RPC transport.",
            )
        };
        // Reads wait for usable evidence or caller cancellation. Keep the
        // existing HTTP timeout on the separate financial broadcast client.
        let client = reqwest::Client::builder()
            .build()
            .map_err(transport_error)?;
        let write_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            urls,
            expected_chain_id,
            client,
            write_client,
            verified: Mutex::new(BTreeSet::new()),
            read_cooldowns: Mutex::new(BTreeMap::new()),
            write_identity_timeout: RPC_WRITE_IDENTITY_TIMEOUT,
            selection: Mutex::new(SelectionState::default()),
        })
    }
    fn write_identity_timeout_error(&self) -> GoFinError {
        error("RPC_READ_TIMEOUT", "rpc", "The primary RPC identity check exceeded its pre-dispatch deadline; no transaction was sent.")
            .with_details(json!({"chain_id": self.expected_chain_id, "method": "eth_chainId", "timeout_ms": self.write_identity_timeout.as_millis()}))
    }
    fn read_cooldown_remaining(&self, url: &str) -> Result<Option<Duration>> {
        let mut cooldowns = self.read_cooldowns.lock().map_err(|_| {
            error(
                "RPC_UNAVAILABLE",
                "rpc",
                "RPC cooldown state is unavailable.",
            )
        })?;
        let remaining = cooldowns
            .get(url)
            .and_then(|until| until.checked_duration_since(Instant::now()))
            .filter(|remaining| !remaining.is_zero());
        if remaining.is_none() {
            cooldowns.remove(url);
        }
        Ok(remaining)
    }

    fn record_read_rate_limit(&self, url: &str, delay: Duration) -> Result<()> {
        let until = Instant::now() + delay;
        self.read_cooldowns
            .lock()
            .map_err(|_| {
                error(
                    "RPC_UNAVAILABLE",
                    "rpc",
                    "RPC cooldown state is unavailable.",
                )
            })?
            .entry(url.to_owned())
            .and_modify(|previous| *previous = (*previous).max(until))
            .or_insert(until);
        Ok(())
    }
    async fn at_once(
        &self,
        url: &str,
        method: &str,
        params: Value,
        historical_state_fallback: bool,
    ) -> std::result::Result<Value, RpcAttemptFailure> {
        let client = if method == "eth_sendRawTransaction" {
            &self.write_client
        } else {
            &self.client
        };
        let response = client
            .post(url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|failure| RpcAttemptFailure {
                retryable: failure.is_timeout() || failure.is_connect() || failure.is_body(),
                error: error(
                    "RPC_UNAVAILABLE",
                    "rpc",
                    "RPC request did not return a usable response.",
                ),
            })?;
        if !response.status().is_success() {
            let status = response.status();
            let mut details = json!({"http_status":status.as_u16()});
            if status.as_u16() == 429 && method != "eth_sendRawTransaction" {
                let retry_after = retry_after_delay(response.headers(), SystemTime::now());
                self.record_read_rate_limit(url, retry_after.unwrap_or(RPC_RATE_LIMIT_COOLDOWN))
                    .map_err(|error| RpcAttemptFailure {
                        error,
                        retryable: false,
                    })?;
                if let Some(delay) = retry_after {
                    details["retry_after_ms"] = json!(delay.as_nanos().div_ceil(1_000_000));
                }
            }
            return Err(RpcAttemptFailure {
                retryable: matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504),
                error: error("RPC_UNAVAILABLE", "rpc", "RPC HTTP request failed.")
                    .with_details(details),
            });
        }
        let body = response
            .bytes()
            .await
            .map_err(|failure| RpcAttemptFailure {
                retryable: failure.is_timeout() || failure.is_body(),
                error: error(
                    "RPC_UNAVAILABLE",
                    "rpc",
                    "RPC response body could not be read.",
                ),
            })?;
        let result: Value = serde_json::from_slice(&body).map_err(|_| RpcAttemptFailure {
            retryable: false,
            error: error(
                "RPC_DATA_INVALID",
                "rpc",
                "RPC response was not valid JSON.",
            ),
        })?;
        if result.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || result.get("id").and_then(Value::as_u64) != Some(1)
        {
            return Err(RpcAttemptFailure {
                retryable: false,
                error: error(
                    "RPC_DATA_INVALID",
                    "rpc",
                    "RPC response identity is invalid.",
                ),
            });
        }
        if let Some(details) = result.get("error") {
            if rpc_selection::unsupported_error(method, &params, details) {
                return Err(RpcAttemptFailure {
                    retryable: false,
                    error: error("RPC_UNSUPPORTED_CAPABILITY", "rpc", "RPC does not support the required read capability; trying another eligible endpoint.")
                        .with_details(json!({"json_rpc_code":details.get("code").and_then(Value::as_i64)})),
                });
            }
            return Err(rpc_rejection(details, historical_state_fallback));
        }
        result
            .get("result")
            .cloned()
            .ok_or_else(|| RpcAttemptFailure {
                retryable: false,
                error: error("RPC_DATA_INVALID", "rpc", "RPC result is missing."),
            })
    }
    async fn at(
        &self,
        url: &str,
        method: &str,
        params: Value,
        retries: usize,
        historical_state_fallback: bool,
    ) -> std::result::Result<Value, RpcAttemptFailure> {
        let mut retry_index = 0;
        loop {
            match self
                .at_once(url, method, params.clone(), historical_state_fallback)
                .await
            {
                Ok(result) => return Ok(result),
                Err(failure)
                    if failure.retryable
                        && retry_index < retries
                        && !has_server_retry_delay(&failure.error) =>
                {
                    let delay = READ_RETRY_DELAYS_MS
                        .get(retry_index)
                        .copied()
                        .unwrap_or(READ_RETRY_DELAYS_MS[READ_RETRY_DELAYS_MS.len() - 1]);
                    retry_index += 1;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                Err(failure) => {
                    let error = if failure.error.code == "RPC_HISTORICAL_STATE_UNAVAILABLE" {
                        preserve_rpc_cause(
                            error(
                                "RPC_HISTORICAL_STATE_UNAVAILABLE",
                                "rpc",
                                "Configured RPC endpoint could not serve historical state after bounded retries.",
                            ),
                            &failure.error,
                        )
                    } else if failure.retryable {
                        let message = if failure.error.code == "RPC_REQUEST_FAILED" {
                            // This message is generated locally by rpc_rejection;
                            // retain its validated numeric cause, never provider text.
                            format!(
                                "Transient RPC failure persisted after bounded retries. {}",
                                failure.error.message
                            )
                        } else {
                            "Transient RPC failure persisted after bounded retries.".to_owned()
                        };
                        preserve_rpc_cause(
                            error("RPC_UNAVAILABLE", "rpc", &message),
                            &failure.error,
                        )
                    } else {
                        failure.error
                    };
                    return Err(RpcAttemptFailure {
                        error,
                        retryable: failure.retryable,
                    });
                }
            }
        }
    }
    async fn ensure_endpoint(&self, url: &str) -> std::result::Result<(), RpcAttemptFailure> {
        if self
            .verified
            .lock()
            .map_err(|_| RpcAttemptFailure {
                retryable: false,
                error: error(
                    "RPC_UNAVAILABLE",
                    "rpc",
                    "RPC identity state is unavailable.",
                ),
            })?
            .contains(url)
        {
            return Ok(());
        }
        let chain_id_response = self
            .at(
                url,
                "eth_chainId",
                json!([]),
                MAX_READ_RETRIES_PER_ENDPOINT,
                false,
            )
            .await?;
        let chain_id = rpc_u64(&chain_id_response).map_err(|failure| RpcAttemptFailure {
            retryable: false,
            error: failure,
        })?;
        if chain_id != self.expected_chain_id {
            return Err(RpcAttemptFailure {
                retryable: false,
                error: error(
                    "WRONG_CHAIN",
                    "rpc",
                    "RPC chain identity differs from the configured network.",
                ),
            });
        }
        self.verified
            .lock()
            .map_err(|_| RpcAttemptFailure {
                retryable: false,
                error: error(
                    "RPC_UNAVAILABLE",
                    "rpc",
                    "RPC identity state is unavailable.",
                ),
            })?
            .insert(url.to_owned());
        Ok(())
    }
}
impl HttpRpc {
    async fn request_internal(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        const READS: &[&str] = &[
            "eth_chainId",
            "eth_getCode",
            "eth_getBalance",
            "eth_getStorageAt",
            "eth_getLogs",
            "eth_getProof",
            "eth_call",
            "eth_estimateGas",
            "eth_maxPriorityFeePerGas",
            "eth_getTransactionCount",
            "eth_blockNumber",
            "eth_getBlockByNumber",
            "eth_getTransactionByHash",
            "eth_getTransactionReceipt",
        ];
        if !READS.contains(&method) && method != "eth_sendRawTransaction" {
            return Err(error(
                "RPC_METHOD_UNREVIEWED",
                "rpc",
                "This RPC method is outside the reviewed runtime.",
            ));
        }
        let result = if method == "eth_sendRawTransaction" {
            // No read race or automatic retry can replay a write. Identity is
            // bounded before dispatch; the broadcast keeps its HTTP timeout.
            let primary = &self.urls[0];
            match tokio::time::timeout(self.write_identity_timeout, self.ensure_endpoint(primary))
                .await
            {
                Err(_) => Err(self.write_identity_timeout_error()),
                Ok(Err(failure)) => Err(failure.error),
                Ok(Ok(())) => self
                    .at(primary, method, params, 0, false)
                    .await
                    .map_err(|failure| failure.error),
            }
        } else {
            self.raced_read(method, params, requirements).await
        };
        result.map_err(|mut error| {
            // The snapshot contains reviewed method names, codes and rpc-N IDs only.
            let mut details = error
                .details
                .take()
                .map(|value| *value)
                .unwrap_or_else(|| json!({}));
            details["rpc_health"] = self.selection_diagnostics();
            error.details = Some(Box::new(details));
            error
        })
    }
}

#[async_trait]
impl EvmRpc for HttpRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_internal(method, params, RpcReadRequirements::default())
            .await
    }
    async fn request_checked(
        &self,
        method: &str,
        params: Value,
        requirements: RpcReadRequirements,
    ) -> Result<Value> {
        requirements.validate_request(method, &params)?;
        self.request_internal(method, params, requirements).await
    }
    fn diagnostics(&self) -> Value {
        self.selection_diagnostics()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrozenTransaction {
    pub chain_id: u64,
    pub sender: String,
    pub to: String,
    pub value: Quantity,
    pub calldata: String,
    pub nonce: u64,
    pub gas: u64,
    pub max_fee_per_gas: Quantity,
    pub max_priority_fee_per_gas: Quantity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollup_fee: Option<RollupFeeBound>,
}
impl FrozenTransaction {
    pub fn consensus(&self) -> Result<TxEip1559> {
        if self.chain_id == 0
            || self.gas == 0
            || self.max_priority_fee_per_gas > self.max_fee_per_gas
        {
            return Err(error(
                "INVALID_ENVELOPE",
                "validation",
                "The frozen transaction fee, gas or chain bounds are invalid.",
            ));
        }
        let max_fee = u256(&self.max_fee_per_gas)?.try_into().map_err(|_| {
            error(
                "INVALID_ENVELOPE",
                "validation",
                "Fee exceeds the EIP-1559 u128 bound.",
            )
        })?;
        let priority = u256(&self.max_priority_fee_per_gas)?
            .try_into()
            .map_err(|_| {
                error(
                    "INVALID_ENVELOPE",
                    "validation",
                    "Priority fee exceeds the EIP-1559 u128 bound.",
                )
            })?;
        address(&self.sender)?;
        Ok(TxEip1559 {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_limit: self.gas,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: priority,
            to: TxKind::Call(address(&self.to)?),
            value: u256(&self.value)?,
            input: rpc_bytes(&json!(self.calldata))?,
            access_list: AccessList::default(),
        })
    }
    pub fn rpc_call(&self) -> Value {
        json!({"from":self.sender,"to":self.to,"value":hex_quantity(u256(&self.value).expect("validated envelope")),"data":self.calldata,
          "gas":format!("{:#x}",self.gas),"maxFeePerGas":hex_quantity(u256(&self.max_fee_per_gas).expect("validated envelope")),
          "maxPriorityFeePerGas":hex_quantity(u256(&self.max_priority_fee_per_gas).expect("validated envelope"))})
    }
    pub fn maximum_cost(&self) -> Result<U256> {
        U256::from(self.gas)
            .checked_mul(u256(&self.max_fee_per_gas)?)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "validation",
                    "Maximum gas cost overflows uint256.",
                )
            })?
            .checked_add(fees::bound_cost(self)?)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "validation",
                    "Total fee bound overflows uint256.",
                )
            })
    }
}

#[async_trait]
pub trait EnvelopeSigner: Send + Sync {
    /// Never called until the broker's single-write journal confirms authorization.
    async fn sign_prepared(&self, transaction: &FrozenTransaction) -> Result<Bytes>;

    /// Typed permit signing is deliberately separate from transaction/hash
    /// signing. Providers that do not implement reviewed permit signing fail closed.
    async fn sign_authorized_permit(&self, _permit: &AuthorizedPermit) -> Result<Bytes> {
        Err(error(
            "PERMIT_SIGNING_UNAVAILABLE",
            "signing",
            "The protected signing provider does not support typed permit signing.",
        ))
    }
}
pub struct LocalEnvelopeSigner {
    signer: PrivateKeySigner,
}
impl LocalEnvelopeSigner {
    /// Broker credential enrollment/runtime only. No CLI operation returns this object or its secret.
    pub fn from_private_key(secret: &str) -> Result<Self> {
        Ok(Self {
            signer: secret.parse().map_err(|_| {
                error(
                    "INVALID_SIGNER",
                    "credentials",
                    "Signing credential is invalid.",
                )
            })?,
        })
    }
    pub fn address(&self) -> Address {
        self.signer.address()
    }
}
#[async_trait]
impl EnvelopeSigner for LocalEnvelopeSigner {
    async fn sign_prepared(&self, transaction: &FrozenTransaction) -> Result<Bytes> {
        if address(&transaction.sender)? != self.signer.address() {
            return Err(error(
                "SIGNER_MISMATCH",
                "signing",
                "Protected signer does not match the frozen sender.",
            ));
        }
        let tx = transaction.consensus()?;
        let signature = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .map_err(|_| {
                error(
                    "SIGNING_FAILED",
                    "signing",
                    "Protected signing did not succeed.",
                )
            })?;
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        use alloy::eips::eip2718::Encodable2718;
        Ok(envelope.encoded_2718().into())
    }

    async fn sign_authorized_permit(&self, permit: &AuthorizedPermit) -> Result<Bytes> {
        if permit.owner()? != self.signer.address() {
            return Err(error(
                "SIGNER_MISMATCH",
                "signing",
                "Protected signer does not match the typed permit owner.",
            ));
        }
        let digest = permit.digest()?;
        let signature = self.signer.sign_hash_sync(&digest).map_err(|_| {
            error(
                "SIGNING_FAILED",
                "signing",
                "Protected signing did not succeed.",
            )
        })?;
        if signature
            .recover_address_from_prehash(&digest)
            .map_err(|_| {
                error(
                    "SIGNING_FAILED",
                    "signing",
                    "Permit signer recovery failed.",
                )
            })?
            != permit.owner()?
        {
            return Err(error(
                "SIGNER_MISMATCH",
                "signing",
                "Typed permit signature does not recover the authorized owner.",
            ));
        }
        Ok(Bytes::from(signature.as_bytes().to_vec()))
    }
}

pub fn verify_signed_envelope(transaction: &FrozenTransaction, raw: &[u8]) -> Result<B256> {
    let envelope = TxEnvelope::decode_2718_exact(raw).map_err(|_| {
        error(
            "SIGNED_TRANSACTION_MISMATCH",
            "signing",
            "Signer returned a malformed or noncanonical transaction envelope.",
        )
    })?;
    let tx = envelope.as_eip1559().ok_or_else(|| {
        error(
            "SIGNED_TRANSACTION_MISMATCH",
            "signing",
            "Only the frozen EIP-1559 transaction type is supported.",
        )
    })?;
    if tx.tx() != &transaction.consensus()?
        || envelope.recover_signer().map_err(|_| {
            error(
                "SIGNED_TRANSACTION_MISMATCH",
                "signing",
                "Signed sender could not be recovered.",
            )
        })? != address(&transaction.sender)?
    {
        return Err(error(
            "SIGNED_TRANSACTION_MISMATCH",
            "signing",
            "Signer returned fields outside the authorized frozen envelope.",
        ));
    }
    let calculated = keccak256(raw);
    if calculated != *envelope.tx_hash() {
        return Err(error(
            "SIGNED_TRANSACTION_MISMATCH",
            "signing",
            "Canonical envelope hash mismatch.",
        ));
    }
    Ok(calculated)
}

pub struct EvmRuntime {
    pub rpc: Arc<dyn EvmRpc>,
    signer: Option<Arc<dyn EnvelopeSigner>>,
    pub chain_id: u64,
}
#[derive(Clone, Debug)]
pub struct PreparedCall {
    pub transaction: FrozenTransaction,
    pub simulation: Bytes,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub enum SignatureLayout {
    Packed65 {
        offset: usize,
    },
    /// ABI-word offsets for `v`, `r` and `s`, each pointing at a distinct
    /// 32-byte slot in the complete calldata.
    Eip2612Abi {
        v_offset: usize,
        r_offset: usize,
        s_offset: usize,
    },
}

impl SignatureLayout {
    fn ranges(&self, calldata_len: usize) -> Result<Vec<(usize, usize)>> {
        let ranges = match self {
            Self::Packed65 { offset } => vec![(*offset, offset.checked_add(65))],
            Self::Eip2612Abi {
                v_offset,
                r_offset,
                s_offset,
            } => vec![
                (*v_offset, v_offset.checked_add(32)),
                (*r_offset, r_offset.checked_add(32)),
                (*s_offset, s_offset.checked_add(32)),
            ],
        };
        let mut resolved = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            let end = end.ok_or_else(|| {
                error(
                    "INVALID_PERMIT_TEMPLATE",
                    "validation",
                    "Permit signature slot offset overflowed.",
                )
            })?;
            if end > calldata_len {
                return Err(error(
                    "INVALID_PERMIT_TEMPLATE",
                    "validation",
                    "Permit signature slot lies outside calldata.",
                ));
            }
            resolved.push((start, end));
        }
        let mut sorted = resolved.clone();
        sorted.sort_unstable();
        if sorted.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(error(
                "INVALID_PERMIT_TEMPLATE",
                "validation",
                "Permit signature slots overlap.",
            ));
        }
        Ok(resolved)
    }

    fn validate_zero_template(&self, calldata: &[u8]) -> Result<()> {
        for (start, end) in self.ranges(calldata.len())? {
            if calldata[start..end].iter().any(|byte| *byte != 0) {
                return Err(error(
                    "INVALID_PERMIT_TEMPLATE",
                    "validation",
                    "Permit signature template slots must be zeroed.",
                ));
            }
        }
        Ok(())
    }

    fn materialize(&self, calldata: &mut [u8], signature: &[u8]) -> Result<()> {
        if signature.len() != 65 || !matches!(signature[64], 27 | 28) {
            return Err(error(
                "PERMIT_SIGNATURE_MISMATCH",
                "signing",
                "Protected permit signer returned a noncanonical 65-byte signature.",
            ));
        }
        self.validate_zero_template(calldata)?;
        match self {
            Self::Packed65 { offset } => {
                calldata[*offset..*offset + 65].copy_from_slice(signature);
            }
            Self::Eip2612Abi {
                v_offset,
                r_offset,
                s_offset,
            } => {
                calldata[*r_offset..*r_offset + 32].copy_from_slice(&signature[..32]);
                calldata[*s_offset..*s_offset + 32].copy_from_slice(&signature[32..64]);
                calldata[*v_offset..*v_offset + 32].fill(0);
                calldata[*v_offset + 31] = signature[64];
            }
        }
        Ok(())
    }

    fn signature_from(&self, calldata: &[u8]) -> Result<Bytes> {
        self.ranges(calldata.len())?;
        let signature = match self {
            Self::Packed65 { offset } => calldata[*offset..*offset + 65].to_vec(),
            Self::Eip2612Abi {
                v_offset,
                r_offset,
                s_offset,
            } => {
                if calldata[*v_offset..*v_offset + 31]
                    .iter()
                    .any(|byte| *byte != 0)
                {
                    return Err(error(
                        "PERMIT_SIGNATURE_MISMATCH",
                        "reconciliation",
                        "ABI permit signature has a malformed v word.",
                    ));
                }
                let mut bytes = Vec::with_capacity(65);
                bytes.extend_from_slice(&calldata[*r_offset..*r_offset + 32]);
                bytes.extend_from_slice(&calldata[*s_offset..*s_offset + 32]);
                bytes.push(calldata[*v_offset + 31]);
                bytes
            }
        };
        if signature[64] != 27 && signature[64] != 28 {
            return Err(error(
                "PERMIT_SIGNATURE_MISMATCH",
                "reconciliation",
                "Permit signature v must be 27 or 28.",
            ));
        }
        Ok(Bytes::from(signature))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct PreparedPermitTemplate {
    /// Preview envelope with an all-zero 65-byte signature slot. It must never
    /// be submitted; materialization replaces that slot after authorization.
    transaction: FrozenTransaction,
    permit: AuthorizedPermit,
    signature_layout: SignatureLayout,
}

/// Unsigned transaction fields frozen around a typed permit signature slot.
/// The adapter supplies a finite, reviewed gas limit and fee ceiling; this
/// request never contains a signature or a caller-provided digest.
#[derive(Clone, Debug)]
pub struct UnsignedPermitTransaction {
    pub sender: Address,
    pub to: Address,
    pub value: U256,
    pub calldata: Bytes,
    pub gas_limit: u64,
    pub maximum_total_fee: U256,
}
impl PreparedPermitTemplate {
    pub fn transaction(&self) -> &FrozenTransaction {
        &self.transaction
    }
    pub fn permit(&self) -> &AuthorizedPermit {
        &self.permit
    }
    pub fn signature_layout(&self) -> &SignatureLayout {
        &self.signature_layout
    }
}
fn verify_authorized_permit_signature(permit: &AuthorizedPermit, bytes: &[u8]) -> Result<()> {
    if bytes.len() != 65 || !matches!(bytes[64], 27 | 28) {
        return Err(error(
            "PERMIT_SIGNATURE_MISMATCH",
            "signing",
            "Protected permit signer returned a noncanonical 65-byte signature.",
        ));
    }
    let signature = Signature::from_raw(bytes).map_err(|_| {
        error(
            "PERMIT_SIGNATURE_MISMATCH",
            "signing",
            "Protected permit signer returned a malformed signature.",
        )
    })?;
    let digest = permit.digest()?;
    let recovered = signature
        .recover_address_from_prehash(&digest)
        .map_err(|_| {
            error(
                "PERMIT_SIGNATURE_MISMATCH",
                "signing",
                "Protected permit signature could not be recovered.",
            )
        })?;
    if recovered != permit.owner()? {
        return Err(error(
            "PERMIT_SIGNATURE_MISMATCH",
            "signing",
            "Protected permit signature does not recover the authorized owner.",
        ));
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub struct CanonicalReceipt {
    pub hash: B256,
    pub transaction_sender: String,
    pub block_hash: B256,
    pub block_number: u64,
    pub confirmations: u64,
    pub success: bool,
    pub fee: U256,
    pub receipt: Value,
}

fn receipt_inclusion_pending(receipt: &Value, transaction: &Value) -> Result<bool> {
    let mut pending = false;
    for response in [receipt, transaction] {
        for field in ["blockHash", "blockNumber"] {
            match response.get(field) {
                None | Some(Value::Null) => pending = true,
                Some(value) if field == "blockHash" => {
                    let value = value.as_str().ok_or_else(|| {
                        error(
                            "INVALID_HASH",
                            "validation",
                            "Expected an exact 32-byte Ethereum hash.",
                        )
                    })?;
                    hash(value)?;
                }
                Some(value) => {
                    rpc_u64(value)?;
                }
            }
        }
    }
    Ok(pending)
}

impl EvmRuntime {
    pub fn new(
        rpc: Arc<dyn EvmRpc>,
        chain_id: u64,
        signer: Option<Arc<dyn EnvelopeSigner>>,
    ) -> Self {
        Self {
            rpc,
            signer,
            chain_id,
        }
    }
    pub fn can_sign(&self) -> bool {
        self.signer.is_some()
    }
    pub async fn assert_chain(&self) -> Result<()> {
        if rpc_u64(&self.rpc.request("eth_chainId", json!([])).await?)? != self.chain_id {
            return Err(error(
                "WRONG_CHAIN",
                "read",
                "RPC chain differs from the configured immutable chain identity.",
            ));
        }
        Ok(())
    }
    pub async fn code(&self, contract: Address) -> Result<()> {
        self.code_at(contract, None).await
    }
    /// Observe deployment at the same historical block as the associated read.
    /// Writes continue to check latest deployment through `code`.
    pub async fn code_at(&self, contract: Address, block: Option<u64>) -> Result<()> {
        let code = rpc_bytes(
            &self
                .rpc
                .request_checked(
                    "eth_getCode",
                    json!([lower_address(contract), block_tag(block)]),
                    RpcReadRequirements {
                        pin_block: block,
                        ..Default::default()
                    },
                )
                .await?,
        )?;
        if code.is_empty() {
            return Err(error(
                "CONTRACT_UNAVAILABLE",
                "read",
                "The configured contract has no deployed code.",
            ));
        }
        Ok(())
    }
    pub async fn balance(&self, owner: Address, block: Option<u64>) -> Result<U256> {
        rpc_u256(
            &self
                .rpc
                .request_checked(
                    "eth_getBalance",
                    json!([lower_address(owner), block_tag(block)]),
                    RpcReadRequirements {
                        pin_block: block,
                        ..Default::default()
                    },
                )
                .await?,
        )
    }
    pub async fn call<C: SolCall>(
        &self,
        to: Address,
        call: &C,
        block: Option<u64>,
    ) -> Result<C::Return> {
        self.call_with_sender(None, to, call, block).await
    }
    /// Simulate a state-changing method with its real caller while preserving
    /// the ordinary caller-free form for public view reads.
    pub async fn call_from<C: SolCall>(
        &self,
        from: Address,
        to: Address,
        call: &C,
        block: Option<u64>,
    ) -> Result<C::Return> {
        self.call_with_sender(Some(from), to, call, block).await
    }
    async fn call_with_sender<C: SolCall>(
        &self,
        from: Option<Address>,
        to: Address,
        call: &C,
        block: Option<u64>,
    ) -> Result<C::Return> {
        let mut transaction = json!({
            "to":lower_address(to),
            "data":hex_bytes(&call.abi_encode())
        });
        if let Some(from) = from {
            transaction["from"] = json!(lower_address(from));
        }
        let raw = rpc_bytes(
            &self
                .rpc
                .request_checked(
                    "eth_call",
                    json!([transaction, block_tag(block)]),
                    RpcReadRequirements {
                        pin_block: block,
                        ..Default::default()
                    },
                )
                .await?,
        )?;
        C::abi_decode_returns(&raw).map_err(|_| {
            error(
                "CONTRACT_DATA_INVALID",
                "read",
                "Contract return does not match the reviewed ABI.",
            )
        })
    }
    pub async fn pending_nonce(&self, owner: Address) -> Result<u64> {
        rpc_u64(
            &self
                .rpc
                .request(
                    "eth_getTransactionCount",
                    json!([lower_address(owner), "pending"]),
                )
                .await?,
        )
    }
    pub async fn fees(&self) -> Result<(U256, U256)> {
        let (block, priority) = tokio::try_join!(
            self.rpc
                .request("eth_getBlockByNumber", json!(["latest", false])),
            self.rpc.request("eth_maxPriorityFeePerGas", json!([])),
        )?;
        let base = rpc_u256(&block["baseFeePerGas"])?;
        let priority = rpc_u256(&priority)?.max(U256::from(10_000_000));
        let max = base
            .checked_mul(U256::from(2))
            .and_then(|v| v.checked_add(priority))
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "preview",
                    "EIP-1559 fee computation overflowed.",
                )
            })?;
        Ok((max, priority))
    }
    pub async fn prepare(
        &self,
        sender: Address,
        to: Address,
        value: U256,
        calldata: Bytes,
        maximum_total_fee: U256,
    ) -> Result<PreparedCall> {
        self.assert_chain().await?;
        let request = json!({"from":lower_address(sender),"to":lower_address(to),"value":hex_quantity(value),"data":hex_bytes(&calldata)});
        let ((simulation, estimate), (required, priority), nonce) = tokio::try_join!(
            async {
                let (simulation, estimate) = tokio::try_join!(
                    self.rpc
                        .request("eth_call", json!([request.clone(), "latest"])),
                    self.rpc
                        .request("eth_estimateGas", json!([request.clone()])),
                )?;
                Ok::<_, GoFinError>((rpc_bytes(&simulation)?, rpc_u64(&estimate)?))
            },
            self.fees(),
            self.pending_nonce(sender),
        )?;
        let gas = estimate
            .checked_mul(120)
            .and_then(|v| v.checked_add(99))
            .map(|v| v / 100)
            .ok_or_else(|| {
                error(
                    "INVALID_GAS",
                    "preview",
                    "Gas estimate exceeds its protocol bound.",
                )
            })?;
        let affordable = maximum_total_fee / U256::from(gas.max(1));
        if affordable < required {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "Current required fee exceeds the explicit total fee bound.",
            ));
        }
        let max = ceil_percent(required, 150)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "preview",
                    "Conservative fee headroom overflowed.",
                )
            })?
            .min(affordable);
        let max_priority = ceil_percent(priority, 150)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "preview",
                    "Priority fee headroom overflowed.",
                )
            })?
            .min(max);
        let mut transaction = FrozenTransaction {
            chain_id: self.chain_id,
            sender: lower_address(sender),
            to: lower_address(to),
            value: quantity(value),
            calldata: hex_bytes(&calldata),
            nonce,
            gas,
            max_fee_per_gas: quantity(max),
            max_priority_fee_per_gas: quantity(max_priority),
            rollup_fee: None,
        };
        transaction.consensus()?;
        let (rollup_fee, balance) = tokio::try_join!(
            self.rollup_fee_bound(&transaction),
            self.balance(sender, None),
        )?;
        transaction.rollup_fee = rollup_fee;
        let extras = fees::bound_cost(&transaction)?;
        let execution_budget = maximum_total_fee.checked_sub(extras).ok_or_else(|| {
            error(
                "FEE_LIMIT",
                "preview",
                "The total bound cannot fund rollup data and operator fees.",
            )
        })?;
        let max = max.min(execution_budget / U256::from(gas.max(1)));
        if max < required {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "The total fee bound cannot fund execution and rollup fees.",
            ));
        }
        transaction.max_fee_per_gas = quantity(max);
        // Padded tips must leave room for the observed base fee when the
        // caller's total budget clips fee headroom. Existing frozen envelopes
        // retain their exact bid and are never silently repriced.
        let observed_base = (required - priority) / U256::from(2);
        transaction.max_priority_fee_per_gas = quantity(max_priority.min(max - observed_base));
        transaction.consensus()?;
        let cost = transaction.maximum_cost()?;
        if cost > maximum_total_fee {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "Current conservative gas cost exceeds the explicit maximum fee.",
            ));
        }
        if balance
            < value.checked_add(cost).ok_or_else(|| {
                error("U256_OVERFLOW", "preview", "Gross debit overflows uint256.")
            })?
        {
            return Err(error(
                "INSUFFICIENT_GAS_BALANCE",
                "preview",
                "Sender cannot fund exact value and the maximum gas cost.",
            ));
        }
        Ok(PreparedCall {
            transaction,
            simulation,
        })
    }

    /// Freeze a finite transaction envelope around a typed permit without
    /// signing or simulating its signature-dependent calldata. `gas_limit` is
    /// supplied by the trusted adapter as a reviewed worst-case bound. The
    /// signature slot must be exactly 65 zero bytes.
    pub async fn prepare_permit_template(
        &self,
        request: UnsignedPermitTransaction,
        signature_offset: usize,
        permit: AuthorizedPermit,
    ) -> Result<PreparedPermitTemplate> {
        self.prepare_permit_template_with_layout(
            request,
            SignatureLayout::Packed65 {
                offset: signature_offset,
            },
            permit,
        )
        .await
    }

    pub async fn prepare_permit_template_with_layout(
        &self,
        request: UnsignedPermitTransaction,
        signature_layout: SignatureLayout,
        permit: AuthorizedPermit,
    ) -> Result<PreparedPermitTemplate> {
        let UnsignedPermitTransaction {
            sender,
            to,
            value,
            calldata: calldata_template,
            gas_limit,
            maximum_total_fee,
        } = request;
        if gas_limit == 0 {
            return Err(error(
                "INVALID_PERMIT_TEMPLATE",
                "preview",
                "Permit template requires a finite gas limit.",
            ));
        }
        signature_layout.validate_zero_template(&calldata_template)?;
        if permit.owner()? != sender {
            return Err(error(
                "SIGNER_MISMATCH",
                "preview",
                "Permit owner differs from the transaction sender.",
            ));
        }
        self.assert_chain().await?;
        let block = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        permit.validate(self.chain_id, rpc_u64(&block["timestamp"])?)?;
        let (required, priority) = self.fees().await?;
        let affordable = maximum_total_fee / U256::from(gas_limit);
        if affordable < required {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "Current required fee exceeds the explicit total fee bound.",
            ));
        }
        let max_fee = ceil_percent(required, 150)
            .ok_or_else(|| error("U256_OVERFLOW", "preview", "Fee headroom overflowed."))?
            .min(affordable);
        let max_priority = ceil_percent(priority, 150)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "preview",
                    "Priority fee headroom overflowed.",
                )
            })?
            .min(max_fee);
        let mut transaction = FrozenTransaction {
            chain_id: self.chain_id,
            sender: lower_address(sender),
            to: lower_address(to),
            value: quantity(value),
            calldata: hex_bytes(&calldata_template),
            nonce: self.pending_nonce(sender).await?,
            gas: gas_limit,
            max_fee_per_gas: quantity(max_fee),
            max_priority_fee_per_gas: quantity(max_priority),
            rollup_fee: None,
        };
        // Signature replacement keeps calldata length fixed. The OP oracle's
        // size-based upper bound therefore reserves its eventual signed cost.
        transaction.rollup_fee = self.rollup_fee_bound(&transaction).await?;
        let execution_budget = maximum_total_fee
            .checked_sub(fees::bound_cost(&transaction)?)
            .ok_or_else(|| {
                error(
                    "FEE_LIMIT",
                    "preview",
                    "The permit fee ceiling cannot fund rollup fees.",
                )
            })?;
        let affordable = execution_budget / U256::from(gas_limit);
        if affordable < required {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "The permit fee ceiling cannot fund execution and rollup fees.",
            ));
        }
        transaction.max_fee_per_gas = quantity(max_fee.min(affordable));
        let observed_base = (required - priority) / U256::from(2);
        transaction.max_priority_fee_per_gas =
            quantity(max_priority.min(max_fee.min(affordable) - observed_base));
        transaction.consensus()?;
        let cost = transaction.maximum_cost()?;
        if cost > maximum_total_fee
            || self.balance(sender, None).await?
                < value.checked_add(cost).ok_or_else(|| {
                    error("U256_OVERFLOW", "preview", "Gross debit overflows uint256.")
                })?
        {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "Frozen permit transaction exceeds its explicit fee or balance bound.",
            ));
        }
        Ok(PreparedPermitTemplate {
            transaction,
            permit,
            signature_layout,
        })
    }

    /// Revalidate a permit template created during preview without rebuilding
    /// its pending nonce, gas limit, or fee bid. This deliberately avoids
    /// simulating unsigned signature-dependent calldata; materialization runs
    /// the exact signed transaction through normal simulation before submit.
    pub async fn revalidate_permit_template(
        &self,
        template: &PreparedPermitTemplate,
        request: UnsignedPermitTransaction,
        signature_layout: SignatureLayout,
        permit: AuthorizedPermit,
    ) -> Result<PreparedPermitTemplate> {
        let transaction = &template.transaction;
        if template.permit != permit || template.signature_layout != signature_layout {
            return Err(error(
                "PERMIT_STATE_CHANGED",
                "submission",
                "Fresh typed permit authorization differs from the reviewed permit template.",
            ));
        }
        transaction.consensus()?;
        if transaction.chain_id != self.chain_id
            || transaction.chain_id != permit.chain_id()
            || address(&transaction.sender)? != request.sender
            || address(&transaction.to)? != request.to
            || u256(&transaction.value)? != request.value
            || transaction.gas != request.gas_limit
            || transaction.calldata != hex_bytes(&request.calldata)
            || address(&transaction.sender)? != permit.owner()?
        {
            return Err(error(
                "PERMIT_TRANSACTION_MISMATCH",
                "submission",
                "Fresh unsigned permit transaction differs from its frozen preview envelope.",
            ));
        }
        signature_layout.validate_zero_template(&request.calldata)?;
        self.assert_chain().await?;
        let latest = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        permit.validate(self.chain_id, rpc_u64(&latest["timestamp"])?)?;
        validate_frozen_fee_bid(transaction, rpc_u256(&latest["baseFeePerGas"])?)?;
        self.revalidate_rollup_fee(transaction).await?;
        if self.pending_nonce(request.sender).await? != transaction.nonce {
            return Err(error(
                "NONCE_CHANGED",
                "submission",
                "Pending nonce differs from the reviewed frozen permit envelope.",
            ));
        }
        let maximum_cost = transaction.maximum_cost()?;
        if maximum_cost > request.maximum_total_fee {
            return Err(error(
                "FEE_LIMIT",
                "submission",
                "Frozen permit gas ceiling exceeds the reviewed total fee bound.",
            ));
        }
        let debit = request.value.checked_add(maximum_cost).ok_or_else(|| {
            error(
                "U256_OVERFLOW",
                "submission",
                "Gross debit overflows uint256.",
            )
        })?;
        if self.balance(request.sender, None).await? < debit {
            return Err(error(
                "INSUFFICIENT_GAS_BALANCE",
                "submission",
                "Sender cannot fund the reviewed frozen permit value and gas ceiling.",
            ));
        }
        Ok(template.clone())
    }

    /// Obtain the typed signature only after the operation journal authorizes
    /// the write, replace the one frozen slot, and simulate/revalidate the exact
    /// resulting transaction before normal journaled submission.
    pub async fn materialize_permit_template(
        &self,
        template: &PreparedPermitTemplate,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<PreparedCall> {
        journal.assert_authorized()?;
        let mut transaction = template.transaction.clone();
        transaction.consensus()?;
        if transaction.chain_id != self.chain_id
            || template.permit.chain_id() != self.chain_id
            || address(&transaction.sender)? != template.permit.owner()?
        {
            return Err(error(
                "INVALID_PERMIT_TEMPLATE",
                "submission",
                "Restored permit template does not match this chain and owner.",
            ));
        }
        self.assert_chain().await?;
        let latest = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        template
            .permit
            .validate(self.chain_id, rpc_u64(&latest["timestamp"])?)?;
        let mut calldata = rpc_bytes(&json!(transaction.calldata))?.to_vec();
        template
            .signature_layout
            .validate_zero_template(&calldata)?;
        let signature = self
            .sign_authorized_permit(&template.permit, journal)
            .await?;
        template
            .signature_layout
            .materialize(&mut calldata, &signature)?;
        transaction.calldata = hex_bytes(&calldata);
        validate_materialized_permit_template(template, &transaction)?;
        let simulation = self.revalidate(&transaction).await?;
        Ok(PreparedCall {
            transaction,
            simulation,
        })
    }
    pub async fn revalidate(&self, transaction: &FrozenTransaction) -> Result<Bytes> {
        transaction.consensus()?;
        if transaction.chain_id != self.chain_id {
            return Err(error(
                "WRONG_CHAIN",
                "submission",
                "Frozen envelope differs from the configured immutable chain identity.",
            ));
        }
        self.assert_chain().await?;
        let request = transaction.rpc_call();
        let sender = address(&transaction.sender)?;
        let (latest, nonce, (), simulation, estimate, balance) = tokio::try_join!(
            self.rpc
                .request("eth_getBlockByNumber", json!(["latest", false])),
            self.pending_nonce(sender),
            self.revalidate_rollup_fee(transaction),
            async {
                self.rpc
                    .request("eth_call", json!([request.clone(), "latest"]))
                    .await
                    .and_then(|value| rpc_bytes(&value))
            },
            async {
                self.rpc
                    .request("eth_estimateGas", json!([request.clone()]))
                    .await
                    .and_then(|value| rpc_u64(&value))
            },
            self.balance(sender, None),
        )?;
        validate_frozen_fee_bid(transaction, rpc_u256(&latest["baseFeePerGas"])?)?;
        if nonce != transaction.nonce {
            return Err(error(
                "NONCE_CHANGED",
                "submission",
                "Pending nonce differs from the authorized frozen envelope.",
            ));
        }
        if estimate > transaction.gas {
            return Err(error(
                "GAS_BOUND_CHANGED",
                "submission",
                "Current gas estimate exceeds the approved gas limit.",
            ));
        }
        let debit = u256(&transaction.value)?
            .checked_add(transaction.maximum_cost()?)
            .ok_or_else(|| {
                error(
                    "U256_OVERFLOW",
                    "submission",
                    "Gross debit overflows uint256.",
                )
            })?;
        if balance < debit {
            return Err(error(
                "INSUFFICIENT_GAS_BALANCE",
                "submission",
                "Sender cannot fund the approved maximum debit.",
            ));
        }
        Ok(simulation)
    }
    pub async fn submit(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.revalidate(transaction).await?;
        self.dispatch(transaction, metadata, journal, None).await
    }

    /// Submit the exact envelope already checked by the adapter's final
    /// revalidation. Only permit signature slots may differ, after their exact
    /// materialized transaction has passed simulation.
    pub async fn submit_revalidated(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
    ) -> Result<Submission> {
        self.submit_at_revalidated_binding(transaction, metadata, journal, validated, "")
            .await
    }

    /// Submit a composite's exact nested envelope while retaining the complete
    /// outer candidate's freshness and journal authority through dispatch.
    /// The composite adapter must have revalidated this nested execution binding.
    pub async fn submit_nested_revalidated(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
        binding_pointer: &str,
    ) -> Result<Submission> {
        if !binding_pointer.starts_with('/') {
            return Err(GoFinError::validation(
                "A nested execution binding is required.",
            ));
        }
        self.submit_at_revalidated_binding(
            transaction,
            metadata,
            journal,
            validated,
            binding_pointer,
        )
        .await
    }

    async fn submit_at_revalidated_binding(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
        binding_pointer: &str,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        let binding = validated
            .prepared()
            .binding
            .pointer(binding_pointer)
            .ok_or_else(|| {
                error(
                    "BINDING_CHANGED",
                    "submission",
                    "The final candidate has no selected execution binding.",
                )
            })?;
        if binding.get("permit_template").is_some_and(|v| !v.is_null()) {
            return Err(error(
                "PERMIT_MATERIALIZATION_REQUIRED",
                "submission",
                "A permit template must be materialized and simulated through its dedicated submission path.",
            ));
        }
        if binding.get("transaction")
            != Some(
                &serde_json::to_value(transaction)
                    .map_err(|_| GoFinError::validation("Validated transaction is malformed."))?,
            )
        {
            return Err(error(
                "BINDING_CHANGED",
                "submission",
                "The transaction differs from the final revalidated candidate.",
            ));
        }
        self.submit_validated_envelope(transaction, metadata, journal, validated)
            .await
    }

    pub async fn submit_permit_template_revalidated(
        &self,
        template: &PreparedPermitTemplate,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
    ) -> Result<Submission> {
        validated.assert_fresh()?;
        if validated.prepared().binding.get("permit_template")
            != Some(
                &serde_json::to_value(template).map_err(|_| {
                    GoFinError::validation("Validated permit template is malformed.")
                })?,
            )
        {
            return Err(error(
                "BINDING_CHANGED",
                "submission",
                "The permit template differs from the final revalidated candidate.",
            ));
        }
        if validated.prepared().binding.get("transaction")
            != Some(&serde_json::to_value(template.transaction()).map_err(|_| {
                GoFinError::validation("Validated permit transaction is malformed.")
            })?)
        {
            return Err(error(
                "BINDING_CHANGED",
                "submission",
                "The permit template differs from the validated frozen envelope.",
            ));
        }
        let concrete = self
            .materialize_permit_template(template, journal.clone())
            .await?;
        self.submit_validated_envelope(&concrete.transaction, metadata, journal, validated)
            .await
    }

    async fn submit_validated_envelope(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: &RevalidatedOperation,
    ) -> Result<Submission> {
        transaction.consensus()?;
        if transaction.chain_id != self.chain_id {
            return Err(error(
                "WRONG_CHAIN",
                "submission",
                "Revalidated transaction belongs to a different runtime chain.",
            ));
        }
        // Nonce can change during intervening pricing/journal work. This cheap
        // read remains immediately before signing; simulations and estimates
        // already belong to the single adapter revalidation above.
        if self.pending_nonce(address(&transaction.sender)?).await? != transaction.nonce {
            return Err(error(
                "NONCE_CHANGED",
                "submission",
                "Pending nonce differs from the revalidated envelope.",
            ));
        }
        self.dispatch(transaction, metadata, journal, Some(validated))
            .await
    }

    async fn dispatch(
        &self,
        transaction: &FrozenTransaction,
        metadata: Value,
        journal: Arc<dyn SubmissionJournal>,
        validated: Option<&RevalidatedOperation>,
    ) -> Result<Submission> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            error(
                "PROTECTED_SIGNER_REQUIRED",
                "signing",
                "No protected signing provider is configured.",
            )
        })?;
        if let Some(validated) = validated {
            validated.assert_fresh()?;
        }
        journal.assert_authorized()?;
        let raw = signer.sign_prepared(transaction).await?;
        let tx_hash = verify_signed_envelope(transaction, &raw)?;
        let submission = Submission {
            kind: "ethereum".into(),
            reference: format!("{tx_hash:#x}"),
            data: json!({"transaction":transaction,"metadata":metadata}),
        };
        if let Some(validated) = validated {
            validated.assert_fresh()?;
        }
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        let returned = self
            .rpc
            .request("eth_sendRawTransaction", json!([hex_bytes(&raw)]))
            .await?;
        if hash(returned.as_str().unwrap_or(""))? != tx_hash {
            return Err(error(
                "RPC_HASH_MISMATCH",
                "submission",
                "RPC hash differs from the original journaled envelope hash.",
            ));
        }
        Ok(submission)
    }

    /// Sign one broker-reviewed typed permit. The fresh journal authorization
    /// is checked immediately before the protected signer and again afterward.
    /// The signature remains an internal adapter result and is never persisted
    /// or exposed by this crate's CLI-facing surface.
    pub async fn sign_authorized_permit(
        &self,
        permit: &AuthorizedPermit,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Bytes> {
        journal.assert_authorized()?;
        if permit.chain_id() != self.chain_id {
            return Err(permit_error(
                "Permit chain differs from the configured wallet chain.",
            ));
        }
        self.assert_chain().await?;
        let latest = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let now = rpc_u64(&latest["timestamp"])?;
        permit.validate(self.chain_id, now)?;
        let signer = self.signer.as_ref().ok_or_else(|| {
            error(
                "PROTECTED_SIGNER_REQUIRED",
                "signing",
                "No protected signing provider is configured.",
            )
        })?;
        let signature = signer.sign_authorized_permit(permit).await?;
        verify_authorized_permit_signature(permit, &signature)?;
        journal.assert_authorized()?;
        // Recheck time and chain after potentially slow hardware signing so an
        // expired or cross-chain result is never released to the caller.
        self.assert_chain().await?;
        let latest = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        permit
            .validate(self.chain_id, rpc_u64(&latest["timestamp"])?)
            .map_err(|_| permit_error("Permit expired while protected signing was in progress."))?;
        Ok(signature)
    }
    pub async fn receipt(
        &self,
        submission: &Submission,
        transaction: &FrozenTransaction,
        required: u64,
    ) -> Result<Option<CanonicalReceipt>> {
        if required == 0 || submission.kind != "ethereum" {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "Receipt verification requires the original Ethereum identity and nonzero confirmations.",
            ));
        }
        self.assert_chain().await?;
        let expected = hash(&submission.reference)?;
        let receipt = self
            .rpc
            .request("eth_getTransactionReceipt", json!([submission.reference]))
            .await?;
        if receipt.is_null() {
            return Ok(None);
        }
        let reported = self
            .rpc
            .request("eth_getTransactionByHash", json!([submission.reference]))
            .await?;
        if reported.is_null() {
            return Ok(None);
        }
        let read_hash = |value: &Value| hash(value.as_str().unwrap_or(""));
        if read_hash(&receipt["transactionHash"])? != expected
            || read_hash(&reported["hash"])? != expected
            || address(reported["from"].as_str().unwrap_or(""))? != address(&transaction.sender)?
            || address(reported["to"].as_str().unwrap_or(""))? != address(&transaction.to)?
            || rpc_u64(&reported["chainId"])? != transaction.chain_id
            || rpc_u64(&reported["type"])? != 2
            || rpc_u64(&reported["nonce"])? != transaction.nonce
            || rpc_u64(&reported["gas"])? != transaction.gas
            || rpc_u256(&reported["value"])? != u256(&transaction.value)?
            || rpc_bytes(&reported["input"])? != rpc_bytes(&json!(transaction.calldata))?
            || rpc_u256(&reported["maxFeePerGas"])? != u256(&transaction.max_fee_per_gas)?
            || rpc_u256(&reported["maxPriorityFeePerGas"])?
                != u256(&transaction.max_priority_fee_per_gas)?
            || reported["accessList"]
                .as_array()
                .is_none_or(|v| !v.is_empty())
        {
            return Err(error(
                "RECEIPT_TRANSACTION_MISMATCH",
                "reconciliation",
                "RPC transaction fields differ from the original approved envelope.",
            ));
        }
        if receipt_inclusion_pending(&receipt, &reported)? {
            return Ok(None);
        }
        let block_number = rpc_u64(&receipt["blockNumber"])?;
        let block_hash = read_hash(&receipt["blockHash"])?;
        if read_hash(&reported["blockHash"])? != block_hash
            || rpc_u64(&reported["blockNumber"])? != block_number
        {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "Receipt and transaction inclusion differ.",
            ));
        }
        let block = self
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("{block_number:#x}"), false]),
            )
            .await?;
        if block.is_null()
            || read_hash(&block["hash"])? != block_hash
            || rpc_u64(&block["number"])? != block_number
        {
            return Ok(None);
        }
        let minimum = block_number.checked_add(required - 1).ok_or_else(|| {
            error(
                "RPC_DATA_INVALID",
                "reconciliation",
                "Required confirmation height overflows.",
            )
        })?;
        let latest = match self
            .rpc
            .request_checked(
                "eth_blockNumber",
                json!([]),
                RpcReadRequirements {
                    minimum_block: Some(minimum),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(value) => rpc_u64(&value)?,
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "RPC_EVIDENCE_UNAVAILABLE" | "RPC_STALE_READ" | "RPC_UNSUPPORTED_CAPABILITY"
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let confirmations = if latest >= block_number {
            latest - block_number + 1
        } else {
            0
        };
        if confirmations < required {
            return Ok(None);
        }
        let status = rpc_u64(&receipt["status"])?;
        if status > 1 {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "Receipt status is not a canonical success/revert value.",
            ));
        }
        let gas = rpc_u64(&receipt["gasUsed"])?;
        let price = rpc_u256(&receipt["effectiveGasPrice"])?;
        if gas > transaction.gas || price > u256(&transaction.max_fee_per_gas)? {
            return Err(error(
                "RECEIPT_FEE_MISMATCH",
                "reconciliation",
                "Actual execution gas/price exceeds the approved envelope.",
            ));
        }
        let fee = self
            .receipt_total_fee(&receipt, &reported, &block, gas, price)
            .await?;
        Ok(Some(CanonicalReceipt {
            hash: expected,
            transaction_sender: transaction.sender.clone(),
            block_hash,
            block_number,
            confirmations,
            success: status == 1,
            fee,
            receipt,
        }))
    }

    /// Observe a transaction candidate reported by an external protocol. This
    /// proves only canonical inclusion and configured finality; callers must
    /// independently verify the receipt logs and protocol correlation before
    /// treating it as a financial effect.
    pub async fn observe_receipt(
        &self,
        transaction_hash: &str,
        required: u64,
        require_finalized: bool,
    ) -> Result<Option<CanonicalReceipt>> {
        if required == 0 {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "Receipt observation requires a nonzero confirmation rule.",
            ));
        }
        self.assert_chain().await?;
        let expected = hash(transaction_hash)?;
        let receipt = self
            .rpc
            .request("eth_getTransactionReceipt", json!([transaction_hash]))
            .await?;
        if receipt.is_null() {
            return Ok(None);
        }
        let reported = self
            .rpc
            .request("eth_getTransactionByHash", json!([transaction_hash]))
            .await?;
        if reported.is_null() {
            return Ok(None);
        }
        if hash(receipt["transactionHash"].as_str().unwrap_or(""))? != expected
            || hash(reported["hash"].as_str().unwrap_or(""))? != expected
        {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "RPC receipt and transaction do not match the reported candidate hash.",
            ));
        }
        if receipt_inclusion_pending(&receipt, &reported)? {
            return Ok(None);
        }
        let block_number = rpc_u64(&receipt["blockNumber"])?;
        let block_hash = hash(receipt["blockHash"].as_str().unwrap_or(""))?;
        if hash(reported["blockHash"].as_str().unwrap_or(""))? != block_hash
            || rpc_u64(&reported["blockNumber"])? != block_number
        {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "Receipt and transaction inclusion differ.",
            ));
        }
        let block = self
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("{block_number:#x}"), false]),
            )
            .await?;
        if block.is_null()
            || hash(block["hash"].as_str().unwrap_or(""))? != block_hash
            || rpc_u64(&block["number"])? != block_number
        {
            return Ok(None);
        }
        let minimum = block_number.checked_add(required - 1).ok_or_else(|| {
            error(
                "RPC_DATA_INVALID",
                "reconciliation",
                "Required confirmation height overflows.",
            )
        })?;
        // A finalized head at the required height proves both confirmations
        // and finality. A separate latest-head read can pin a volatile fork
        // even while this historical receipt remains canonical.
        let head_method = if require_finalized {
            "eth_getBlockByNumber"
        } else {
            "eth_blockNumber"
        };
        let head_params = if require_finalized {
            json!(["finalized", false])
        } else {
            json!([])
        };
        let head = match self
            .rpc
            .request_checked(
                head_method,
                head_params,
                RpcReadRequirements {
                    minimum_block: Some(minimum),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(value) if require_finalized && value.is_null() => return Ok(None),
            Ok(value) if require_finalized => rpc_u64(&value["number"])?,
            Ok(value) => rpc_u64(&value)?,
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "RPC_EVIDENCE_UNAVAILABLE" | "RPC_STALE_READ" | "RPC_UNSUPPORTED_CAPABILITY"
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let confirmations = if head >= block_number {
            head - block_number + 1
        } else {
            0
        };
        if confirmations < required {
            return Ok(None);
        }
        if require_finalized {
            // Recheck the candidate block after observing the finalized head;
            // the finalized head's hash is expected to differ from this block.
            let canonical = self
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([format!("{block_number:#x}"), false]),
                )
                .await?;
            if canonical.is_null() || hash(canonical["hash"].as_str().unwrap_or(""))? != block_hash
            {
                return Ok(None);
            }
        }
        let status = rpc_u64(&receipt["status"])?;
        if status > 1 {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "Receipt status is not a canonical success/revert value.",
            ));
        }
        let gas = rpc_u64(&receipt["gasUsed"])?;
        let price = rpc_u256(&receipt["effectiveGasPrice"])?;
        let fee = self
            .receipt_total_fee(&receipt, &reported, &block, gas, price)
            .await?;
        Ok(Some(CanonicalReceipt {
            hash: expected,
            transaction_sender: lower_address(address(reported["from"].as_str().unwrap_or(""))?),
            block_hash,
            block_number,
            confirmations,
            success: status == 1,
            fee,
            receipt,
        }))
    }
}

/// Prove that a concrete submitted envelope is precisely the reviewed permit
/// template with only its declared signature slots materialized.
pub fn validate_materialized_permit_template(
    template: &PreparedPermitTemplate,
    concrete: &FrozenTransaction,
) -> Result<()> {
    let expected = &template.transaction;
    if concrete.chain_id != expected.chain_id
        || concrete.sender != expected.sender
        || concrete.to != expected.to
        || concrete.value != expected.value
        || concrete.nonce != expected.nonce
        || concrete.gas != expected.gas
        || concrete.max_fee_per_gas != expected.max_fee_per_gas
        || concrete.max_priority_fee_per_gas != expected.max_priority_fee_per_gas
    {
        return Err(error(
            "PERMIT_TRANSACTION_MISMATCH",
            "reconciliation",
            "Concrete permit transaction changed an approved envelope field.",
        ));
    }
    concrete.consensus()?;
    if address(&expected.sender)? != template.permit.owner()?
        || concrete.chain_id != template.permit.chain_id()
    {
        return Err(error(
            "PERMIT_TRANSACTION_MISMATCH",
            "reconciliation",
            "Permit owner or chain differs from its frozen transaction template.",
        ));
    }
    let mut expected_calldata = rpc_bytes(&json!(expected.calldata))?.to_vec();
    template
        .signature_layout
        .validate_zero_template(&expected_calldata)?;
    let concrete_calldata = rpc_bytes(&json!(concrete.calldata))?;
    let signature = template
        .signature_layout
        .signature_from(&concrete_calldata)?;
    verify_authorized_permit_signature(&template.permit, &signature)?;
    template
        .signature_layout
        .materialize(&mut expected_calldata, &signature)?;
    if concrete_calldata.as_ref() != expected_calldata.as_slice() {
        return Err(error(
            "PERMIT_TRANSACTION_MISMATCH",
            "reconciliation",
            "Concrete permit calldata changed bytes outside its typed signature slots.",
        ));
    }
    Ok(())
}

pub fn block_tag(block: Option<u64>) -> String {
    block
        .map(|n| format!("{n:#x}"))
        .unwrap_or_else(|| "latest".into())
}

#[cfg(test)]
mod fee_math_tests {
    use super::ceil_percent;
    use alloy::primitives::U256;

    #[test]
    fn percentage_headroom_uses_checked_integer_arithmetic() {
        assert_eq!(ceil_percent(U256::from(10), 150), Some(U256::from(15)));
        assert_eq!(ceil_percent(U256::from(1), 150), Some(U256::from(2)));
        assert_eq!(ceil_percent(U256::MAX, 150), None);
    }
}

#[cfg(test)]
mod http_rpc_tests {
    use super::*;
    mod rate_limits;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;

    enum FixtureResponse {
        Json(Value),
        RpcError(i64, &'static str),
        Raw(Value),
        Http(u16),
    }

    fn fresh_fixture_header() -> Value {
        json!({"number":"0x11","hash":format!("0x{}","33".repeat(32)),"timestamp":format!("0x{:x}",SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs())})
    }

    #[test]
    fn isolated_rpc_pool_validates_every_endpoint_and_keeps_production_strict() {
        let local = "http://127.0.0.1:1234".to_owned();
        let backup = "http://localhost:5678".to_owned();
        assert!(HttpRpc::isolated_localhost_pool(vec![local.clone(), backup], 1).is_ok());
        assert!(HttpRpc::new(vec![local.clone()], 1).is_err());
        assert!(HttpRpc::isolated_localhost_pool(vec![], 1).is_err());
        assert!(HttpRpc::isolated_localhost_pool(vec![local.clone()], 0).is_err());
        for invalid in [
            "http://example.com",
            "https://127.0.0.1:5678",
            "http://user:secret@127.0.0.1:5678",
            "http://127.0.0.1:5678?key=secret",
            "http://127.0.0.1:5678#fragment",
        ] {
            assert!(
                HttpRpc::isolated_localhost_pool(vec![local.clone(), invalid.into()], 1).is_err(),
                "invalid backup must be rejected: {invalid}"
            );
        }
    }

    fn fixture_rpc(responses: Vec<FixtureResponse>) -> (HttpRpc, JoinHandle<Vec<String>>) {
        fixture_rpc_with_gate(responses, None, None)
    }

    fn fixture_rpc_with_gate(
        responses: Vec<FixtureResponse>,
        before_first_response: Option<Arc<std::sync::atomic::AtomicBool>>,
        after_last_response: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> (HttpRpc, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let body = read_request(&mut stream);
                let request: Value = serde_json::from_slice(&body).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
                if methods.len() == 1
                    && let Some(gate) = &before_first_response
                {
                    wait_for_fixture_gate(gate);
                }
                match response {
                    FixtureResponse::Json(result) => {
                        let payload = json!({
                            "jsonrpc":"2.0",
                            "id":request["id"],
                            "result":result
                        })
                        .to_string();
                        write_response(&mut stream, 200, payload.as_bytes());
                    }
                    FixtureResponse::RpcError(code, message) => {
                        let payload = json!({
                            "jsonrpc":"2.0",
                            "id":request["id"],
                            "error":{"code":code,"message":message}
                        })
                        .to_string();
                        write_response(&mut stream, 200, payload.as_bytes());
                    }
                    FixtureResponse::Raw(payload) => {
                        write_response(&mut stream, 200, payload.to_string().as_bytes());
                    }
                    FixtureResponse::Http(status) => write_response(&mut stream, status, b""),
                }
            }
            if let Some(gate) = after_last_response {
                gate.store(true, std::sync::atomic::Ordering::Release);
            }
            methods
        });
        let rpc = HttpRpc::isolated_localhost(format!("http://{address}"), 1).unwrap();
        (rpc, worker)
    }

    fn wait_for_fixture_gate(gate: &std::sync::atomic::AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !gate.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "RPC fixture gate was not released"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        read_request_or_cancelled(stream).expect("fixture client did not send a full request")
    }

    fn read_request_or_cancelled(stream: &mut TcpStream) -> Option<Vec<u8>> {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let count = match stream.read(&mut chunk) {
                Ok(0) => return None,
                Ok(count) => count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return None;
                }
                Err(error) => panic!("fixture request failed: {error}"),
            };
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let body_start = header_end + 4;
                if bytes.len() >= body_start + content_length {
                    return Some(bytes[body_start..body_start + content_length].to_vec());
                }
            }
        }
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Unavailable" };
        let sent = (|| -> std::io::Result<()> {
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(body)?;
            stream.flush()
        })();
        if let Err(error) = sent {
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ));
        }
    }

    #[tokio::test]
    async fn observer_receipt_proves_only_canonical_confirmed_inclusion() {
        let tx_hash = format!("0x{}", "11".repeat(32));
        let block_hash = format!("0x{}", "22".repeat(32));
        let receipt = json!({
            "transactionHash":tx_hash,
            "blockHash":block_hash,
            "blockNumber":"0x10",
            "status":"0x1",
            "gasUsed":"0x5208",
            "effectiveGasPrice":"0x2",
            "logs":[{"address":"0x3333333333333333333333333333333333333333"}]
        });
        let transaction = json!({"hash":tx_hash,"blockHash":block_hash,"blockNumber":"0x10","from":"0x4444444444444444444444444444444444444444"});
        let block = json!({"hash":block_hash,"number":"0x10"});
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(receipt.clone()),
            FixtureResponse::Json(transaction),
            FixtureResponse::Json(block),
            FixtureResponse::Json(json!("0x11")),
            FixtureResponse::Json(
                json!({"number":"0x11","hash":format!("0x{}","33".repeat(32)),"timestamp":format!("0x{:x}",SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs())}),
            ),
        ]);
        let runtime = EvmRuntime::new(Arc::new(rpc), 1, None);
        let observed = runtime
            .observe_receipt(&tx_hash, 2, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.hash, hash(&tx_hash).unwrap());
        assert_eq!(
            observed.transaction_sender,
            "0x4444444444444444444444444444444444444444"
        );
        assert_eq!(observed.block_number, 16);
        assert_eq!(observed.confirmations, 2);
        assert!(observed.success);
        assert_eq!(observed.fee, U256::from(0x5208 * 2));
        assert_eq!(
            observed.receipt["logs"][0]["address"],
            "0x3333333333333333333333333333333333333333"
        );
        assert_eq!(
            worker.join().unwrap(),
            vec![
                "eth_chainId",
                "eth_chainId",
                "eth_getTransactionReceipt",
                "eth_getTransactionByHash",
                "eth_getBlockByNumber",
                "eth_blockNumber",
                "eth_getBlockByNumber"
            ]
        );
    }

    #[tokio::test]
    async fn finalized_observer_receipt_uses_finalized_head_without_volatile_latest_read() {
        let tx_hash = format!("0x{}", "11".repeat(32));
        let block_hash = format!("0x{}", "22".repeat(32));
        let receipt = json!({
            "transactionHash":tx_hash,
            "blockHash":block_hash,
            "blockNumber":"0x10",
            "status":"0x1",
            "gasUsed":"0x5208",
            "effectiveGasPrice":"0x2",
            "logs":[]
        });
        let transaction = json!({
            "hash":tx_hash,
            "blockHash":block_hash,
            "blockNumber":"0x10",
            "from":"0x4444444444444444444444444444444444444444"
        });
        let block = json!({"hash":block_hash,"number":"0x10"});
        let finalized = json!({"hash":format!("0x{}", "33".repeat(32)),"number":"0x11"});
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(receipt),
            FixtureResponse::Json(transaction),
            FixtureResponse::Json(block.clone()),
            FixtureResponse::Json(finalized),
            FixtureResponse::Json(block),
        ]);
        let runtime = EvmRuntime::new(Arc::new(rpc), 1, None);
        let observed = runtime
            .observe_receipt(&tx_hash, 2, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.confirmations, 2);
        assert_eq!(observed.block_hash, hash(&block_hash).unwrap());
        assert_eq!(
            worker.join().unwrap(),
            vec![
                "eth_chainId",
                "eth_chainId",
                "eth_getTransactionReceipt",
                "eth_getTransactionByHash",
                "eth_getBlockByNumber",
                "eth_getBlockByNumber",
                "eth_getBlockByNumber"
            ]
        );
    }

    #[tokio::test]
    async fn receipt_with_verified_transaction_but_null_inclusion_is_pending() {
        let tx_hash = format!("0x{}", "11".repeat(32));
        let sender = "0x4444444444444444444444444444444444444444";
        let to = "0x5555555555555555555555555555555555555555";
        let receipt = json!({
            "transactionHash":tx_hash,
            "blockHash":null,
            "blockNumber":null
        });
        let reported = json!({
            "hash":tx_hash,
            "blockHash":null,
            "blockNumber":null,
            "from":sender,
            "to":to,
            "chainId":"0x1",
            "type":"0x2",
            "nonce":"0x0",
            "gas":"0x5208",
            "value":"0x0",
            "input":"0x",
            "maxFeePerGas":"0x2",
            "maxPriorityFeePerGas":"0x1",
            "accessList":[]
        });
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(receipt),
            FixtureResponse::Json(reported),
        ]);
        let runtime = EvmRuntime::new(Arc::new(rpc), 1, None);
        let transaction = FrozenTransaction {
            chain_id: 1,
            sender: sender.into(),
            to: to.into(),
            value: Quantity::zero(),
            calldata: "0x".into(),
            nonce: 0,
            gas: 21_000,
            max_fee_per_gas: Quantity::new("2").unwrap(),
            max_priority_fee_per_gas: Quantity::new("1").unwrap(),
            rollup_fee: None,
        };
        let submission = Submission {
            kind: "ethereum".into(),
            reference: tx_hash,
            data: json!({}),
        };

        assert!(
            runtime
                .receipt(&submission, &transaction, 1)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            worker.join().unwrap(),
            vec![
                "eth_chainId",
                "eth_chainId",
                "eth_getTransactionReceipt",
                "eth_getTransactionByHash"
            ]
        );
    }

    #[tokio::test]
    async fn observer_receipt_with_null_inclusion_is_pending_but_malformed_hash_fails() {
        let tx_hash = format!("0x{}", "11".repeat(32));
        let mut receipt = json!({
            "transactionHash":tx_hash,
            "blockHash":null,
            "blockNumber":"0x10"
        });
        let mut transaction = json!({
            "hash":tx_hash,
            "blockHash":null,
            "blockNumber":"0x10",
            "from":"0x4444444444444444444444444444444444444444"
        });
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(receipt.clone()),
            FixtureResponse::Json(transaction.clone()),
        ]);
        let runtime = EvmRuntime::new(Arc::new(rpc), 1, None);
        assert!(
            runtime
                .observe_receipt(&tx_hash, 1, false)
                .await
                .unwrap()
                .is_none()
        );
        worker.join().unwrap();

        receipt["blockHash"] = json!("0xnot-a-hash");
        transaction["blockHash"] = json!("0xnot-a-hash");
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(receipt.clone()),
            FixtureResponse::Json(transaction.clone()),
        ]);
        let runtime = EvmRuntime::new(Arc::new(rpc), 1, None);
        let error = runtime
            .observe_receipt(&tx_hash, 1, false)
            .await
            .unwrap_err();
        assert_eq!(error.code, "INVALID_HASH");
        worker.join().unwrap();

        receipt["blockHash"] = json!("0xnot-a-hash");
        transaction["blockHash"] = Value::Null;
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(receipt),
            FixtureResponse::Json(transaction),
        ]);
        let runtime = EvmRuntime::new(Arc::new(rpc), 1, None);
        let error = runtime
            .observe_receipt(&tx_hash, 1, false)
            .await
            .unwrap_err();
        assert_eq!(error.code, "INVALID_HASH");
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn null_inclusion_does_not_hide_original_envelope_mismatch() {
        let tx_hash = format!("0x{}", "11".repeat(32));
        let receipt = json!({
            "transactionHash":tx_hash,
            "blockHash":null,
            "blockNumber":null
        });
        let reported = json!({
            "hash":tx_hash,
            "blockHash":null,
            "blockNumber":null,
            "from":"0x6666666666666666666666666666666666666666",
            "to":"0x5555555555555555555555555555555555555555",
            "chainId":"0x1",
            "type":"0x2",
            "nonce":"0x0",
            "gas":"0x5208",
            "value":"0x0",
            "input":"0x",
            "maxFeePerGas":"0x2",
            "maxPriorityFeePerGas":"0x1",
            "accessList":[]
        });
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Json(receipt),
            FixtureResponse::Json(reported),
        ]);
        let runtime = EvmRuntime::new(Arc::new(rpc), 1, None);
        let transaction = FrozenTransaction {
            chain_id: 1,
            sender: "0x4444444444444444444444444444444444444444".into(),
            to: "0x5555555555555555555555555555555555555555".into(),
            value: Quantity::zero(),
            calldata: "0x".into(),
            nonce: 0,
            gas: 21_000,
            max_fee_per_gas: Quantity::new("2").unwrap(),
            max_priority_fee_per_gas: Quantity::new("1").unwrap(),
            rollup_fee: None,
        };
        let submission = Submission {
            kind: "ethereum".into(),
            reference: tx_hash,
            data: json!({}),
        };

        let error = runtime
            .receipt(&submission, &transaction, 1)
            .await
            .unwrap_err();
        assert_eq!(error.code, "RECEIPT_TRANSACTION_MISMATCH");
        worker.join().unwrap();
    }

    fn two_endpoint_rpc(
        first_result: FixtureResponse,
    ) -> (HttpRpc, JoinHandle<Vec<String>>, JoinHandle<Vec<String>>) {
        two_endpoint_rpc_with_chain(first_result, "0x1")
    }

    fn two_endpoint_rpc_with_chain(
        first_result: FixtureResponse,
        chain_id: &'static str,
    ) -> (HttpRpc, JoinHandle<Vec<String>>, JoinHandle<Vec<String>>) {
        two_endpoint_rpc_with_chain_responses(first_result, chain_id, 2)
    }

    fn two_endpoint_rpc_with_chain_responses(
        first_result: FixtureResponse,
        chain_id: &'static str,
        first_response_count: usize,
    ) -> (HttpRpc, JoinHandle<Vec<String>>, JoinHandle<Vec<String>>) {
        let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let second = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let first_address = first.local_addr().unwrap();
        let second_address = second.local_addr().unwrap();
        let first_script_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = first_script_complete.clone();
        let first_worker = std::thread::spawn(move || {
            let mut methods = Vec::new();
            let mut responses = vec![FixtureResponse::Json(json!(chain_id))];
            if first_response_count > 1 {
                responses.push(first_result);
            }
            for response in responses {
                let (mut stream, _) = first.accept().unwrap();
                let request: Value = serde_json::from_slice(&read_request(&mut stream)).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
                match response {
                    FixtureResponse::Json(result) => {
                        let payload =
                            json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                        write_response(&mut stream, 200, payload.as_bytes());
                    }
                    FixtureResponse::RpcError(code, message) => {
                        let payload = json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":code,"message":message}})
                            .to_string();
                        write_response(&mut stream, 200, payload.as_bytes());
                    }
                    FixtureResponse::Raw(payload) => {
                        write_response(&mut stream, 200, payload.to_string().as_bytes());
                    }
                    FixtureResponse::Http(status) => write_response(&mut stream, status, b""),
                }
            }
            completed.store(true, std::sync::atomic::Ordering::Release);
            methods
        });
        let second_worker = std::thread::spawn(move || {
            second.set_nonblocking(true).unwrap();
            let mut methods = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while std::time::Instant::now() < deadline {
                match second.accept() {
                    Ok((mut stream, _)) => {
                        // macOS can inherit the listener's nonblocking mode;
                        // read_request expects a blocking fixture connection.
                        stream.set_nonblocking(false).unwrap();
                        let Some(body) = read_request_or_cancelled(&mut stream) else {
                            continue;
                        };
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        let method = request["method"].as_str().unwrap().to_owned();
                        methods.push(method.clone());
                        if method == "eth_chainId" {
                            // Observe A's scripted error before B can become
                            // a winner, even when fixture threads run out of order.
                            wait_for_fixture_gate(&first_script_complete);
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        let result = if method == "eth_chainId" {
                            json!("0x1")
                        } else if method == "eth_getBlockByNumber" {
                            fresh_fixture_header()
                        } else {
                            json!("0xbeef")
                        };
                        let payload =
                            json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                        write_response(&mut stream, 200, payload.as_bytes());
                    }
                    Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(failure) => panic!("fixture listener failed: {failure}"),
                }
            }
            methods
        });
        let rpc = HttpRpc::isolated_localhost(format!("http://{first_address}"), 1).unwrap();
        let rpc = HttpRpc {
            urls: vec![
                format!("http://{first_address}"),
                format!("http://{second_address}"),
            ],
            expected_chain_id: rpc.expected_chain_id,
            client: rpc.client,
            write_client: rpc.write_client,
            verified: rpc.verified,
            read_cooldowns: rpc.read_cooldowns,
            write_identity_timeout: rpc.write_identity_timeout,
            selection: rpc.selection,
        };
        (rpc, first_worker, second_worker)
    }

    fn historical_state_fallback_rpc(
        message: &'static str,
        params: Value,
    ) -> (HttpRpc, JoinHandle<Vec<String>>, JoinHandle<Vec<String>>) {
        let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let second = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let first_address = first.local_addr().unwrap();
        let second_address = second.local_addr().unwrap();
        let first_params = params.clone();
        let retries_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = retries_complete.clone();
        let first_worker = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = first.accept().unwrap();
                let request: Value = serde_json::from_slice(&read_request(&mut stream)).unwrap();
                let method = request["method"].as_str().unwrap().to_owned();
                methods.push(method.clone());
                let payload = if index == 0 {
                    json!({"jsonrpc":"2.0","id":request["id"],"result":"0x1"})
                } else {
                    assert_eq!(request["params"], first_params);
                    json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":message}})
                }
                .to_string();
                write_response(&mut stream, 200, payload.as_bytes());
            }
            completed.store(true, std::sync::atomic::Ordering::Release);
            methods
        });
        let second_worker = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = second.accept().unwrap();
                let request: Value = serde_json::from_slice(&read_request(&mut stream)).unwrap();
                let method = request["method"].as_str().unwrap().to_owned();
                methods.push(method.clone());
                let result = if method == "eth_chainId" {
                    json!("0x1")
                } else {
                    assert_eq!(request["params"], params);
                    wait_for_fixture_gate(&retries_complete);
                    json!("0x6000")
                };
                let payload =
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                write_response(&mut stream, 200, payload.as_bytes());
            }
            methods
        });
        let rpc = HttpRpc::isolated_localhost(format!("http://{first_address}"), 1).unwrap();
        let rpc = HttpRpc {
            urls: vec![
                format!("http://{first_address}"),
                format!("http://{second_address}"),
            ],
            expected_chain_id: rpc.expected_chain_id,
            client: rpc.client,
            write_client: rpc.write_client,
            verified: rpc.verified,
            read_cooldowns: rpc.read_cooldowns,
            write_identity_timeout: rpc.write_identity_timeout,
            selection: rpc.selection,
        };
        (rpc, first_worker, second_worker)
    }

    #[tokio::test]
    async fn caller_can_cancel_a_pending_read_identity_check() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["method"], "eth_chainId");
            // No identity response arrives before the caller cancels the read.
            std::thread::sleep(Duration::from_millis(100));
        });
        let rpc = HttpRpc::isolated_localhost(format!("http://{address}"), 1).unwrap();
        let cancelled = tokio::time::timeout(
            Duration::from_millis(25),
            rpc.request("eth_getBalance", json!(["0x0", "latest"])),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the caller should cancel the pending read"
        );
        let diagnostics = rpc.diagnostics();
        assert_eq!(diagnostics["chain_id"], 1);
        assert_eq!(diagnostics["last_read"]["outcome"], "cancelled");
        assert_eq!(
            diagnostics["last_read"]["attempts"],
            json!([{"endpoint_id":"rpc-1","reason":"cancelled"}])
        );
        assert!(!diagnostics.to_string().contains(&address.to_string()));
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn rate_limited_read_retries_then_fails_over_to_backup() {
        // Keep retry coverage deterministic: B waits until A finishes its script.
        let retries_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut rpc, primary) = fixture_rpc_with_gate(
            vec![
                FixtureResponse::Json(json!("0x1")),
                FixtureResponse::Http(429),
                FixtureResponse::Http(429),
                FixtureResponse::Http(429),
            ],
            None,
            Some(retries_complete.clone()),
        );
        // RPC B is also on Ethereum and returns the requested balance.
        let (backup_rpc, backup) = fixture_rpc_with_gate(
            vec![
                FixtureResponse::Json(json!("0x1")),
                FixtureResponse::Json(json!("0x2a")),
            ],
            Some(retries_complete),
            None,
        );
        rpc.urls.extend(backup_rpc.urls);

        let balance = rpc
            .request(
                "eth_getBalance",
                json!(["0x0000000000000000000000000000000000000002", "0x10"]),
            )
            .await
            .unwrap();
        assert_eq!(balance, json!("0x2a"));

        // Check completion before joining so missing requests cannot hang the test.
        tokio::time::timeout(Duration::from_secs(1), async {
            while !primary.is_finished() || !backup.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both RPC fixtures must finish their expected requests");
        assert_eq!(
            primary.join().unwrap(),
            vec![
                "eth_chainId",
                "eth_getBalance",
                "eth_getBalance",
                "eth_getBalance"
            ]
        );
        assert_eq!(
            backup.join().unwrap(),
            vec!["eth_chainId", "eth_getBalance"]
        );
    }

    #[tokio::test]
    async fn transient_read_retries_then_succeeds() {
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Http(503),
            FixtureResponse::Json(json!("0x1234")),
            FixtureResponse::Json(fresh_fixture_header()),
        ]);
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap(),
            "0x1234"
        );
        assert_eq!(
            worker.join().unwrap(),
            vec![
                "eth_chainId",
                "eth_call",
                "eth_call",
                "eth_getBlockByNumber"
            ]
        );
    }

    #[tokio::test]
    async fn endpoint_access_failure_fails_over_for_reads() {
        let (rpc, first, second) = two_endpoint_rpc(FixtureResponse::Http(403));
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap(),
            "0xbeef"
        );
        assert_eq!(first.join().unwrap(), vec!["eth_chainId", "eth_call"]);
        assert_eq!(
            second.join().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_getBlockByNumber"]
        );
    }

    #[tokio::test]
    async fn explicitly_transient_rpc_error_retries_but_contract_revert_does_not() {
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::RpcError(-32005, "rate limit exceeded"),
            FixtureResponse::Json(json!("0x1234")),
            FixtureResponse::Json(fresh_fixture_header()),
        ]);
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap(),
            "0x1234"
        );
        assert_eq!(
            worker.join().unwrap(),
            vec![
                "eth_chainId",
                "eth_call",
                "eth_call",
                "eth_getBlockByNumber"
            ]
        );

        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::RpcError(3, "execution reverted"),
        ]);
        let failure = rpc
            .request("eth_call", json!([{}, "latest"]))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "RPC_REQUEST_FAILED");
        assert_eq!(worker.join().unwrap(), vec!["eth_chainId", "eth_call"]);
    }

    #[tokio::test]
    async fn historical_state_error_fails_over_but_revert_does_not() {
        let (rpc, first, second) = historical_state_fallback_rpc(
            "missing trie node PRIVATE_PROVIDER_DATA",
            json!(["0x0000000000000000000000000000000000000002", "0x10"]),
        );
        assert_eq!(
            rpc.request(
                "eth_getCode",
                json!(["0x0000000000000000000000000000000000000002", "0x10"])
            )
            .await
            .unwrap(),
            "0x6000"
        );
        assert_eq!(
            first.join().unwrap(),
            vec!["eth_chainId", "eth_getCode", "eth_getCode", "eth_getCode"]
        );
        assert_eq!(second.join().unwrap(), vec!["eth_chainId", "eth_getCode"]);

        let (rpc, first, second) =
            two_endpoint_rpc(FixtureResponse::RpcError(-32000, "execution reverted"));
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap_err()
                .code,
            "RPC_REQUEST_FAILED"
        );
        assert_eq!(first.join().unwrap(), vec!["eth_chainId", "eth_call"]);
        assert!(
            second
                .join()
                .unwrap()
                .iter()
                .all(|method| method == "eth_chainId")
        );

        let (rpc, first, second) =
            two_endpoint_rpc_with_chain_responses(FixtureResponse::Json(json!("0x6000")), "0x2", 1);
        assert_eq!(
            rpc.request(
                "eth_getCode",
                json!(["0x0000000000000000000000000000000000000002", "0x10"])
            )
            .await
            .unwrap(),
            json!("0xbeef")
        );
        assert_eq!(first.join().unwrap(), vec!["eth_chainId"]);
        assert_eq!(second.join().unwrap(), vec!["eth_chainId", "eth_getCode"]);
    }

    #[tokio::test]
    async fn semantic_reverts_fail_closed_but_invalid_response_identity_cannot_win() {
        let (rpc, first, second) =
            two_endpoint_rpc(FixtureResponse::RpcError(3, "execution reverted"));
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap_err()
                .code,
            "RPC_REQUEST_FAILED"
        );
        assert_eq!(first.join().unwrap(), vec!["eth_chainId", "eth_call"]);
        assert!(
            second
                .join()
                .unwrap()
                .iter()
                .all(|method| method == "eth_chainId")
        );

        let (rpc, first, second) = two_endpoint_rpc(FixtureResponse::Raw(json!({
            "jsonrpc":"2.0",
            "id":9,
            "error":{"code":-32005,"message":"rate limit exceeded"}
        })));
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap(),
            json!("0xbeef")
        );
        assert_eq!(first.join().unwrap(), vec!["eth_chainId", "eth_call"]);
        assert_eq!(
            second.join().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_getBlockByNumber"]
        );
    }

    #[tokio::test]
    async fn transient_read_exhaustion_returns_sanitized_failure() {
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Http(503),
            FixtureResponse::Http(503),
            FixtureResponse::Http(503),
        ]);
        let failure = rpc
            .request("eth_call", json!([{}, "latest"]))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "RPC_UNAVAILABLE");
        assert_eq!(failure.details.as_deref().unwrap()["http_status"], 503);
        assert_eq!(
            worker.join().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_call", "eth_call"]
        );
    }

    #[test]
    fn revert_diagnostics_expose_only_selectors_and_reject_noncanonical_wrappers() {
        let mut payload = keccak256("ExecutionFailed(uint256,bytes)")[..4].to_vec();
        payload.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        payload.extend_from_slice(&U256::from(64).to_be_bytes::<32>());
        let private = b"\x12\x34\x56\x78SYNTHETIC_PRIVATE_SIGNATURE";
        payload.extend_from_slice(&U256::from(private.len()).to_be_bytes::<32>());
        payload.extend_from_slice(private);
        payload.resize(100 + private.len().div_ceil(32) * 32, 0);
        let failure = rpc_rejection(
            &json!({"code":3,"message":"execution reverted SYNTHETIC_PRIVATE_MESSAGE","data":format!("0x{}",hex::encode(&payload))}),
            false,
        );
        assert!(!failure.retryable);
        let encoded = serde_json::to_value(&failure.error).unwrap();
        assert_eq!(encoded["details"]["inner_error_selector"], "0x12345678");
        assert_eq!(encoded["details"]["error_class"], "contract_revert");
        assert!(!encoded.to_string().contains("SYNTHETIC_PRIVATE"));
        assert!(!encoded.to_string().contains(&hex::encode(private)));
        for mutation in [0, 1, 2, 3, 4] {
            let mut invalid = payload.clone();
            match mutation {
                0 => invalid[67] = 32,
                1 => invalid[68..100].fill(0xff),
                2 => invalid.truncate(103),
                3 => invalid.push(0),
                _ => *invalid.last_mut().unwrap() = 1,
            }
            assert_eq!(universal_router_inner_error_selector(&invalid), None);
        }
        for invalid in [
            "0x123",
            "0xzzzzzzzz",
            "12345678",
            &format!("0x{}", "ab".repeat(4097)),
        ] {
            assert!(hex_revert_payload(&json!(invalid)).is_none());
        }
        let no_revert = rpc_rejection(
            &json!({"code":3,"message":"private error","data":"0x12345678"}),
            false,
        );
        let details = no_revert.error.details.as_deref().unwrap();
        assert_eq!(details["json_rpc_code"], 3);
        assert!(details.get("error_class").is_none());
        assert!(details.get("error_selector").is_none());
    }

    #[test]
    fn rpc_rejection_retains_only_exact_numeric_codes() {
        for code in [-32700_i64, -32602, -32005, -32002, 3, i64::MIN, i64::MAX] {
            let failure = rpc_rejection(
                &json!({
                    "code":code,
                    "message":"SYNTHETIC_PRIVATE_MESSAGE",
                    "data":{"calldata":"SYNTHETIC_PRIVATE_DATA"},
                    "params":["SYNTHETIC_PRIVATE_PARAMS"]
                }),
                false,
            );
            assert_eq!(failure.error.code, "RPC_REQUEST_FAILED");
            assert!(
                failure
                    .error
                    .message
                    .contains(&format!("JSON-RPC code {code}"))
            );
            assert_eq!(failure.retryable, matches!(code, -32002 | -32005));
            assert_eq!(
                failure.error.details.as_deref().unwrap()["json_rpc_code"],
                code
            );
            assert!(
                !serde_json::to_string(&failure.error)
                    .unwrap()
                    .contains("SYNTHETIC_PRIVATE")
            );
        }
        for details in [
            json!({"code":"SYNTHETIC_PRIVATE_CODE","message":"SYNTHETIC_PRIVATE_MESSAGE"}),
            json!({"code":-32005.5,"data":"SYNTHETIC_PRIVATE_DATA"}),
            json!({"code":u64::MAX,"message":"SYNTHETIC_PRIVATE_MESSAGE"}),
            json!({"message":"SYNTHETIC_PRIVATE_MESSAGE"}),
            json!("SYNTHETIC_PRIVATE_ERROR"),
            Value::Null,
        ] {
            let failure = rpc_rejection(&details, false);
            assert_eq!(failure.error.code, "RPC_REQUEST_FAILED");
            assert!(!failure.retryable);
            assert!(!failure.error.message.contains("JSON-RPC code"));
            assert!(
                !serde_json::to_string(&failure.error)
                    .unwrap()
                    .contains("SYNTHETIC_PRIVATE")
            );
        }
    }

    #[test]
    fn only_recognized_historical_state_errors_are_fallback_eligible() {
        for (code, message) in [
            (-32000, "missing trie node abc"),
            (-32001, "historical state is not available"),
            (-32000, "state unavailable"),
            (
                -32000,
                "historical state 61a3d7d087734a9401c2fdfdb914fb3085bce806bb4c7f432be010eefea0e042 is not available",
            ),
            (
                -32000,
                "historical state 0x61a3d7d087734a9401c2fdfdb914fb3085bce806bb4c7f432be010eefea0e042 is not available",
            ),
        ] {
            let failure = rpc_rejection(&json!({"code":code,"message":message}), true);
            assert!(failure.retryable);
            assert_eq!(failure.error.code, "RPC_HISTORICAL_STATE_UNAVAILABLE");
            assert!(!failure.error.message.contains("abc"));
            assert!(!failure.error.message.contains(message));
        }
        for (code, message) in [
            (-32000, "execution reverted"),
            (-32000, "execution reverted: missing trie node"),
            (-32001, "invalid params"),
            (-32000, "historical state invalid params is not available"),
            (-32000, "historical state 61a3 is not available"),
            (
                -32000,
                "execution reverted: historical state 61a3d7d087734a9401c2fdfdb914fb3085bce806bb4c7f432be010eefea0e042 is not available",
            ),
            (-32002, "missing trie node"),
        ] {
            let failure = rpc_rejection(&json!({"code":code,"message":message}), true);
            assert!(!failure.retryable || matches!(code, -32002));
            if code != -32002 {
                assert_eq!(failure.error.code, "RPC_REQUEST_FAILED");
            }
        }
        let latest_state_error =
            rpc_rejection(&json!({"code":-32000,"message":"missing trie node"}), false);
        assert!(!latest_state_error.retryable);
        assert_eq!(latest_state_error.error.code, "RPC_REQUEST_FAILED");
    }

    #[tokio::test]
    async fn historical_state_root_error_uses_backup_without_changing_block() {
        let params = json!([{"to":"0x82af49447d8a07e3bd95bd0d56f35241523fbab1", "data":"0x18160ddd"}, "0x11e1a300"]);
        let (rpc, first, second) = historical_state_fallback_rpc(
            "historical state 61a3d7d087734a9401c2fdfdb914fb3085bce806bb4c7f432be010eefea0e042 is not available",
            params.clone(),
        );
        assert_eq!(
            rpc.request("eth_call", params).await.unwrap(),
            json!("0x6000")
        );
        assert_eq!(
            first.join().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_call", "eth_call"]
        );
        assert_eq!(second.join().unwrap(), vec!["eth_chainId", "eth_call"]);
    }

    #[test]
    fn historical_fallback_is_limited_to_read_methods_with_explicit_block_numbers() {
        for (method, params) in [
            ("eth_getCode", json!(["0x1", "0x10"])),
            ("eth_getBalance", json!(["0x1", "0x10"])),
            ("eth_getStorageAt", json!(["0x1", "0x0", "0x10"])),
            ("eth_getProof", json!(["0x1", ["0x0"], "0x10"])),
            ("eth_call", json!([{"to":"0x1"}, "0x10"])),
            (
                "eth_call",
                json!([{"to":"0x1"}, {"blockHash":format!("0x{}", "11".repeat(32)), "requireCanonical":true}]),
            ),
        ] {
            assert!(is_historical_state_read(method, &params), "{method}");
        }
        for (method, params) in [
            ("eth_getCode", json!(["0x1", "latest"])),
            ("eth_getBalance", json!(["0x1"])),
            ("eth_getProof", json!(["0x1", ["0x0"], "pending"])),
            ("eth_call", json!([{"to":"0x1"}, "pending"])),
            (
                "eth_call",
                json!([{"to":"0x1"}, {"blockHash":"0x10", "requireCanonical":true}]),
            ),
            (
                "eth_call",
                json!([{"to":"0x1"}, {"blockHash":format!("0x{}", "11".repeat(32)), "requireCanonical":false}]),
            ),
            ("eth_sendRawTransaction", json!(["0xdeadbeef"])),
        ] {
            assert!(!is_historical_state_read(method, &params), "{method}");
        }
    }

    #[tokio::test]
    async fn transient_json_rpc_exhaustion_preserves_safe_numeric_diagnosis() {
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::RpcError(-32002, "SYNTHETIC_PRIVATE_MESSAGE"),
            FixtureResponse::RpcError(-32002, "SYNTHETIC_PRIVATE_MESSAGE"),
            FixtureResponse::RpcError(-32002, "SYNTHETIC_PRIVATE_MESSAGE"),
        ]);
        let failure = rpc
            .request("eth_call", json!([{}, "latest"]))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "RPC_UNAVAILABLE");
        assert!(failure.message.contains("JSON-RPC code -32002"));
        assert!(
            !serde_json::to_string(&failure)
                .unwrap()
                .contains("SYNTHETIC_PRIVATE")
        );
        assert_eq!(
            worker.join().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_call", "eth_call"]
        );
    }

    #[tokio::test]
    async fn hostile_json_rpc_write_error_keeps_numeric_code_without_retry_or_fallback() {
        let (rpc, first, second) = two_endpoint_rpc(FixtureResponse::Raw(json!({
            "jsonrpc":"2.0","id":1,
            "error":{
                "code":-32005,
                "message":"SYNTHETIC_PRIVATE_MESSAGE",
                "data":{"signed_transaction":"SYNTHETIC_PRIVATE_DATA"},
                "params":["SYNTHETIC_PRIVATE_PARAMS"]
            }
        })));
        let failure = rpc
            .request(
                "eth_sendRawTransaction",
                json!(["SYNTHETIC_PRIVATE_REQUEST"]),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "RPC_UNAVAILABLE");
        assert!(failure.message.contains("JSON-RPC code -32005"));
        assert!(
            !serde_json::to_string(&failure)
                .unwrap()
                .contains("SYNTHETIC_PRIVATE")
        );
        assert_eq!(
            first.join().unwrap(),
            vec!["eth_chainId", "eth_sendRawTransaction"]
        );
        assert!(second.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn submitted_write_is_never_retried() {
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Http(503),
        ]);
        let failure = rpc
            .request("eth_sendRawTransaction", json!(["0xdeadbeef"]))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "RPC_UNAVAILABLE");
        assert_eq!(
            worker.join().unwrap(),
            vec!["eth_chainId", "eth_sendRawTransaction"]
        );
    }
}

#[cfg(test)]
mod clipped_tip_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ParallelFeeRpc {
        entered: AtomicUsize,
        both_entered: tokio::sync::Barrier,
        calls: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl EvmRpc for ParallelFeeRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.calls.lock().unwrap().push((method.to_owned(), params));
            let index = self.entered.fetch_add(1, Ordering::SeqCst);
            if index < 2 {
                self.both_entered.wait().await;
            }
            Ok(match method {
                "eth_getBlockByNumber" => json!({"baseFeePerGas":"0x3b9aca00"}),
                "eth_maxPriorityFeePerGas" => json!("0x2540be400"),
                _ => {
                    return Err(error(
                        "SYNTHETIC_METHOD",
                        "test",
                        "Unexpected fee fixture RPC.",
                    ));
                }
            })
        }
    }

    struct FeeRpc;
    #[async_trait]
    impl EvmRpc for FeeRpc {
        async fn request(&self, method: &str, _: Value) -> Result<Value> {
            Ok(match method {
                "eth_chainId" => json!("0x1"),
                "eth_call" | "eth_getCode" => json!("0x"),
                "eth_estimateGas" => json!("0x5208"),
                "eth_getBlockByNumber" => json!({"baseFeePerGas":"0x3b9aca00"}),
                "eth_maxPriorityFeePerGas" => json!("0x2540be400"),
                "eth_getTransactionCount" => json!("0x0"),
                "eth_getBalance" => json!("0xde0b6b3a7640000"),
                _ => {
                    return Err(error(
                        "SYNTHETIC_METHOD",
                        "test",
                        "Unreviewed fixture method.",
                    ));
                }
            })
        }
    }

    #[tokio::test]
    async fn fee_block_and_priority_reads_overlap_with_unchanged_rpc_envelopes() {
        let rpc = Arc::new(ParallelFeeRpc {
            entered: AtomicUsize::new(0),
            both_entered: tokio::sync::Barrier::new(2),
            calls: Mutex::new(Vec::new()),
        });
        let runtime = EvmRuntime::new(rpc.clone(), 1, None);
        let (max, priority) = runtime.fees().await.unwrap();

        assert_eq!(max, U256::from(12_000_000_000u64));
        assert_eq!(priority, U256::from(10_000_000_000u64));
        let calls = rpc.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().any(|(method, params)| {
            method == "eth_getBlockByNumber" && params == &json!(["latest", false])
        }));
        assert!(calls.iter().any(|(method, params)| {
            method == "eth_maxPriorityFeePerGas" && params == &json!([])
        }));
    }

    struct ParallelPrepareRpc {
        preview_reads: tokio::sync::Barrier,
        balance_reads: AtomicUsize,
        calls: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl EvmRpc for ParallelPrepareRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.calls.lock().unwrap().push((method.to_owned(), params));
            match method {
                "eth_chainId" => Ok(json!("0x1")),
                "eth_getBalance" => {
                    if self.balance_reads.fetch_add(1, Ordering::SeqCst) > 0 {
                        self.preview_reads.wait().await;
                    }
                    Ok(json!("0xde0b6b3a7640000"))
                }
                "eth_call"
                | "eth_estimateGas"
                | "eth_getBlockByNumber"
                | "eth_maxPriorityFeePerGas"
                | "eth_getTransactionCount" => {
                    self.preview_reads.wait().await;
                    Ok(match method {
                        "eth_call" => json!("0x"),
                        "eth_estimateGas" => json!("0x5208"),
                        "eth_getBlockByNumber" => json!({"baseFeePerGas":"0x3b9aca00"}),
                        "eth_maxPriorityFeePerGas" => json!("0x2540be400"),
                        "eth_getTransactionCount" => json!("0x0"),
                        _ => unreachable!(),
                    })
                }
                _ => Err(error(
                    "SYNTHETIC_METHOD",
                    "test",
                    "Unexpected preview fixture RPC.",
                )),
            }
        }
    }

    #[tokio::test]
    async fn preview_simulation_estimate_fees_and_nonce_overlap_with_exact_calls() {
        let rpc = Arc::new(ParallelPrepareRpc {
            preview_reads: tokio::sync::Barrier::new(5),
            balance_reads: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
        });
        let runtime = EvmRuntime::new(rpc.clone(), 1, None);
        let sender = address("0x1111111111111111111111111111111111111111").unwrap();
        let to = address("0x2222222222222222222222222222222222222222").unwrap();
        let calldata = Bytes::from_static(&[0x12, 0x34]);
        let prepared = runtime
            .prepare(
                sender,
                to,
                U256::ZERO,
                calldata,
                U256::from(1_000_000_000_000_000u64),
            )
            .await
            .unwrap();
        runtime.revalidate(&prepared.transaction).await.unwrap();

        let calls = rpc.calls.lock().unwrap();
        let call = calls
            .iter()
            .find(|(method, _)| method == "eth_call")
            .unwrap();
        let expected = json!({
            "from": lower_address(sender),
            "to": lower_address(to),
            "value": "0x0",
            "data": "0x1234"
        });
        assert_eq!(call.1, json!([expected, "latest"]));
        let estimate = calls
            .iter()
            .find(|(method, _)| method == "eth_estimateGas")
            .unwrap();
        assert_eq!(estimate.1, json!([expected]));
        assert!(calls.iter().any(|(method, params)| {
            method == "eth_getTransactionCount"
                && params == &json!([lower_address(sender), "pending"])
        }));
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "eth_call")
                .count(),
            2
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "eth_estimateGas")
                .count(),
            2
        );
    }

    struct FailedPreviewRpc {
        methods: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl EvmRpc for FailedPreviewRpc {
        async fn request(&self, method: &str, _: Value) -> Result<Value> {
            self.methods.lock().unwrap().push(method.to_owned());
            match method {
                "eth_chainId" => Ok(json!("0x1")),
                "eth_call" => Err(error("SIMULATION_FAILED", "test", "Synthetic revert.")),
                "eth_estimateGas" => Ok(json!("0x5208")),
                "eth_getBlockByNumber" => Ok(json!({"baseFeePerGas":"0x3b9aca00"})),
                "eth_maxPriorityFeePerGas" => Ok(json!("0x2540be400")),
                "eth_getTransactionCount" => Ok(json!("0x0")),
                _ => Err(error(
                    "SYNTHETIC_METHOD",
                    "test",
                    "Unexpected failed-preview fixture RPC.",
                )),
            }
        }
    }

    #[tokio::test]
    async fn failed_preview_read_stops_before_later_balance_work() {
        let rpc = Arc::new(FailedPreviewRpc {
            methods: Mutex::new(Vec::new()),
        });
        let runtime = EvmRuntime::new(rpc.clone(), 1, None);
        let failure = runtime
            .prepare(
                address("0x1111111111111111111111111111111111111111").unwrap(),
                address("0x2222222222222222222222222222222222222222").unwrap(),
                U256::ZERO,
                Bytes::new(),
                U256::from(1_000_000_000_000_000u64),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "SIMULATION_FAILED");
        assert!(!rpc.methods.lock().unwrap().iter().any(|method| {
            method == "eth_getBalance" || method == "eth_getL1Fee" || method == "eth_estimateL1Fee"
        }));
    }

    #[tokio::test]
    async fn clipped_quote_preserves_base_fee_room_and_frozen_bid_stays_strict() {
        let runtime = EvmRuntime::new(Arc::new(FeeRpc), 1, None);
        let cap = U256::from(25_200) * U256::from(13_000_000_000u64);
        let prepared = runtime
            .prepare(
                address("0x1111111111111111111111111111111111111111").unwrap(),
                address("0x2222222222222222222222222222222222222222").unwrap(),
                U256::ZERO,
                Bytes::new(),
                cap,
            )
            .await
            .unwrap();
        assert_eq!(
            prepared.transaction.max_fee_per_gas,
            quantity(U256::from(13_000_000_000u64))
        );
        assert_eq!(
            prepared.transaction.max_priority_fee_per_gas,
            quantity(U256::from(12_000_000_000u64))
        );
        runtime.revalidate(&prepared.transaction).await.unwrap();
        assert!(prepared.transaction.maximum_cost().unwrap() <= cap);
        assert_eq!(
            validate_frozen_fee_bid(&prepared.transaction, U256::from(2_000_000_000u64))
                .unwrap_err()
                .code,
            "FEE_BOUND_CHANGED"
        );
    }
}
