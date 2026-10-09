//! Relay adapter. The HTTP service provides untrusted candidate data only;
//! execution targets and spender identities are pinned in `ReviewedConfig`.
use crate::{
    ApprovalRequirement, BridgeBounds, BridgeCapabilities, BridgeEvidenceCandidate, BridgeFee,
    BridgeIntent, BridgeProvider, BridgeQuote, BridgeQuoteBatch, BridgeQuoteFailure,
    BridgeQuoteRequest, BridgeReconciliation, BridgeStep, BridgeStepKind, BridgeTransferRef,
    EvidenceKind, EvmCall, FeeKind, PreparedBridge, QuoteClass, RefundPath, bridge_error, ensure,
};
use alloy::{
    primitives::{Address, U256, keccak256},
    sol,
    sol_types::SolCall,
};
pub mod order;
pub mod proof;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fin_domain::{Amount, AssetId, Quantity, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    time::Duration,
};

sol! {
    interface IERC20Bridge { function approve(address spender, uint256 amount) external returns (bool); }
    interface IRelayDepository { function depositNative(address depositor, bytes32 id) external payable; function depositErc20(address depositor,address token,uint256 amount,bytes32 id) external; }
}

const RELAY_API: &str = "https://api.relay.link";
const SUPPORTED_CHAIN_IDS: [u64; 4] = [1, 42161, 4663, 8453];
const MAX_RELAY_ARRIVAL_ESTIMATE_SECONDS: u64 = 7 * 24 * 60 * 60;
/// Reviewed Relay solver account that may originate a RouterV3 destination fill.
pub const RELAY_FILL_AUTHORITY: &str = "0xf70da97812cb96acdf810712aa562db8dfa3dbef";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewedChain {
    pub chain_id: u64,
    pub protocol_chain_id: String,
    pub depository: String,
    pub router_v3: Option<String>,
    /// Protocol identity is deployment metadata; the API never selects it.
    pub protocol: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewedToken {
    pub chain_id: u64,
    pub address: String,
    pub decimals: u8,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewedConfig {
    pub chains: BTreeMap<u64, ReviewedChain>,
    pub tokens: BTreeSet<ReviewedTokenKey>,
    pub relay_solver_protocol_chain_id: String,
    pub settlement_hub_chain_id: u64,
    pub settlement_hub_address: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReviewedTokenKey {
    pub chain_id: u64,
    pub address: String,
    pub decimals: u8,
}
impl ReviewedConfig {
    pub fn validate(&self) -> Result<()> {
        ensure(
            self.relay_solver_protocol_chain_id == "base",
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Relay solver order identity must match the reviewed Base protocol chain.",
        )?;
        ensure(
            self.settlement_hub_chain_id == 537713,
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Relay settlement hub must be the independently reviewed Relay Chain (537713).",
        )?;
        ensure(
            parse_address(&self.settlement_hub_address)?
                == parse_address("0xDDD361727C22A01EB137880678A20b0BEaE69318")?,
            "BRIDGE_CONFIG_INVALID",
            "configuration",
            "Relay settlement hub address differs from the reviewed deployment.",
        )?;
        for (id, chain) in &self.chains {
            ensure(
                SUPPORTED_CHAIN_IDS.contains(id)
                    && *id == chain.chain_id
                    && [1, 42161, 4663, 8453].contains(id),
                "BRIDGE_CONFIG_INVALID",
                "configuration",
                "A reviewed deployment uses an unsupported or mismatched EVM chain id.",
            )?;
            parse_address(&chain.depository)?;

            if let Some(router) = &chain.router_v3 {
                parse_address(router)?;
            }
        }
        for token in &self.tokens {
            ensure(
                SUPPORTED_CHAIN_IDS.contains(&token.chain_id),
                "BRIDGE_CONFIG_INVALID",
                "configuration",
                "A reviewed token uses an unsupported chain.",
            )?;
            parse_address(&token.address)?;
        }
        Ok(())
    }
    fn chain(&self, id: u64) -> Result<&ReviewedChain> {
        self.chains.get(&id).ok_or_else(|| {
            bridge_error(
                "BRIDGE_ROUTE_UNAVAILABLE",
                "quote",
                format!("No reviewed Relay deployment is configured for chain {id}."),
                "Choose a reviewed chain or update Fin's reviewed deployment configuration.",
            )
        })
    }
    fn token_decimals(&self, chain: u64, address: &str) -> Option<u8> {
        self.tokens
            .iter()
            .find(|token| token.chain_id == chain && token.address.eq_ignore_ascii_case(address))
            .map(|t| t.decimals)
    }
}

#[derive(Clone)]
pub struct RelayProvider {
    client: Client,
    config: ReviewedConfig,
    api_base: String,
}
impl RelayProvider {
    pub fn new(config: ReviewedConfig) -> Result<Self> {
        config.validate()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("Fin/bridge")
            .build()
            .map_err(|_| {
                bridge_error(
                    "BRIDGE_PROVIDER_UNAVAILABLE",
                    "quote",
                    "Could not initialize the Relay HTTP client.",
                    "Retry the quote; no financial action was submitted.",
                )
            })?;
        Ok(Self {
            client,
            config,
            api_base: RELAY_API.into(),
        })
    }
    async fn quote_http(&self, request: &BridgeQuoteRequest) -> Result<Value> {
        let intent = &request.intent;
        let source_currency = currency(&intent.source);
        let dest = intent.destination_assets.first().ok_or_else(|| {
            bridge_error(
                "BRIDGE_ROUTE_UNAVAILABLE",
                "quote",
                "At least one exact destination asset is required.",
                "Select a supported destination token.",
            )
        })?;
        let body = json!({
            "user": intent.source_address,
            "originChainId": intent.source.chain_id,
            "destinationChainId": intent.destination_chain_id,
            "originCurrency": source_currency,
            "destinationCurrency": currency(dest),
            "amount": intent.amount.as_str(),
            "tradeType": "EXACT_INPUT",
            "recipient": intent.recipient,
            "refundTo": intent.refund_to,
            "explicitDeposit": true,
            "usePermit": false,
            "useDepositAddress": false,
            "disableOriginSwaps": true,
            "useExternalLiquidity": false,
            "includeProtocolData": true
        });
        let response = self
            .client
            .post(format!("{}/quote", self.api_base))
            .json(&body)
            .send()
            .await
            .map_err(|_| {
                bridge_error(
                    "BRIDGE_PROVIDER_UNAVAILABLE",
                    "quote",
                    "Relay's anonymous quote service could not be reached.",
                    "Retry later. An API key or unverifiable deposit flow is not supported.",
                )
            })?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "quote",
                "Relay now requires authentication for anonymous quotes.",
                "Fin does not accept a Relay API key; use another supported route or wait for anonymous protocol quotes to return.",
            ));
        }
        if response.status() == reqwest::StatusCode::GONE
            || response.status() == reqwest::StatusCode::NOT_FOUND
        {
            return Err(bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "quote",
                "Relay's anonymous quote endpoint is retired or unavailable.",
                "Fin does not fall back to a less verifiable deposit flow.",
            ));
        }
        if !response.status().is_success() {
            return Err(bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "quote",
                format!(
                    "Relay quote request failed with HTTP {}.",
                    response.status()
                ),
                "Retry later or choose another supported route.",
            ));
        }
        response.json().await.map_err(|_| {
            bridge_error(
                "BRIDGE_RESPONSE_INVALID",
                "quote",
                "Relay returned malformed JSON.",
                "Retry the quote. No financial action was submitted.",
            )
        })
    }
}

