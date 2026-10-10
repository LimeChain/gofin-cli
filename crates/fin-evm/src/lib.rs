//! Reviewed EVM mechanics. Signing accepts only a frozen, broker-authorized envelope.
//! This crate never loads credentials, exposes signatures, or stores broadcast bytes.
use alloy::{
    consensus::{SignableTransaction, TxEip1559, TxEnvelope, transaction::SignerRecoverable},
    eips::{eip2718::Decodable2718, eip2930::AccessList},
    primitives::{Address, B256, Bytes, TxKind, U256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::SolCall,
};
use async_trait::async_trait;
use fin_domain::{FinError, Quantity, Result, Submission, SubmissionJournal};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

pub use alloy;
mod fees;
pub use fees::{OperatorFeeModel, RollupFeeBound};

fn error(code: &str, stage: &str, message: &str) -> FinError {
    FinError::new(
        code,
        stage,
        message,
        "Inspect the original operation and its configured network; never replay an uncertain submission.",
    )
}

fn preserve_rpc_cause(mut mapped: FinError, source: &FinError) -> FinError {
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

#[async_trait]
pub trait EvmRpc: Send + Sync {
    /// Internal runtime boundary, never an agent-visible raw-method endpoint.
    async fn request(&self, method: &str, params: Value) -> Result<Value>;
}

pub struct HttpRpc {
    urls: Vec<String>,
    expected_chain_id: u64,
    client: reqwest::Client,
    verified: Mutex<BTreeSet<String>>,
}

struct RpcAttemptFailure {
    error: FinError,
    retryable: bool,
}

const MAX_READ_RETRIES_PER_ENDPOINT: usize = 2;
const READ_RETRY_DELAYS_MS: [u64; MAX_READ_RETRIES_PER_ENDPOINT] = [100, 250];

fn rpc_rejection(details: &Value, historical_state_fallback: bool) -> RpcAttemptFailure {
    // Read only an exact signed integer. Provider messages, data and other
    // fields can contain request parameters or private revert payloads.
    let code = details.get("code").and_then(Value::as_i64);
    let normalized_message = details
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
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
            || normalized_message.contains("state unavailable"));
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
    if let Some(code) = code {
        error = error.with_details(json!({"json_rpc_code":code}));
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
        Self::construct(vec![url], expected_chain_id, true)
    }
    fn construct(urls: Vec<String>, expected_chain_id: u64, local: bool) -> Result<Self> {
        if urls.is_empty() || expected_chain_id == 0 {
            return Err(FinError::validation(
                "RPC URLs and an explicit nonzero chain identity are required.",
            ));
        }
        for text in &urls {
            let url =
                reqwest::Url::parse(text).map_err(|_| FinError::validation("Invalid RPC URL."))?;
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
                return Err(FinError::validation(
                    "Production RPCs require public HTTPS without embedded credentials; isolated fixtures require loopback HTTP.",
                ));
            }
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| {
                error(
                    "RPC_UNAVAILABLE",
                    "configuration",
                    "Could not initialize the public RPC transport.",
                )
            })?;
        Ok(Self {
            urls,
            expected_chain_id,
            client,
            verified: Mutex::new(BTreeSet::new()),
        })
    }
    async fn at_once(
        &self,
        url: &str,
        method: &str,
        params: Value,
        historical_state_fallback: bool,
    ) -> std::result::Result<Value, RpcAttemptFailure> {
        let response = self
            .client
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
            return Err(RpcAttemptFailure {
                retryable: matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504),
                error: error("RPC_UNAVAILABLE", "rpc", "RPC HTTP request failed.")
                    .with_details(json!({"http_status":status.as_u16()})),
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
                Err(failure) if failure.retryable && retry_index < retries => {
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
#[async_trait]
impl EvmRpc for HttpRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
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
        let urls = if method == "eth_sendRawTransaction" {
            &self.urls[..1]
        } else {
            &self.urls[..]
        };
        let historical_state_fallback = is_historical_state_read(method, &params);
        let mut failure = error(
            "RPC_UNAVAILABLE",
            "rpc",
            "No configured RPC returned a usable response.",
        );
        for url in urls {
            match self.ensure_endpoint(url).await {
                Ok(()) => (),
                Err(e) if e.retryable => {
                    failure = e.error;
                    continue;
                }
                Err(e) => return Err(e.error),
            }
            let retries = if method == "eth_sendRawTransaction" {
                0
            } else {
                MAX_READ_RETRIES_PER_ENDPOINT
            };
            match self
                .at(
                    url,
                    method,
                    params.clone(),
                    retries,
                    historical_state_fallback,
                )
                .await
            {
                Ok(value) => return Ok(value),
                Err(e) if e.retryable => failure = e.error,
                Err(e) => return Err(e.error),
            }
        }
        // Financial dispatch never advances to a second endpoint after an uncertain result.
        Err(failure)
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
                .request(
                    "eth_getCode",
                    json!([lower_address(contract), block_tag(block)]),
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
                .request(
                    "eth_getBalance",
                    json!([lower_address(owner), block_tag(block)]),
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
                .request("eth_call", json!([transaction, block_tag(block)]))
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
        let block = self
            .rpc
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let base = rpc_u256(&block["baseFeePerGas"])?;
        let priority = rpc_u256(
            &self
                .rpc
                .request("eth_maxPriorityFeePerGas", json!([]))
                .await?,
        )?
        .max(U256::from(10_000_000));
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
        let simulation = rpc_bytes(
            &self
                .rpc
                .request("eth_call", json!([request.clone(), "latest"]))
                .await?,
        )?;
        let estimate = rpc_u64(
            &self
                .rpc
                .request("eth_estimateGas", json!([request]))
                .await?,
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
        let (required, priority) = self.fees().await?;
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
            nonce: self.pending_nonce(sender).await?,
            gas,
            max_fee_per_gas: quantity(max),
            max_priority_fee_per_gas: quantity(max_priority),
            rollup_fee: None,
        };
        transaction.consensus()?;
        transaction.rollup_fee = self.rollup_fee_bound(&transaction).await?;
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
        transaction.max_priority_fee_per_gas = quantity(max_priority.min(max));
        transaction.consensus()?;
        let cost = transaction.maximum_cost()?;
        if cost > maximum_total_fee {
            return Err(error(
                "FEE_LIMIT",
                "preview",
                "Current conservative gas cost exceeds the explicit maximum fee.",
            ));
        }
        if self.balance(sender, None).await?
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
        if self.pending_nonce(address(&transaction.sender)?).await? != transaction.nonce {
            return Err(error(
                "NONCE_CHANGED",
                "submission",
                "Pending nonce differs from the authorized frozen envelope.",
            ));
        }
        let (max, priority) = self.fees().await?;
        if max > u256(&transaction.max_fee_per_gas)?
            || priority > u256(&transaction.max_priority_fee_per_gas)?
        {
            return Err(error(
                "FEE_BOUND_CHANGED",
                "submission",
                "Current fee conditions exceed the approved frozen envelope.",
            ));
        }
        self.revalidate_rollup_fee(transaction).await?;
        let request = transaction.rpc_call();
        let simulation = rpc_bytes(
            &self
                .rpc
                .request("eth_call", json!([request.clone(), "latest"]))
                .await?,
        )?;
        let estimate = rpc_u64(
            &self
                .rpc
                .request("eth_estimateGas", json!([request]))
                .await?,
        )?;
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
        if self.balance(address(&transaction.sender)?, None).await? < debit {
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
        let signer = self.signer.as_ref().ok_or_else(|| {
            error(
                "PROTECTED_SIGNER_REQUIRED",
                "signing",
                "No protected signing provider is configured.",
            )
        })?;
        journal.assert_authorized()?;
        let raw = signer.sign_prepared(transaction).await?;
        let tx_hash = verify_signed_envelope(transaction, &raw)?;
        let submission = Submission {
            kind: "ethereum".into(),
            reference: format!("{tx_hash:#x}"),
            data: json!({"transaction":transaction,"metadata":metadata}),
        };
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
        let latest = rpc_u64(&self.rpc.request("eth_blockNumber", json!([])).await?)?;
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
        if reported.is_null()
            || hash(receipt["transactionHash"].as_str().unwrap_or(""))? != expected
            || hash(reported["hash"].as_str().unwrap_or(""))? != expected
        {
            return Err(error(
                "RECEIPT_IDENTITY_INVALID",
                "reconciliation",
                "RPC receipt and transaction do not match the reported candidate hash.",
            ));
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
        let latest = rpc_u64(&self.rpc.request("eth_blockNumber", json!([])).await?)?;
        let confirmations = if latest >= block_number {
            latest - block_number + 1
        } else {
            0
        };
        if confirmations < required {
            return Ok(None);
        }
        if require_finalized {
            let finalized = self
                .rpc
                .request("eth_getBlockByNumber", json!(["finalized", false]))
                .await?;
            if finalized.is_null() || rpc_u64(&finalized["number"])? < block_number {
                return Ok(None);
            }
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;

    enum FixtureResponse {
        Json(Value),
        RpcError(i64, &'static str),
        Raw(Value),
        Http(u16),
    }

    fn fixture_rpc(responses: Vec<FixtureResponse>) -> (HttpRpc, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let body = read_request(&mut stream);
                let request: Value = serde_json::from_slice(&body).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
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
            methods
        });
        let rpc = HttpRpc::isolated_localhost(format!("http://{address}"), 1).unwrap();
        (rpc, worker)
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let count = stream.read(&mut chunk).unwrap();
            assert_ne!(
                count, 0,
                "fixture client closed before sending a full request"
            );
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
                    return bytes[body_start..body_start + content_length].to_vec();
                }
            }
        }
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Unavailable" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
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
                "eth_blockNumber"
            ]
        );
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
            methods
        });
        let second_worker = std::thread::spawn(move || {
            second.set_nonblocking(true).unwrap();
            let mut methods = Vec::new();
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while std::time::Instant::now() < deadline {
                match second.accept() {
                    Ok((mut stream, _)) => {
                        let request: Value =
                            serde_json::from_slice(&read_request(&mut stream)).unwrap();
                        let method = request["method"].as_str().unwrap().to_owned();
                        methods.push(method.clone());
                        let result = if method == "eth_chainId" {
                            json!("0x1")
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
            verified: rpc.verified,
        };
        (rpc, first_worker, second_worker)
    }

    fn historical_state_fallback_rpc() -> (HttpRpc, JoinHandle<Vec<String>>, JoinHandle<Vec<String>>)
    {
        let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let second = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let first_address = first.local_addr().unwrap();
        let second_address = second.local_addr().unwrap();
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
                    json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"missing trie node PRIVATE_PROVIDER_DATA"}})
                }
                .to_string();
                write_response(&mut stream, 200, payload.as_bytes());
            }
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
            verified: rpc.verified,
        };
        (rpc, first_worker, second_worker)
    }

    #[tokio::test]
    async fn transient_read_retries_then_succeeds() {
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::Http(503),
            FixtureResponse::Json(json!("0x1234")),
        ]);
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap(),
            "0x1234"
        );
        assert_eq!(
            worker.join().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_call"]
        );
    }

    #[tokio::test]
    async fn explicitly_transient_rpc_error_retries_but_contract_revert_does_not() {
        let (rpc, worker) = fixture_rpc(vec![
            FixtureResponse::Json(json!("0x1")),
            FixtureResponse::RpcError(-32005, "rate limit exceeded"),
            FixtureResponse::Json(json!("0x1234")),
        ]);
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap(),
            "0x1234"
        );
        assert_eq!(
            worker.join().unwrap(),
            vec!["eth_chainId", "eth_call", "eth_call"]
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
        let (rpc, first, second) = historical_state_fallback_rpc();
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
        assert!(second.join().unwrap().is_empty());

        let (rpc, first, second) =
            two_endpoint_rpc_with_chain_responses(FixtureResponse::Json(json!("0x6000")), "0x2", 1);
        assert_eq!(
            rpc.request(
                "eth_getCode",
                json!(["0x0000000000000000000000000000000000000002", "0x10"])
            )
            .await
            .unwrap_err()
            .code,
            "WRONG_CHAIN"
        );
        assert_eq!(first.join().unwrap(), vec!["eth_chainId"]);
        assert!(second.join().unwrap().is_empty());
    }

    #[tokio::test]
    async fn semantic_and_identity_errors_do_not_fail_over_to_another_endpoint() {
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
        assert!(second.join().unwrap().is_empty());

        let (rpc, first, second) = two_endpoint_rpc(FixtureResponse::Raw(json!({
            "jsonrpc":"2.0",
            "id":9,
            "error":{"code":-32005,"message":"rate limit exceeded"}
        })));
        assert_eq!(
            rpc.request("eth_call", json!([{}, "latest"]))
                .await
                .unwrap_err()
                .code,
            "RPC_DATA_INVALID"
        );
        assert_eq!(first.join().unwrap(), vec!["eth_chainId", "eth_call"]);
        assert!(second.join().unwrap().is_empty());
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