#[async_trait]
impl BridgeProvider for RelayProvider {
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "relay".into(),
            chains: self
                .config
                .chains
                .keys()
                .copied()
                .filter(|chain_id| *chain_id != 8453)
                .collect(),
            native_eth: true,
            erc20: true,
            protocol_versions: ["protocol.v2".to_owned()].into_iter().collect(),
        }
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        validate_intent(&self.config, &request.intent, &request.bounds)?;
        let mut batch = BridgeQuoteBatch {
            quotes: Vec::new(),
            failures: Vec::new(),
        };
        for asset in &request.intent.destination_assets {
            let mut one = request.clone();
            one.intent.destination_assets = vec![asset.clone()];
            match self
                .quote_http(&one)
                .await
                .and_then(|raw| parse_order_response(&raw, &one, &self.config))
            {
                Ok(quote) => batch.quotes.push(quote),
                Err(error) => batch.failures.push(BridgeQuoteFailure {
                    provider: Some("relay".into()),
                    destination_asset: asset.clone(),
                    error,
                }),
            }
        }
        Ok(batch)
    }
    fn prepare(
        &self,
        quote: &BridgeQuote,
        bounds: &BridgeBounds,
        wallet: &str,
    ) -> Result<PreparedBridge> {
        validate_intent(&self.config, &quote.intent, bounds)?;
        ensure(
            quote.provider == "relay",
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Quote provider is not Relay.",
        )?;
        ensure(
            quote.class == QuoteClass::Executable,
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Indicative quotes cannot be prepared for execution.",
        )?;
        let order_id = quote.protocol_order_id.as_deref().ok_or_else(|| {
            bridge_error(
                "BRIDGE_PROTOCOL_DATA_MISSING",
                "prepare",
                "Relay quote has no validated protocol.v2 order.",
                "Request another quote. Fin cannot execute the legacy deposit format.",
            )
        })?;
        let _order = parse_b256(order_id)?;
        let signer = parse_address(wallet)?;
        let mut steps = quote.protocol_steps.clone();
        ensure(
            !steps.is_empty(),
            "BRIDGE_PROTOCOL_DATA_MISSING",
            "prepare",
            "Relay quote has no validated finite source steps.",
        )?;
        for step in &steps {
            ensure(
                parse_address(&step.expected_sender)? == signer,
                "BRIDGE_ORDER_INVALID",
                "prepare",
                "Relay transaction sender differs from the selected physical wallet.",
            )?;
        }
        validate_quote_bounds(quote, bounds)?;
        validate_prepared_calls(&self.config, quote, &steps, parse_b256(order_id)?)?;
        let fixed_scope_digest = fixed_scope_digest(&quote.intent, &self.config)?;
        steps.sort_by_key(|step| step.ordinal);
        Ok(PreparedBridge {
            quote: quote.clone(),
            bounds: bounds.clone(),
            steps,
            source_order_id: Some(order_id.to_ascii_lowercase()),
            fixed_scope_digest,
        })
    }
    async fn reconcile(&self, transfer: &BridgeTransferRef) -> Result<BridgeReconciliation> {
        let request_id = transfer.provider_request_id.as_deref().ok_or_else(|| {
            bridge_error(
                "BRIDGE_TRANSFER_IDENTITY_MISSING",
                "reconcile",
                "Saved Relay transfer has no immutable provider request id.",
                "Keep the transfer unresolved and inspect its protected quote record.",
            )
        })?;
        let response = self
            .client
            .get(format!("{}/intents/status", self.api_base))
            .query(&[("requestId", request_id)])
            .send()
            .await
            .map_err(|_| {
                bridge_error(
                    "BRIDGE_PROVIDER_UNAVAILABLE",
                    "reconcile",
                    "Relay status is unavailable.",
                    "Retry status; preserve the original deposit identity.",
                )
            })?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "reconcile",
                "Relay status requires authentication.",
                "Retry later; Fin will reconcile the original order using its saved identity.",
            ));
        }
        if !response.status().is_success() {
            return Err(bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "reconcile",
                format!(
                    "Relay status request failed with HTTP {}.",
                    response.status()
                ),
                "Retry status; do not submit another deposit.",
            ));
        }
        let value: Value = response.json().await.map_err(|_| {
            bridge_error(
                "BRIDGE_RESPONSE_INVALID",
                "reconcile",
                "Relay returned malformed status JSON.",
                "Retry status; no settlement proof is inferred.",
            )
        })?;
        parse_evidence_candidates(&value, transfer)
    }
}

fn validate_quote_bounds(quote: &BridgeQuote, bounds: &BridgeBounds) -> Result<()> {
    ensure(
        quote.gross_debit.asset == quote.intent.source.asset_id()
            && quote.gross_debit.atoms == quote.intent.amount,
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Stored quote debit differs from the exact approved source asset or amount.",
    )?;
    let quote_deadline = DateTime::parse_from_rfc3339(&quote.valid_until)
        .map_err(|_| invalid_order("stored quote deadline is malformed"))?
        .timestamp();
    ensure(
        quote_deadline > 0,
        "BRIDGE_QUOTE_INVALID",
        "prepare",
        "Stored quote deadline is not a positive Unix timestamp.",
    )?;
    if let Some(maximum_deadline) = bounds.latest_protocol_deadline {
        validate_latest_protocol_deadline(maximum_deadline)?;
        ensure(
            quote_deadline as u64 <= maximum_deadline,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Stored quote protocol deadline exceeds the approved latest deadline.",
        )?;
    }
    let max = bounds
        .maximum_source_debit
        .as_ref()
        .unwrap_or(&quote.intent.amount);
    ensure(
        quote.gross_debit.atoms <= *max,
        "BRIDGE_BOUNDS_VIOLATED",
        "prepare",
        "Quote gross debit exceeds the explicit cap.",
    )?;
    if let Some(interval) = &bounds.input {
        ensure(
            interval.minimum <= quote.gross_debit.atoms
                && quote.gross_debit.atoms <= interval.maximum,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Quote input falls outside the approved interval.",
        )?;
    }
    if let Some(minimum) = &bounds.minimum_net_receipt {
        ensure(
            minimum.asset == quote.minimum_receipt.asset
                && quote.minimum_receipt.atoms >= minimum.atoms,
            "BRIDGE_BOUNDS_VIOLATED",
            "prepare",
            "Quote minimum receipt is below the approved floor.",
        )?;
    }
    for fee in &quote.fees {
        if let Some(cap) = bounds
            .fee_caps
            .iter()
            .find(|cap| cap.asset == fee.amount.asset && cap.kind == fee.kind)
        {
            ensure(
                fee.amount.atoms <= cap.maximum,
                "BRIDGE_BOUNDS_VIOLATED",
                "prepare",
                "Quote fee exceeds the explicit cap.",
            )?;
        } else if !bounds.fee_caps.is_empty() {
            return Err(bridge_error(
                "BRIDGE_BOUNDS_VIOLATED",
                "prepare",
                "Quote contains a fee without a matching explicit cap.",
                "Add an exact asset and fee-kind cap or choose another quote.",
            ));
        }
    }
    validate_minimum_ratio(
        &quote.intent.source,
        &crate::EvmAsset {
            chain_id: quote.intent.destination_chain_id,
            token: if quote.expected_receipt.asset.reference == "native" {
                None
            } else {
                Some(quote.expected_receipt.asset.reference.clone())
            },
            decimals: quote.expected_receipt.asset.decimals,
        },
        &quote.gross_debit.atoms,
        &quote.minimum_receipt.atoms,
        bounds.minimum_output_input_ratio.as_ref(),
    )?;
    Ok(())
}

fn validate_latest_protocol_deadline(deadline: u64) -> Result<()> {
    ensure(
        deadline > 0
            && i64::try_from(deadline)
                .ok()
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
                .is_some(),
        "BRIDGE_BOUNDS_INVALID",
        "validation",
        "The latest protocol deadline must be a positive representable Unix timestamp.",
    )
}
fn validate_prepared_calls(
    config: &ReviewedConfig,
    quote: &BridgeQuote,
    steps: &[BridgeStep],
    order_id: alloy::primitives::B256,
) -> Result<()> {
    let intent = &quote.intent;
    let amount = parse_u256(&quote.gross_debit.atoms)?;
    let depositor = parse_address(&intent.source_address)?;
    let depository = parse_address(&config.chain(intent.source.chain_id)?.depository)?;
    let expected_count = if intent.source.token.is_some() {
        ensure(
            steps.len() == 1 || steps.len() == 2,
            "BRIDGE_ORDER_INVALID",
            "prepare",
            "Prepared ERC-20 route must have one deposit and at most one approval step.",
        )?;
        steps.len()
    } else {
        ensure(
            steps.len() == 1,
            "BRIDGE_ORDER_INVALID",
            "prepare",
            "Native route must contain exactly one deposit step.",
        )?;
        1
    };
    let offset = if intent.source.token.is_some() && expected_count == 2 {
        1
    } else {
        0
    };
    if let Some(token) = &intent.source.token {
        let token_addr = parse_address(token)?;
        if expected_count == 2 {
            let approval = &steps[0];
            ensure(
                approval.kind == BridgeStepKind::Approval
                    && approval.ordinal == 0
                    && approval.call.chain_id == intent.source.chain_id
                    && parse_address(&approval.call.to)? == token_addr
                    && parse_address(&approval.expected_sender)? == depositor
                    && approval.call.value.is_zero(),
                "BRIDGE_ORDER_INVALID",
                "prepare",
                "Prepared approval is outside the exact source token, signer or chain.",
            )?;
            let bytes = parse_calldata(&approval.call.data)?;
            ensure(
                bytes
                    == IERC20Bridge::approveCall {
                        spender: depository,
                        amount,
                    }
                    .abi_encode(),
                "BRIDGE_ORDER_INVALID",
                "prepare",
                "Prepared approval is not finite to the reviewed depository and order amount.",
            )?;
        }
    }
    let deposit = &steps[offset];
    let value = if intent.source.token.is_some() {
        U256::ZERO
    } else {
        amount
    };
    let expected = if let Some(token) = &intent.source.token {
        IRelayDepository::depositErc20Call {
            depositor,
            token: parse_address(token)?,
            amount,
            id: order_id,
        }
        .abi_encode()
    } else {
        IRelayDepository::depositNativeCall {
            depositor,
            id: order_id,
        }
        .abi_encode()
    };
    ensure(
        deposit.kind == BridgeStepKind::Deposit
            && deposit.ordinal == offset as u8
            && deposit.call.chain_id == intent.source.chain_id
            && parse_address(&deposit.call.to)? == depository
            && parse_address(&deposit.expected_sender)? == depositor
            && parse_u256(&deposit.call.value)? == value
            && parse_calldata(&deposit.call.data)? == expected,
        "BRIDGE_ORDER_INVALID",
        "prepare",
        "Prepared deposit call differs from Relay's reviewed depository, asset, exact amount or canonical order ID.",
    )?;
    Ok(())
}
fn validate_intent(
    config: &ReviewedConfig,
    intent: &BridgeIntent,
    bounds: &BridgeBounds,
) -> Result<()> {
    if let Some(deadline) = bounds.latest_protocol_deadline {
        validate_latest_protocol_deadline(deadline)?;
    }
    ensure(
        intent.source.chain_id != intent.destination_chain_id,
        "BRIDGE_ROUTE_UNAVAILABLE",
        "validation",
        "Source and destination chains must differ.",
    )?;
    config.chain(intent.source.chain_id)?;
    config.chain(intent.destination_chain_id)?;
    ensure(
        intent.source.chain_id != 8453 && intent.destination_chain_id != 8453,
        "BRIDGE_ROUTE_UNAVAILABLE",
        "validation",
        "Base is a read-only Relay solver observer in this configuration.",
    )?;
    let source_addr = parse_address(&intent.source_address)?;
    let recipient_addr = parse_address(&intent.recipient)?;
    let refund_addr = parse_address(&intent.refund_to)?;
    ensure(
        source_addr != Address::ZERO
            && recipient_addr != Address::ZERO
            && refund_addr != Address::ZERO,
        "INVALID_ADDRESS",
        "validation",
        "Recipient and refund address must be nonzero.",
    )?;
    if let Some(token) = &intent.source.token {
        parse_address(token)?;
        let decimals = config
            .token_decimals(intent.source.chain_id, token)
            .ok_or_else(|| {
                bridge_error(
                    "BRIDGE_ROUTE_UNAVAILABLE",
                    "validation",
                    "Source token is not explicitly reviewed for this network.",
                    "Choose a reviewed asset.",
                )
            })?;
        ensure(
            decimals == intent.source.decimals,
            "BRIDGE_ASSET_INVALID",
            "validation",
            "Source token precision differs from reviewed asset metadata.",
        )?;
    } else {
        ensure(
            intent.source.decimals == 18,
            "BRIDGE_ASSET_INVALID",
            "validation",
            "Native ETH precision must be exactly 18 decimals.",
        )?;
    }
    ensure(
        !intent.destination_assets.is_empty() && intent.destination_assets.len() <= 8,
        "INVALID_INPUT",
        "validation",
        "Choose between one and eight explicit destination assets.",
    )?;
    for asset in &intent.destination_assets {
        ensure(
            asset.chain_id == intent.destination_chain_id,
            "INVALID_INPUT",
            "validation",
            "Every destination asset must belong to the requested destination chain.",
        )?;
        if let Some(token) = &asset.token {
            parse_address(token)?;
            let decimals = config
                .token_decimals(asset.chain_id, token)
                .ok_or_else(|| {
                    bridge_error(
                        "BRIDGE_ROUTE_UNAVAILABLE",
                        "validation",
                        "Destination token is not explicitly reviewed for this network.",
                        "Choose a reviewed asset.",
                    )
                })?;
            ensure(
                decimals == asset.decimals,
                "BRIDGE_ASSET_INVALID",
                "validation",
                "Destination token precision differs from reviewed asset metadata.",
            )?;
        } else {
            ensure(
                asset.decimals == 18,
                "BRIDGE_ASSET_INVALID",
                "validation",
                "Native ETH precision must be exactly 18 decimals.",
            )?;
        }
    }
    ensure(
        !intent.amount.is_zero(),
        "INVALID_INPUT",
        "validation",
        "Bridge amount must be greater than zero.",
    )?;
    ensure(
        !intent.allowed_refund_assets.is_empty() && intent.allowed_refund_assets.len() <= 8,
        "BRIDGE_BOUNDS_INVALID",
        "validation",
        "Explicit refund assets are required, up to eight.",
    )?;
    for asset in &intent.allowed_refund_assets {
        ensure(
            asset.chain_id == intent.source.chain_id
                || asset.chain_id == intent.destination_chain_id,
            "BRIDGE_BOUNDS_INVALID",
            "validation",
            "Refund assets must be on the source or destination chain.",
        )?;
        if let Some(token) = &asset.token {
            parse_address(token)?;
            let decimals = config
                .token_decimals(asset.chain_id, token)
                .ok_or_else(|| {
                    bridge_error(
                        "BRIDGE_ROUTE_UNAVAILABLE",
                        "validation",
                        "Refund token is not explicitly reviewed for this network.",
                        "Choose a reviewed refund asset.",
                    )
                })?;
            ensure(
                decimals == asset.decimals,
                "BRIDGE_ASSET_INVALID",
                "validation",
                "Refund token precision differs from reviewed asset metadata.",
            )?;
        } else {
            ensure(
                asset.decimals == 18,
                "BRIDGE_ASSET_INVALID",
                "validation",
                "Native refund precision must be exactly 18 decimals.",
            )?;
        }
    }
    let maximum = bounds
        .maximum_source_debit
        .as_ref()
        .unwrap_or(&intent.amount);
    ensure(
        intent.amount <= *maximum,
        "BRIDGE_BOUNDS_VIOLATED",
        "validation",
        "Requested input exceeds the explicit maximum source debit.",
    )?;
    if let Some(interval) = &bounds.input {
        ensure(
            interval.minimum <= intent.amount && intent.amount <= interval.maximum,
            "BRIDGE_BOUNDS_VIOLATED",
            "validation",
            "Requested input is outside the approved interval.",
        )?;
    }
    for fee in &bounds.fee_caps {
        ensure(
            fee.asset.network == format!("ethereum:{}", intent.source.chain_id)
                || fee.asset.network == format!("ethereum:{}", intent.destination_chain_id),
            "BRIDGE_BOUNDS_INVALID",
            "validation",
            "Fee cap asset must be on the source or destination network.",
        )?;
    }
    Ok(())
}

fn parse_order_response(
    value: &Value,
    request: &BridgeQuoteRequest,
    config: &ReviewedConfig,
) -> Result<BridgeQuote> {
    parse_order_response_at(value, request, config, Utc::now())
}

fn parse_order_response_at(
    value: &Value,
    request: &BridgeQuoteRequest,
    config: &ReviewedConfig,
    now: DateTime<Utc>,
) -> Result<BridgeQuote> {
    if let Some(maximum_deadline) = request.bounds.latest_protocol_deadline {
        validate_latest_protocol_deadline(maximum_deadline)?;
    }
    let protocol = value
        .pointer("/protocol/v2")
        .ok_or_else(|| missing_protocol())?;
    let order = protocol
        .get("orderData")
        .ok_or_else(|| missing_protocol())?;
    ensure(
        order.get("version").and_then(Value::as_str) == Some("v1"),
        "BRIDGE_PROTOCOL_UNSUPPORTED",
        "quote",
        "Relay protocol.v2 order must use the reviewed EVM Order v1 encoding.",
    )?;
    let order_id = protocol
        .get("orderId")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_protocol())?;
    let stored_hash = parse_b256(order_id)?;
    let request_id = value
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_order("missing Relay requestId"))?;
    parse_b256(request_id)?;
    let source = &request.intent.source;
    let inputs = order
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing inputs"))?;
    ensure(
        inputs.len() == 1,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Only a single explicit source payment is supported.",
    )?;
    let payment = inputs[0]
        .get("payment")
        .ok_or_else(|| invalid_order("missing input payment"))?;
    let gross = Quantity::new(field_string(payment, &["amount"])?)?;
    ensure(
        !Quantity::new(field_string(payment, &["weight"])?)?.is_zero(),
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay input payment weight cannot be zero.",
    )?;
    ensure(
        gross == request.intent.amount,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay order source amount differs from the requested exact input.",
    )?;
    ensure(
        field_string(payment, &["chainId"])? == config.chain(source.chain_id)?.protocol_chain_id,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay order source chain differs from the requested route.",
    )?;
    let expected_source_asset = source
        .token
        .as_deref()
        .unwrap_or("0x0000000000000000000000000000000000000000");
    ensure(
        decode_evm_currency(field_string(payment, &["currency"])?)?
            == parse_address(expected_source_asset)?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay order source token differs from the requested asset.",
    )?;
    let output_order = order
        .get("output")
        .ok_or_else(|| invalid_order("missing output"))?;
    ensure(
        field_string(output_order, &["chainId"])?
            == config
                .chain(request.intent.destination_chain_id)?
                .protocol_chain_id,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay order destination chain differs from the request.",
    )?;
    let payments = output_order
        .get("payments")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing output payments"))?;
    ensure(
        payments.len() == 1,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Only one destination payment is supported.",
    )?;
    let output_payment = &payments[0];
    let output_currency = decode_evm_currency(field_string(output_payment, &["currency"])?)?;
    let destination = request
        .intent
        .destination_assets
        .iter()
        .find(|asset| {
            output_currency
                == asset
                    .token
                    .as_deref()
                    .map(parse_address)
                    .transpose()
                    .ok()
                    .flatten()
                    .unwrap_or(Address::ZERO)
        })
        .ok_or_else(|| invalid_order("order destination asset is not explicitly selected"))?;
    let output = Quantity::new(field_string(output_payment, &["expectedAmount"])?)?;
    let minimum_out = Quantity::new(field_string(output_payment, &["minimumAmount"])?)?;
    ensure(
        decode_evm_currency(field_string(output_payment, &["recipient"])?)?
            == parse_address(&request.intent.recipient)?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay output recipient differs from requested recipient.",
    )?;
    ensure(
        !minimum_out.is_zero() && minimum_out <= output,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay minimum output is zero or exceeds expected output.",
    )?;
    if let Some(required) = request.bounds.minimum_net_receipt.as_ref() {
        ensure(
            required.asset == destination.asset_id() && minimum_out >= required.atoms,
            "BRIDGE_BOUNDS_VIOLATED",
            "quote",
            "Relay minimum receipt is below the user's explicit bound.",
        )?;
    }
    let output_calls = output_order
        .get("calls")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing destination calls"))?;
    ensure(
        output_calls.is_empty(),
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Destination calls are unsupported; only a direct asset delivery is allowed.",
    )?;
    validate_router_extra_data(
        config,
        request.intent.destination_chain_id,
        field_string(output_order, &["extraData"])?,
    )?;
    let deadline = field_u64(output_order, &["deadline"])?;
    let valid_until = i64::try_from(deadline)
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .ok_or_else(|| invalid_order("invalid order deadline"))?;
    ensure(
        valid_until > now,
        "BRIDGE_QUOTE_EXPIRED",
        "quote",
        "Relay returned an expired protocol order.",
    )?;
    if let Some(maximum_deadline) = request.bounds.latest_protocol_deadline {
        ensure(
            deadline <= maximum_deadline,
            "BRIDGE_BOUNDS_VIOLATED",
            "quote",
            "Relay protocol deadline exceeds the approved latest deadline.",
        )?;
    }
    ensure(
        field_string(order, &["solverChainId"])? == config.relay_solver_protocol_chain_id,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Order solver chain is not the reviewed Base protocol identity.",
    )?;
    let solver = parse_address(field_string(order, &["solver"])?)?;
    ensure(
        solver == parse_address(RELAY_FILL_AUTHORITY)?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay order solver differs from the independently reviewed fill authority.",
    )?;
    let refunds = inputs[0]
        .get("refunds")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing refunds"))?;
    ensure(
        !refunds.is_empty() && refunds.len() <= 16,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Refund paths are missing or unbounded.",
    )?;
    for refund in refunds {
        let refund_chain = protocol_chain_to_id(config, field_string(refund, &["chainId"])?)?;
        let refund_currency = decode_evm_currency(field_string(refund, &["currency"])?)?;
        ensure(
            is_refund_asset_allowed(&request.intent, refund_chain, refund_currency),
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Relay refund asset is not allowed.",
        )?;
        ensure(
            decode_evm_currency(field_string(refund, &["recipient"])?)?
                == parse_address(&request.intent.refund_to)?,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Relay refund recipient differs from approved refund address.",
        )?;
        validate_router_extra_data(config, refund_chain, field_string(refund, &["extraData"])?)?;
    }
    ensure(
        canonical_order_id(order)? == stored_hash,
        "BRIDGE_ORDER_ID_MISMATCH",
        "quote",
        "Relay order ID does not match locally recomputed SDK protocol order hash.",
    )?;
    let route_id = fixed_scope_digest(&request.intent, config)?;
    let quote_id = format!("relay:{}:{}", order_id.to_ascii_lowercase(), route_id);
    let (approval, steps) =
        parse_transaction_steps(value, order, &request.intent, config, &gross, stored_hash)?;
    let fees = parse_order_fees(value, config)?;
    validate_fee_caps(&fees, &request.bounds)?;
    validate_minimum_ratio(
        source,
        destination,
        &gross,
        &output,
        request.bounds.minimum_output_input_ratio.as_ref(),
    )?;
    let refund_paths = parse_refund_paths(order, config, &request.intent)?;
    let quote = BridgeQuote {
        quote_id,
        provider: "relay".into(),
        route_id,
        intent: request.intent.clone(),
        gross_debit: source.amount(gross),
        expected_receipt: destination.amount(output),
        minimum_receipt: destination.amount(minimum_out),
        fees,
        approval,
        protocol_steps: steps.clone(),
        estimated_arrival_seconds: parse_arrival_estimate(
            value
                .get("details")
                .and_then(|details| details.get("timeEstimate")),
        )?,
        observed_at: now.to_rfc3339(),
        valid_until: valid_until.to_rfc3339(),
        class: QuoteClass::Executable,
        refund_paths,
        order_solver: Some(format!("{solver:#x}")),
        protocol_order_id: Some(order_id.into()),
        provider_request_id: value
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    Ok(quote)
}

fn parse_arrival_estimate(value: Option<&Value>) -> Result<Option<u64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let seconds = value
        .as_f64()
        .ok_or_else(|| invalid_order("arrival estimate must be a number of seconds"))?;
    ensure(
        seconds.is_finite()
            && seconds > 0.0
            && seconds <= MAX_RELAY_ARRIVAL_ESTIMATE_SECONDS as f64
            && seconds.fract() == 0.0,
        "BRIDGE_RESPONSE_INVALID",
        "quote",
        "Relay arrival estimate must be a finite positive whole number of seconds within seven days.",
    )?;
    Ok(Some(seconds as u64))
}

fn parse_transaction_steps(
    value: &Value,
    order: &Value,
    intent: &BridgeIntent,
    config: &ReviewedConfig,
    gross: &Quantity,
    order_id: alloy::primitives::B256,
) -> Result<(Option<ApprovalRequirement>, Vec<BridgeStep>)> {
    let raw_steps = value
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing executable source transaction steps"))?;
    let mut txs = Vec::new();
    for step in raw_steps {
        if step.get("kind").and_then(Value::as_str) == Some("transaction") {
            if let Some(items) = step.get("items").and_then(Value::as_array) {
                for item in items {
                    if item.get("status").and_then(Value::as_str) == Some("incomplete") {
                        if let Some(data) = item.get("data") {
                            txs.push(data);
                        }
                    }
                }
            }
        }
    }
    if intent.source.token.is_some() {
        ensure(
            txs.len() == 1 || txs.len() == 2,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "ERC-20 route must contain one deposit and at most one finite approval transaction.",
        )?;
    } else {
        ensure(
            txs.len() == 1,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Native route must contain exactly one deposit transaction.",
        )?;
    }
    let depository = parse_address(&config.chain(intent.source.chain_id)?.depository)?;
    let wallet = parse_address(&intent.source_address)?;
    let amount = parse_u256(gross)?;
    let mut out = Vec::new();
    let mut approval = None;
    if let Some(token_text) = &intent.source.token {
        let token = parse_address(token_text)?;
        if txs.len() == 2 {
            let tx = txs[0];
            ensure(
                parse_address(field_string(tx, &["from"])?)?
                    == parse_address(&intent.source_address)?,
                "BRIDGE_ORDER_INVALID",
                "quote",
                "Relay approval sender differs from the selected physical wallet.",
            )?;
            validate_tx_common(tx, intent.source.chain_id, token, U256::ZERO)?;
            let bytes = parse_calldata(field_string(tx, &["data"])?)?;
            let expected = IERC20Bridge::approveCall {
                spender: depository,
                amount,
            }
            .abi_encode();
            ensure(
                bytes == expected,
                "BRIDGE_ORDER_INVALID",
                "quote",
                "ERC-20 approval is not an exact finite approval to the reviewed Relay depository.",
            )?;
            approval = Some(ApprovalRequirement {
                token: token_text.to_ascii_lowercase(),
                spender: address_string(depository),
                amount: gross.clone(),
            });
            out.push(BridgeStep {
                ordinal: 0,
                kind: BridgeStepKind::Approval,
                call: EvmCall {
                    chain_id: intent.source.chain_id,
                    to: address_string(token),
                    value: Quantity::zero(),
                    data: hex_data(bytes),
                },
                expected_sender: field_string(tx, &["from"])?.to_ascii_lowercase(),
                maximum_debit: vec![],
                dependencies: vec![],
            });
        }
    }
    let deposit_tx = txs[txs.len() - 1];
    ensure(
        parse_address(field_string(deposit_tx, &["from"])?)?
            == parse_address(&intent.source_address)?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay deposit sender differs from the selected physical wallet.",
    )?;
    let deposit_value = if intent.source.token.is_some() {
        Quantity::zero()
    } else {
        gross.clone()
    };
    validate_tx_common(
        deposit_tx,
        intent.source.chain_id,
        depository,
        parse_u256(&deposit_value)?,
    )?;
    let bytes = parse_calldata(field_string(deposit_tx, &["data"])?)?;
    let expected = if intent.source.token.is_some() {
        IRelayDepository::depositErc20Call {
            depositor: wallet,
            token: parse_address(intent.source.token.as_deref().expect("token"))?,
            amount,
            id: order_id,
        }
        .abi_encode()
    } else {
        IRelayDepository::depositNativeCall {
            depositor: wallet,
            id: order_id,
        }
        .abi_encode()
    };
    ensure(
        bytes == expected,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay deposit transaction does not match the reviewed depository, payer, asset, amount and canonical order ID.",
    )?;
    let ordinal = out.len() as u8;
    out.push(BridgeStep {
        ordinal,
        kind: BridgeStepKind::Deposit,
        call: EvmCall {
            chain_id: intent.source.chain_id,
            to: address_string(depository),
            value: deposit_value,
            data: hex_data(bytes),
        },
        expected_sender: field_string(deposit_tx, &["from"])?.to_ascii_lowercase(),
        maximum_debit: vec![intent.source.amount(gross.clone())],
        dependencies: if ordinal > 0 { vec![0] } else { vec![] },
    });
    let _ = order;
    let _ = wallet;
    Ok((approval, out))
}
fn validate_tx_common(tx: &Value, chain: u64, to: Address, value: U256) -> Result<()> {
    ensure(
        field_u64(tx, &["chainId"])? == chain,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay transaction chain does not match the reviewed source chain.",
    )?;
    ensure(
        parse_address(field_string(tx, &["to"])?)? == to,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay transaction target is not the reviewed token or depository.",
    )?;
    let actual = field_string(tx, &["value"])?;
    let actual = parse_hex_or_decimal_u256(actual)?;
    ensure(
        actual == value,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay transaction value differs from the exact approved payment.",
    )
}
fn parse_calldata(s: &str) -> Result<Vec<u8>> {
    let s = s
        .strip_prefix("0x")
        .ok_or_else(|| invalid_order("calldata lacks hex prefix"))?;
    hex::decode(s).map_err(|_| invalid_order("calldata is malformed hex"))
}
fn parse_hex_or_decimal_u256(s: &str) -> Result<U256> {
    if let Some(hex) = s.strip_prefix("0x") {
        U256::from_str_radix(hex, 16).map_err(|_| invalid_order("transaction value invalid"))
    } else {
        U256::from_str(s).map_err(|_| invalid_order("transaction value invalid"))
    }
}
fn decode_evm_currency(v: &str) -> Result<Address> {
    let bytes = parse_calldata(v)?;
    ensure(
        bytes.len() == 20,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "EVM currency encoding must contain exactly 20 address bytes.",
    )?;
    Ok(Address::from_slice(&bytes))
}

fn protocol_chain_to_id(config: &ReviewedConfig, name: &str) -> Result<u64> {
    config
        .chains
        .iter()
        .find_map(|(id, c)| (c.protocol_chain_id == name).then_some(*id))
        .ok_or_else(|| invalid_order("protocol chain is not in reviewed configuration"))
}
fn parse_order_fees(value: &Value, config: &ReviewedConfig) -> Result<Vec<BridgeFee>> {
    let Some(entries) = value.get("fees").and_then(Value::as_object) else {
        return Err(invalid_order("missing fee schedule"));
    };
    let mut fees = Vec::new();
    for (name, fee) in entries {
        let Some(currency) = fee.get("currency") else {
            continue;
        };
        let chain = currency
            .get("chainId")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_order("fee chain missing"))?;
        let token = currency
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_order("fee currency missing"))?;
        let amount = fee
            .get("amount")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_order("fee amount missing"))?;
        let token_address = parse_address(token)?;
        let asset = if token_address == Address::ZERO {
            let decimals = currency
                .get("decimals")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid_order("native fee precision missing"))?;
            ensure(
                decimals == 18,
                "BRIDGE_ORDER_INVALID",
                "quote",
                "Native fee precision must be exactly 18 decimals.",
            )?;
            AssetId {
                network: format!("ethereum:{chain}"),
                reference: "native".into(),
                decimals: 18,
            }
        } else {
            let address = address_string(token_address);
            let reviewed = config
                .token_decimals(chain, &address)
                .ok_or_else(|| invalid_order("fee token absent from reviewed asset identities"))?;
            let decimals = currency
                .get("decimals")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid_order("fee token precision missing"))?;
            ensure(
                decimals == reviewed as u64,
                "BRIDGE_ORDER_INVALID",
                "quote",
                "Relay fee token precision differs from reviewed asset metadata.",
            )?;
            AssetId {
                network: format!("ethereum:{chain}"),
                reference: address,
                decimals: reviewed,
            }
        };
        if name == "relayerGas" && entries.contains_key("relayer")
            || name == "relayerService" && entries.contains_key("relayer")
        {
            continue;
        }
        let (kind, payer) = match name.as_str() {
            "gas" => (FeeKind::Gas, "sender"),
            "relayerGas" => (FeeKind::Gas, "relay"),
            "relayer" => (FeeKind::WithheldFromOutput, "relay"),
            "relayerService" => (FeeKind::WithheldFromOutput, "relay"),
            _ => (FeeKind::AdditionalDebit, "unknown"),
        };
        fees.push(BridgeFee {
            amount: Amount {
                asset,
                atoms: Quantity::new(amount)?,
            },
            kind,
            payer: payer.into(),
        });
    }
    Ok(fees)
}
fn validate_router_extra_data(config: &ReviewedConfig, chain: u64, extra: &str) -> Result<()> {
    if extra == "0x" {
        return Ok(());
    }
    let router = config
        .chain(chain)?
        .router_v3
        .as_deref()
        .ok_or_else(|| invalid_order("nonempty protocol extraData has no reviewed router"))?;
    let raw = parse_calldata(extra)?;
    let address = parse_address(router)?;
    ensure(
        raw.len() == 32 && raw[..12].iter().all(|b| *b == 0) && raw[12..] == address.as_slice()[..],
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Protocol extraData does not exactly encode the reviewed Relay router.",
    )
}
fn validate_minimum_ratio(
    source: &crate::EvmAsset,
    destination: &crate::EvmAsset,
    input: &Quantity,
    output: &Quantity,
    ratio: Option<&crate::RatioBound>,
) -> Result<()> {
    let Some(ratio) = ratio else { return Ok(()) };
    let input_scale = num_bigint::BigUint::from(10u8).pow(source.decimals as u32);
    let output_scale = num_bigint::BigUint::from(10u8).pow(destination.decimals as u32);
    let left = output.integer()
        * input_scale
        * num_bigint::BigUint::parse_bytes(ratio.denominator.as_str().as_bytes(), 10)
            .expect("validated quantity");
    let right = input.integer()
        * output_scale
        * num_bigint::BigUint::parse_bytes(ratio.numerator.as_str().as_bytes(), 10)
            .expect("validated quantity");
    ensure(
        left >= right,
        "BRIDGE_BOUNDS_VIOLATED",
        "quote",
        "Relay output is below the explicit output/input ratio bound.",
    )
}
fn validate_fee_caps(fees: &[BridgeFee], bounds: &BridgeBounds) -> Result<()> {
    let mut totals: BTreeMap<(AssetId, FeeKind), Quantity> = BTreeMap::new();
    for fee in fees {
        let key = (fee.amount.asset.clone(), fee.kind.clone());
        let current = totals.remove(&key).unwrap_or_else(Quantity::zero);
        totals.insert(key, current.checked_add(&fee.amount.atoms)?);
    }
    for ((asset, kind), amount) in totals {
        if let Some(cap) = bounds
            .fee_caps
            .iter()
            .find(|cap| cap.asset == asset && cap.kind == kind)
        {
            ensure(
                amount <= cap.maximum,
                "BRIDGE_BOUNDS_VIOLATED",
                "quote",
                "Aggregate Relay fees exceed the explicit asset-denominated cap.",
            )?;
        } else if !bounds.fee_caps.is_empty() {
            return Err(bridge_error(
                "BRIDGE_BOUNDS_VIOLATED",
                "quote",
                "Relay returned a fee without a matching explicit asset and fee-kind cap.",
                "Set an explicit cap for this fee or choose another quote.",
            ));
        }
    }
    Ok(())
}
fn parse_refund_paths(
    order: &Value,
    config: &ReviewedConfig,
    intent: &BridgeIntent,
) -> Result<Vec<RefundPath>> {
    let inputs = order
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing input refunds"))?;
    let mut paths = Vec::new();
    for input in inputs {
        for refund in input
            .get("refunds")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_order("missing refund paths"))?
        {
            let chain_id = protocol_chain_to_id(config, field_string(refund, &["chainId"])?)?;
            let currency = decode_evm_currency(field_string(refund, &["currency"])?)?;
            let allowed = intent
                .allowed_refund_assets
                .iter()
                .find(|a| {
                    a.chain_id == chain_id
                        && currency
                            == a.token
                                .as_deref()
                                .and_then(|v| parse_address(v).ok())
                                .unwrap_or(Address::ZERO)
                })
                .ok_or_else(|| invalid_order("refund asset not allowed"))?;
            let minimum = Quantity::new(field_string(refund, &["minimumAmount"])?)?;
            let maximum = inputs
                .first()
                .and_then(|i| i.pointer("/payment/amount"))
                .and_then(Value::as_str)
                .map(Quantity::new)
                .transpose()?
                .ok_or_else(|| invalid_order("missing source amount for refund bound"))?;
            ensure(
                minimum <= maximum,
                "BRIDGE_ORDER_INVALID",
                "quote",
                "Refund minimum exceeds the source amount at risk.",
            )?;
            let recipient = decode_evm_currency(field_string(refund, &["recipient"])?)?;
            ensure(
                recipient == parse_address(&intent.refund_to)?,
                "BRIDGE_ORDER_INVALID",
                "quote",
                "Refund recipient differs from approved refund address.",
            )?;
            paths.push(RefundPath {
                chain_id,
                asset: allowed.asset_id(),
                recipient: intent.refund_to.to_ascii_lowercase(),
                minimum,
                maximum,
            });
        }
    }
    ensure(
        !paths.is_empty() && paths.len() <= 16,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Refund paths are missing or unbounded.",
    )?;
    Ok(paths)
}
fn is_refund_asset_allowed(intent: &BridgeIntent, chain: u64, currency: Address) -> bool {
    intent.allowed_refund_assets.iter().any(|a| {
        a.chain_id == chain
            && currency
                == a.token
                    .as_deref()
                    .and_then(|v| parse_address(v).ok())
                    .unwrap_or(Address::ZERO)
    })
}

fn canonical_order_id(order: &Value) -> Result<alloy::primitives::B256> {
    order::hash_order(order)
}
fn field_string<'a>(value: &'a Value, names: &[&str]) -> Result<&'a str> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .ok_or_else(|| invalid_order("required string field missing"))
}
fn field_u64(value: &Value, names: &[&str]) -> Result<u64> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_u64))
        .ok_or_else(|| invalid_order("required integer field missing"))
}
fn invalid_order(reason: &str) -> fin_domain::FinError {
    bridge_error(
        "BRIDGE_ORDER_INVALID",
        "quote",
        format!("Relay protocol order is invalid: {reason}."),
        "Request a new quote; Fin will not execute unverifiable order data.",
    )
}
fn missing_protocol() -> fin_domain::FinError {
    bridge_error(
        "BRIDGE_PROTOCOL_DATA_MISSING",
        "quote",
        "Relay quote omitted required protocol.v2 order data.",
        "Request another quote. Fin cannot execute the legacy deposit format.",
    )
}
fn parse_address(value: &str) -> Result<Address> {
    Address::from_str(value).map_err(|_| {
        bridge_error(
            "BRIDGE_ORDER_INVALID",
            "validation",
            "Expected a canonical EVM address.",
            "Choose a valid address on the selected network.",
        )
    })
}
fn parse_b256(value: &str) -> Result<alloy::primitives::B256> {
    alloy::primitives::B256::from_str(value)
        .map_err(|_| invalid_order("order id is not a 32-byte hash"))
}
fn parse_u256(value: &Quantity) -> Result<U256> {
    U256::from_str(value.as_str()).map_err(|_| invalid_order("quantity exceeds uint256"))
}
fn currency(asset: &crate::EvmAsset) -> String {
    asset
        .token
        .clone()
        .unwrap_or_else(|| "0x0000000000000000000000000000000000000000".into())
}
fn address_string(address: Address) -> String {
    format!("{address:#x}").to_ascii_lowercase()
}
fn hex_data(bytes: Vec<u8>) -> String {
    format!("0x{}", hex::encode(bytes))
}
fn fixed_scope_digest(intent: &BridgeIntent, config: &ReviewedConfig) -> Result<String> {
    let fixed = json!({"provider":"relay","wallet":intent.wallet,"source_address":intent.source_address.to_ascii_lowercase(),"source":intent.source,"destination_chain_id":intent.destination_chain_id,"destination_assets":intent.destination_assets,"recipient":intent.recipient.to_ascii_lowercase(),"refund_to":intent.refund_to.to_ascii_lowercase(),"allowed_refund_assets":intent.allowed_refund_assets,"reviewed_source_depository":config.chain(intent.source.chain_id)?.depository.to_ascii_lowercase(),"reviewed_destination_depository":config.chain(intent.destination_chain_id)?.depository.to_ascii_lowercase(),"source_router_v3":config.chain(intent.source.chain_id)?.router_v3,"destination_router_v3":config.chain(intent.destination_chain_id)?.router_v3});
    let encoded = serde_json::to_vec(&fixed)
        .map_err(|_| invalid_order("fixed scope cannot be serialized"))?;
    Ok(format!("0x{}", hex::encode(keccak256(encoded))))
}
fn parse_evidence_candidates(
    value: &Value,
    transfer: &BridgeTransferRef,
) -> Result<BridgeReconciliation> {
    if let Some(request_id) = value.get("requestId") {
        let request_id = request_id
            .as_str()
            .ok_or_else(|| invalid_status("requestId must be a string"))?;
        ensure(
            transfer
                .provider_request_id
                .as_deref()
                .is_some_and(|saved| request_id.eq_ignore_ascii_case(saved)),
            "BRIDGE_RESPONSE_INVALID",
            "reconcile",
            "Relay status response belongs to a different provider request ID.",
        )?;
    }
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_status("missing status"))?;
    let status = status.to_ascii_lowercase();
    ensure(
        matches!(
            status.as_str(),
            "waiting"
                | "received"
                | "depositing"
                | "pending"
                | "submitted"
                | "success"
                | "delayed"
                | "refunded"
                | "refund"
                | "fallback"
                | "failure"
                | "failed"
        ),
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        "Relay status response contains an unknown status value.",
    )?;
    let origin = value
        .get("originChainId")
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid_status("malformed originChainId"))
        })
        .transpose()?
        .unwrap_or(transfer.source_chain_id);
    let destination = value
        .get("destinationChainId")
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid_status("malformed destinationChainId"))
        })
        .transpose()?
        .unwrap_or(transfer.destination_chain_id);
    ensure(
        origin == transfer.source_chain_id && destination == transfer.destination_chain_id,
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        "Relay status chain identities differ from the saved transfer.",
    )?;

    let mut candidates = Vec::new();
    append_hash_candidates(
        &mut candidates,
        value.get("inTxHashes"),
        transfer,
        EvidenceKind::SourceDeposit,
        origin,
    )?;
    let refund_status = matches!(status.as_str(), "refund" | "refunded" | "fallback");
    if refund_status {
        // Refund/fallback may settle on either reviewed side. The broker tries
        // each saved path; no API status or hash is itself a financial proof.
        for chain_id in [origin, destination] {
            append_hash_candidates(
                &mut candidates,
                value.get("outTxHashes"),
                transfer,
                EvidenceKind::Refund,
                chain_id,
            )?;
            append_hash_candidates(
                &mut candidates,
                value.get("txHashes"),
                transfer,
                EvidenceKind::Refund,
                chain_id,
            )?;
        }
    } else {
        append_hash_candidates(
            &mut candidates,
            value.get("outTxHashes"),
            transfer,
            EvidenceKind::DestinationFill,
            destination,
        )?;
        append_hash_candidates(
            &mut candidates,
            value.get("txHashes"),
            transfer,
            EvidenceKind::DestinationFill,
            destination,
        )?;
    }
    candidates.sort_by(|a, b| {
        (&a.kind, a.chain_id, &a.transaction_hash).cmp(&(&b.kind, b.chain_id, &b.transaction_hash))
    });
    candidates.dedup_by(|a, b| {
        a.kind == b.kind && a.chain_id == b.chain_id && a.transaction_hash == b.transaction_hash
    });
    Ok(BridgeReconciliation {
        status,
        failure: value.get("failure").cloned(),
        refund_fail_reason: value.get("refundFailReason").cloned(),
        candidates,
    })
}

fn append_hash_candidates(
    out: &mut Vec<BridgeEvidenceCandidate>,
    hashes: Option<&Value>,
    transfer: &BridgeTransferRef,
    kind: EvidenceKind,
    chain_id: u64,
) -> Result<()> {
    let Some(hashes) = hashes else {
        return Ok(());
    };
    let hashes = hashes
        .as_array()
        .ok_or_else(|| invalid_status("transaction hashes must be an array"))?;
    let allowed_chain = match kind {
        EvidenceKind::SourceDeposit => chain_id == transfer.source_chain_id,
        EvidenceKind::DestinationFill => chain_id == transfer.destination_chain_id,
        EvidenceKind::Refund => {
            chain_id == transfer.source_chain_id || chain_id == transfer.destination_chain_id
        }
    };
    ensure(
        allowed_chain,
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        "Relay status candidate uses an unknown chain.",
    )?;
    for hash in hashes {
        let tx = hash
            .as_str()
            .ok_or_else(|| invalid_status("transaction hash must be a string"))?;
        if tx.len() != 66
            || !tx.starts_with("0x")
            || !tx[2..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid_status("malformed transaction hash"));
        }
        out.push(BridgeEvidenceCandidate {
            order_id: transfer.order_id.clone(),
            kind: kind.clone(),
            chain_id,
            transaction_hash: tx.to_ascii_lowercase(),
            correlation: None,
        });
    }
    Ok(())
}

fn invalid_status(reason: &str) -> fin_domain::FinError {
    bridge_error(
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        format!("Relay status response is invalid: {reason}."),
        "Retry status; do not treat provider status as settlement proof.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn config() -> ReviewedConfig {
        let mut chains = BTreeMap::new();
        for (chain_id, protocol_chain_id) in
            [(1, "ethereum"), (42161, "arbitrum"), (4663, "robinhood")]
        {
            chains.insert(
                chain_id,
                ReviewedChain {
                    chain_id,
                    protocol_chain_id: protocol_chain_id.into(),
                    depository: "0x4cd00e387622c35bddb9b4c962c136462338bc31".into(),
                    router_v3: Some("0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f".into()),
                    protocol: "relay-v2".into(),
                },
            );
        }
        ReviewedConfig {
            chains,
            tokens: BTreeSet::new(),
            relay_solver_protocol_chain_id: "base".into(),
            settlement_hub_chain_id: 537713,
            settlement_hub_address: "0xDDD361727C22A01EB137880678A20b0BEaE69318".into(),
        }
    }
    fn transfer() -> BridgeTransferRef {
        BridgeTransferRef {
            order_id: Some(format!("0x{}", "11".repeat(32))),
            provider_request_id: Some(format!("0x{}", "22".repeat(32))),
            source_chain_id: 1,
            destination_chain_id: 42161,
            destination_locator: None,
        }
    }
    async fn fixture(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = vec![0; 16384];
            let n = stream.read(&mut data).await.unwrap();
            let req = String::from_utf8_lossy(&data[..n]).to_string();
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            req
        });
        (format!("http://{addr}"), task)
    }
    #[tokio::test]
    async fn loopback_retirement_is_actionable_and_request_has_no_key() {
        let (base, task) = fixture(404, "{}").await;
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let provider = RelayProvider {
            client,
            config: config(),
            api_base: base,
        };
        let intent = BridgeIntent {
            wallet: fin_domain::ResourceId("evm:wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            },
            destination_chain_id: 42161,
            destination_assets: vec![crate::EvmAsset {
                chain_id: 42161,
                token: None,
                decimals: 18,
            }],
            recipient: "0x2222222222222222222222222222222222222222".into(),
            refund_to: "0x1111111111111111111111111111111111111111".into(),
            allowed_refund_assets: vec![crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }],
            amount: Quantity::from_u64(1),
        };
        let err = provider
            .quote_http(&BridgeQuoteRequest {
                intent,
                bounds: BridgeBounds::default(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, "BRIDGE_PROVIDER_UNAVAILABLE");
        assert!(err.message.contains("retired"));
        let request = task.await.unwrap().to_ascii_lowercase();
        assert!(!request.contains("x-api-key"));
        assert!(request.contains("includeprotocoldata"));
        assert!(request.contains("explicitdeposit"));
        assert!(request.contains("usepermit"));
        assert!(request.contains("usedepositaddress"));
        assert!(request.contains("disableoriginswaps"));
    }

    #[tokio::test]
    async fn status_http_uses_saved_request_id_and_returns_only_hash_candidates() {
        let body = r#"{"status":"success","requestId":"0x2222222222222222222222222222222222222222222222222222222222222222","inTxHashes":["0x3333333333333333333333333333333333333333333333333333333333333333"],"txHashes":["0x4444444444444444444444444444444444444444444444444444444444444444"],"outTxHashes":[],"originChainId":1,"destinationChainId":42161}"#;
        let (base, task) = fixture(200, body).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let provider = RelayProvider {
            client,
            config: config(),
            api_base: base,
        };
        let result = provider.reconcile(&transfer()).await.unwrap();
        assert_eq!(result.status, "success");
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].kind, EvidenceKind::SourceDeposit);
        assert_eq!(result.candidates[0].chain_id, 1);
        assert_eq!(result.candidates[1].kind, EvidenceKind::DestinationFill);
        assert_eq!(result.candidates[1].chain_id, 42161);
        let request = task.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("/intents/status?requestid=0x2222"));
        assert!(!request.contains("x-api-key"));
    }

    #[test]
    fn refund_status_preserves_reasons_and_unknown_status_data_is_rejected() {
        let transfer = transfer();
        let raw = json!({
            "status": "fallback",
            "requestId": transfer.provider_request_id,
            "originChainId": 1,
            "destinationChainId": 42161,
            "inTxHashes": [],
            "txHashes": [format!("0x{}", "44".repeat(32))],
            "failure": {"code": "route_failed"},
            "refundFailReason": "waiting for refund"
        });
        let status = parse_evidence_candidates(&raw, &transfer).unwrap();
        assert_eq!(status.status, "fallback");
        assert_eq!(status.failure.unwrap()["code"], "route_failed");
        assert_eq!(status.refund_fail_reason.unwrap(), "waiting for refund");
        assert_eq!(status.candidates.len(), 2);
        assert!(
            status
                .candidates
                .iter()
                .all(|candidate| candidate.kind == EvidenceKind::Refund)
        );
        assert_eq!(status.candidates[0].chain_id, 1);
        assert_eq!(status.candidates[1].chain_id, 42161);

        let bad_status = json!({
            "status": "completed_successfully",
            "originChainId": 1,
            "destinationChainId": 42161
        });
        assert!(parse_evidence_candidates(&bad_status, &transfer).is_err());
    }

    #[test]
    fn status_rejects_wrong_request_chain_and_transaction_hashes() {
        let transfer = transfer();
        let base = json!({
            "status": "success",
            "originChainId": 1,
            "destinationChainId": 42161,
            "inTxHashes": [],
            "txHashes": []
        });
        let mut wrong_id = base.clone();
        wrong_id["requestId"] = json!(format!("0x{}", "99".repeat(32)));
        assert!(parse_evidence_candidates(&wrong_id, &transfer).is_err());

        let mut unknown_chain = base.clone();
        unknown_chain["originChainId"] = json!(8453);
        assert!(parse_evidence_candidates(&unknown_chain, &transfer).is_err());

        let mut malformed_hash = base;
        malformed_hash["inTxHashes"] = json!(["0x1234"]);
        assert!(parse_evidence_candidates(&malformed_hash, &transfer).is_err());

        let mut chainless = json!({"status":"success","txHashes":[]});
        assert!(parse_evidence_candidates(&chainless, &transfer).is_ok());
        chainless["destinationChainId"] = json!(8453);
        assert!(parse_evidence_candidates(&chainless, &transfer).is_err());
    }

    #[test]
    fn anonymous_protocol_v2_fixture_parses_and_binds_both_ids() {
        let config = config();
        let raw: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anonymous-synthetic-quote.json"
        ))
        .unwrap();
        let intent = BridgeIntent {
            wallet: fin_domain::ResourceId("evm:wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            },
            destination_chain_id: 42161,
            destination_assets: vec![crate::EvmAsset {
                chain_id: 42161,
                token: None,
                decimals: 18,
            }],
            recipient: "0x1111111111111111111111111111111111111111".into(),
            refund_to: "0x1111111111111111111111111111111111111111".into(),
            allowed_refund_assets: vec![
                crate::EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                },
                crate::EvmAsset {
                    chain_id: 42161,
                    token: None,
                    decimals: 18,
                },
            ],
            amount: Quantity::new("3000000000000000").unwrap(),
        };
        let fixture_now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let request = BridgeQuoteRequest {
            intent,
            bounds: BridgeBounds::default(),
        };
        let quote = parse_order_response_at(&raw, &request, &config, fixture_now).unwrap();
        assert_eq!(
            quote.protocol_order_id.as_deref(),
            Some("0x9141e5b4e6ff9e14342a532db61a93909fb346727191fabe2f8572ea6e965317")
        );
        assert_eq!(
            quote.provider_request_id.as_deref(),
            Some("0x5555555555555555555555555555555555555555555555555555555555555555")
        );
        assert_eq!(quote.protocol_steps.len(), 1);
        assert_eq!(quote.estimated_arrival_seconds, Some(1));
        let protocol_deadline = DateTime::parse_from_rfc3339(&quote.valid_until)
            .unwrap()
            .timestamp() as u64;
        assert_eq!(quote.protocol_steps[0].kind, BridgeStepKind::Deposit);
        assert_eq!(
            quote.protocol_steps[0].call.value.as_str(),
            "3000000000000000"
        );
        let provider = RelayProvider::new(config.clone()).unwrap();
        let prepared = provider
            .prepare(
                &quote,
                &BridgeBounds::default(),
                "0x1111111111111111111111111111111111111111",
            )
            .unwrap();
        assert_eq!(prepared.steps.len(), 1);
        let mut exact_deadline = BridgeBounds::default();
        exact_deadline.latest_protocol_deadline = Some(protocol_deadline);
        assert!(
            provider
                .prepare(&quote, &exact_deadline, &prepared.steps[0].expected_sender)
                .is_ok()
        );
        let mut past_cap = BridgeBounds::default();
        past_cap.latest_protocol_deadline = Some(protocol_deadline - 1);
        assert!(
            provider
                .prepare(&quote, &past_cap, &prepared.steps[0].expected_sender)
                .is_err()
        );

        let mut at_cap_request = request.clone();
        at_cap_request.bounds.latest_protocol_deadline = Some(protocol_deadline);
        assert!(parse_order_response_at(&raw, &at_cap_request, &config, fixture_now).is_ok());
        at_cap_request.bounds.latest_protocol_deadline = Some(protocol_deadline - 1);
        assert!(parse_order_response_at(&raw, &at_cap_request, &config, fixture_now).is_err());
        at_cap_request.bounds.latest_protocol_deadline = Some(0);
        assert!(parse_order_response_at(&raw, &at_cap_request, &config, fixture_now).is_err());
        at_cap_request.bounds.latest_protocol_deadline = Some(u64::MAX);
        assert!(parse_order_response_at(&raw, &at_cap_request, &config, fixture_now).is_err());

        let mut missing_estimate = raw.clone();
        missing_estimate["details"]
            .as_object_mut()
            .unwrap()
            .remove("timeEstimate");
        let without_estimate =
            parse_order_response_at(&missing_estimate, &request, &config, fixture_now).unwrap();
        assert_eq!(without_estimate.estimated_arrival_seconds, None);

        for invalid_estimate in [json!("1"), json!(-1), json!(1.5), json!(604801)] {
            let mut malformed = raw.clone();
            malformed["details"]["timeEstimate"] = invalid_estimate;
            assert!(parse_order_response_at(&malformed, &request, &config, fixture_now).is_err());
        }
        let mut altered = quote.clone();
        altered.protocol_steps[0].call.value = Quantity::new("3000000000000001").unwrap();
        assert!(
            provider
                .prepare(
                    &altered,
                    &BridgeBounds::default(),
                    "0x1111111111111111111111111111111111111111"
                )
                .is_err()
        );
    }

    #[test]
    fn protocol_parser_rejects_tampered_identity_debit_refund_and_missing_data() {
        let config = config();
        let base: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anonymous-synthetic-quote.json"
        ))
        .unwrap();
        let intent = BridgeIntent {
            wallet: fin_domain::ResourceId("evm:wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            },
            destination_chain_id: 42161,
            destination_assets: vec![crate::EvmAsset {
                chain_id: 42161,
                token: None,
                decimals: 18,
            }],
            recipient: "0x1111111111111111111111111111111111111111".into(),
            refund_to: "0x1111111111111111111111111111111111111111".into(),
            allowed_refund_assets: vec![
                crate::EvmAsset {
                    chain_id: 1,
                    token: None,
                    decimals: 18,
                },
                crate::EvmAsset {
                    chain_id: 42161,
                    token: None,
                    decimals: 18,
                },
            ],
            amount: Quantity::new("3000000000000000").unwrap(),
        };
        let request = BridgeQuoteRequest {
            intent,
            bounds: BridgeBounds::default(),
        };
        let mut cases = Vec::new();
        let mut v = base.clone();
        v["protocol"]["v2"]["orderData"]["output"]["payments"][0]["recipient"] =
            "0x2222222222222222222222222222222222222222".into();
        cases.push(v);
        let mut v = base.clone();
        v["protocol"]["v2"]["orderData"]["inputs"][0]["payment"]["chainId"] = "arbitrum".into();
        cases.push(v);
        let mut v = base.clone();
        v["protocol"]["v2"]["orderData"]["inputs"][0]["payment"]["amount"] =
            "3000000000000001".into();
        cases.push(v);
        let mut v = base.clone();
        v["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["currency"] =
            "0x1111111111111111111111111111111111111111".into();
        cases.push(v);
        let mut v = base.clone();
        v["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["minimumAmount"] =
            "4000000000000000".into();
        cases.push(v);
        let mut v = base.clone();
        v["steps"][0]["items"][0]["data"]["to"] =
            "0x0000000000000000000000000000000000000001".into();
        cases.push(v);
        let mut v = base.clone();
        v["protocol"]["v2"]["orderData"]["solver"] =
            "0x0000000000000000000000000000000000000001".into();
        cases.push(v);
        let mut v = base.clone();
        v.as_object_mut().unwrap().remove("protocol");
        cases.push(v);
        let fixture_now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        for tampered in cases {
            assert!(parse_order_response_at(&tampered, &request, &config, fixture_now).is_err());
        }
    }
    #[test]
    fn reviewed_config_rejects_wrong_hub_and_base_spending_route() {
        let mut cfg = config();
        cfg.settlement_hub_address = "0x0000000000000000000000000000000000000001".into();
        assert!(cfg.validate().is_err());
        let cfg = config();
        let intent = BridgeIntent {
            wallet: fin_domain::ResourceId("evm:w".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            },
            destination_chain_id: 8453,
            destination_assets: vec![crate::EvmAsset {
                chain_id: 8453,
                token: None,
                decimals: 18,
            }],
            recipient: "0x2222222222222222222222222222222222222222".into(),
            refund_to: "0x1111111111111111111111111111111111111111".into(),
            allowed_refund_assets: vec![crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }],
            amount: Quantity::from_u64(1),
        };
        assert!(validate_intent(&cfg, &intent, &BridgeBounds::default()).is_err());
    }

    #[test]
    fn aggregate_fees_are_checked_against_one_asset_kind_cap() {
        let asset = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let fees = vec![
            BridgeFee {
                amount: Amount {
                    asset: asset.clone(),
                    atoms: Quantity::from_u64(4),
                },
                kind: FeeKind::Gas,
                payer: "sender".into(),
            },
            BridgeFee {
                amount: Amount {
                    asset: asset.clone(),
                    atoms: Quantity::from_u64(7),
                },
                kind: FeeKind::Gas,
                payer: "sender".into(),
            },
        ];
        let bounds = BridgeBounds {
            fee_caps: vec![crate::FeeCap {
                asset,
                kind: FeeKind::Gas,
                maximum: Quantity::from_u64(10),
                maximum_usd_micros: None,
            }],
            ..Default::default()
        };
        assert!(validate_fee_caps(&fees, &bounds).is_err());
    }
    #[test]
    fn decimal_adjusted_ratio_bounds_are_enforced() {
        let source = crate::EvmAsset {
            chain_id: 1,
            token: None,
            decimals: 18,
        };
        let dest = crate::EvmAsset {
            chain_id: 42161,
            token: None,
            decimals: 6,
        };
        let ratio = crate::RatioBound {
            numerator: Quantity::new("1").unwrap(),
            denominator: Quantity::new("1").unwrap(),
        };
        assert!(
            validate_minimum_ratio(
                &source,
                &dest,
                &Quantity::new("1000000000000000000").unwrap(),
                &Quantity::new("999999").unwrap(),
                Some(&ratio)
            )
            .is_err()
        );
        assert!(
            validate_minimum_ratio(
                &source,
                &dest,
                &Quantity::new("1000000000000000000").unwrap(),
                &Quantity::new("1000000").unwrap(),
                Some(&ratio)
            )
            .is_ok()
        );
    }
}
