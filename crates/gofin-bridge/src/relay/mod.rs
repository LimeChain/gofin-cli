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
pub mod solana;
use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use gofin_domain::{Amount, AssetId, Quantity, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    time::Duration,
};
use zeroize::Zeroizing;

sol! {
    interface IERC20Bridge { function approve(address spender, uint256 amount) external returns (bool); }
    interface IRelayDepository { function depositNative(address depositor, bytes32 id) external payable; function depositErc20(address depositor,address token,uint256 amount,bytes32 id) external; }
}

const RELAY_API: &str = "https://api.relay.link";
pub const RELAY_SOLANA_CHAIN_ID: u64 = 792_703_809;
pub const RELAY_NATIVE_SOL_CURRENCY: &str = "11111111111111111111111111111111";
pub const RELAY_SOLANA_PROTOCOL_CHAIN_ID: &str = "solana";
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
    /// Asset precision is resolved by the protected broker, independently of
    /// provider metadata. Keep this exact request-local scope out of the
    /// deployment registry; accepting an asset never reviews a new spender.
    fn with_intent_assets(&self, intent: &BridgeIntent) -> Result<Self> {
        let mut config = self.clone();
        for asset in intent
            .source
            .iter()
            .chain(&intent.destination_assets)
            .chain(&intent.allowed_refund_assets)
        {
            config.chain(asset.chain_id)?;
            if let Some(token) = &asset.token {
                let address = parse_address(token)?;
                ensure(
                    address != Address::ZERO,
                    "BRIDGE_ASSET_INVALID",
                    "validation",
                    "ERC-20 asset identities must be nonzero.",
                )?;
                let address = address_string(address);
                if let Some(existing) = config.token_decimals(asset.chain_id, &address) {
                    ensure(
                        existing == asset.decimals,
                        "BRIDGE_ASSET_INVALID",
                        "validation",
                        "Exact token identity has conflicting precision.",
                    )?;
                }
                config.tokens.insert(ReviewedTokenKey {
                    chain_id: asset.chain_id,
                    address,
                    decimals: asset.decimals,
                });
            }
        }
        Ok(config)
    }
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
                "Choose a reviewed chain or update GoFin's reviewed deployment configuration.",
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
    api_key: Option<Zeroizing<String>>,
}
impl RelayProvider {
    pub fn new(config: ReviewedConfig) -> Result<Self> {
        config.validate()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("GoFin/bridge")
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
            api_key: None,
        })
    }
    /// The broker supplies a protected Relay service credential. It is never
    /// part of bridge input, quote output, or execution authority.
    pub fn with_api_key(config: ReviewedConfig, api_key: Zeroizing<String>) -> Result<Self> {
        ensure(
            !api_key.trim().is_empty() && api_key.len() <= 4096,
            "INVALID_INPUT",
            "validation",
            "Relay API key is empty or too long.",
        )?;
        let mut provider = Self::new(config)?;
        provider.api_key = Some(api_key);
        Ok(provider)
    }
    async fn quote_http(&self, request: &BridgeQuoteRequest) -> Result<Value> {
        let intent = &request.intent;
        let (origin_chain, source_currency) = match (&intent.source, &intent.solana_source) {
            (Some(source), None) => (source.chain_id, currency(source)),
            (None, Some(source)) if source.network == "solana:mainnet-beta" => (
                RELAY_SOLANA_CHAIN_ID,
                source
                    .mint
                    .clone()
                    .unwrap_or_else(|| RELAY_NATIVE_SOL_CURRENCY.into()),
            ),
            _ => {
                return Err(gofin_domain::GoFinError::validation(
                    "Select exactly one bridge source asset.",
                ));
            }
        };
        let (destination_chain, destination_currency) =
            if let Some(destination) = &intent.solana_destination {
                ensure(
                    intent.destination_chain_id.is_none() && intent.destination_assets.is_empty(),
                    "INVALID_INPUT",
                    "quote",
                    "Select exactly one bridge destination network.",
                )?;
                ensure(
                    destination.network == "solana:mainnet-beta",
                    "INVALID_INPUT",
                    "quote",
                    "Relay supports Solana mainnet-beta only.",
                )?;
                (
                    RELAY_SOLANA_CHAIN_ID,
                    if destination.asset.reference == "native" {
                        RELAY_NATIVE_SOL_CURRENCY.into()
                    } else {
                        destination.asset.reference.clone()
                    },
                )
            } else {
                let dest = intent.destination_assets.first().ok_or_else(|| {
                    bridge_error(
                        "BRIDGE_ROUTE_UNAVAILABLE",
                        "quote",
                        "At least one exact destination asset is required.",
                        "Select a supported destination token.",
                    )
                })?;
                (intent.evm_destination_chain_id()?, currency(dest))
            };
        ensure(
            origin_chain != destination_chain,
            "BRIDGE_ROUTE_UNAVAILABLE",
            "quote",
            "Bridge chains must differ.",
        )?;
        let body = json!({
            "user": intent.source_address,
            "originChainId": origin_chain,
            "destinationChainId": destination_chain,
            "originCurrency": source_currency,
            "destinationCurrency": destination_currency,
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
        let mut quote_request = self.client.post(format!("{}/quote", self.api_base));
        if let Some(key) = self.api_key.as_deref() {
            quote_request = quote_request.header("x-api-key", key);
        }
        let response = quote_request.json(&body).send().await.map_err(|_| {
            bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "quote",
                "Relay's quote service could not be reached.",
                "Retry later or choose another supported route.",
            )
        })?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "quote",
                "Relay rejected the quote request.",
                "Check the Relay connection if one is enrolled, then retry.",
            ));
        }
        if response.status() == reqwest::StatusCode::GONE
            || response.status() == reqwest::StatusCode::NOT_FOUND
        {
            return Err(bridge_error(
                "BRIDGE_PROVIDER_UNAVAILABLE",
                "quote",
                "Relay's quote endpoint is unavailable.",
                "Retry later or choose another supported route.",
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
    async fn discover_routes(
        &self,
        request: &crate::BridgeRouteRequest,
    ) -> Result<crate::BridgeRouteDiscovery> {
        ensure(
            request.source.is_some() != request.solana_source.is_some(),
            "INVALID_INPUT",
            "discovery",
            "Select exactly one bridge source asset.",
        )?;
        if let Some(source) = &request.source {
            self.config.chain(source.chain_id)?;
        }
        if let Some(source) = &request.solana_source {
            ensure(
                source.network == "solana:mainnet-beta",
                "BRIDGE_ROUTE_UNAVAILABLE",
                "discovery",
                "Relay supports Solana mainnet-beta only.",
            )?;
        }
        if request.solana_destination.is_none() {
            self.config.chain(request.destination_chain_id)?;
        }
        ensure(
            request
                .source
                .as_ref()
                .map_or(RELAY_SOLANA_CHAIN_ID, |s| s.chain_id)
                != request.destination_chain_id,
            "BRIDGE_ROUTE_UNAVAILABLE",
            "discovery",
            "Bridge chains must differ.",
        )?;
        let mut result = crate::BridgeRouteDiscovery {
            provider: "relay".into(),
            availability: crate::RouteAvailability::Candidate,
            candidates: vec![],
            observed_at: gofin_domain::now(),
            evidence_source: "Relay /currencies/v2 related currency candidates".into(),
            complete: false,
            limitations: vec!["Indicative only: exact amount, liquidity, pricing, quote calldata and financial proof compatibility are checked by bridge.quotes and execution.".into(), "Currency discovery is partial; an explicit exact destination asset can be quoted even if absent from this list.".into()],
            error: None,
        };
        if let Some(destination) = &request.solana_destination {
            ensure(
                request.destination_chain_id == RELAY_SOLANA_CHAIN_ID
                    && destination.network == "solana:mainnet-beta"
                    && request.destination_asset.is_none(),
                "INVALID_INPUT",
                "discovery",
                "Select one exact Solana destination.",
            )?;
            result.evidence_source =
                "Explicit Solana mint identity; executable Relay quote required".into();
            result.candidates.push(crate::BridgeRouteCandidate {
                destination: None,
                solana_destination: Some(crate::SolanaAssetInput {
                    network: destination.network.clone(),
                    mint: destination.mint.clone(),
                }),
                reported_decimals: None,
            });
            return Ok(result);
        }
        if let Some(asset) = &request.destination_asset {
            ensure(
                asset.chain_id == request.destination_chain_id,
                "BRIDGE_ASSET_INVALID",
                "discovery",
                "Destination asset is on a different chain.",
            )?;
            result.evidence_source =
                "Explicit destination identity; provider feasibility requires an executable quote"
                    .into();
            result.candidates.push(crate::BridgeRouteCandidate {
                destination: Some(crate::BridgeAssetInput {
                    chain_id: asset.chain_id,
                    token: asset.token.clone(),
                }),
                solana_destination: None,
                reported_decimals: Some(asset.decimals),
            });
            return Ok(result);
        }
        if request.solana_source.is_some() {
            result.availability = crate::RouteAvailability::Unknown;
            result.evidence_source = "Relay Solana currency candidates are incomplete".into();
            result
                .limitations
                .push("Supply an exact EVM destination asset for a quote.".into());
            return Ok(result);
        }
        let Some(source_token) = &request.evm_source()?.token else {
            result.evidence_source =
                "Reviewed Relay native ETH chain profiles; executable quote required".into();
            result.candidates.push(crate::BridgeRouteCandidate {
                destination: Some(crate::BridgeAssetInput {
                    chain_id: request.destination_chain_id,
                    token: None,
                }),
                solana_destination: None,
                reported_decimals: Some(18),
            });
            return Ok(result);
        };
        let response = self.client.post(format!("{}/currencies/v2", self.api_base))
            .json(&json!({"chainIds":[request.evm_source()?.chain_id],"address":source_token,"includeAllChains":true,"limit":100,"defaultList":false}))
            .send().await.map_err(|_| bridge_error("BRIDGE_PROVIDER_UNAVAILABLE", "discovery", "Relay currency discovery could not be reached.", "Retry discovery or supply an exact destination asset for a quote."))?;
        ensure(
            response.status().is_success(),
            "BRIDGE_PROVIDER_UNAVAILABLE",
            "discovery",
            "Relay currency discovery did not return a successful response.",
        )?;
        let raw = bounded_provider_json(response).await?;
        result.candidates = parse_currency_candidates(&raw, request)?;
        if result.candidates.is_empty() {
            result.availability = crate::RouteAvailability::Unknown;
        }
        Ok(result)
    }
    fn capabilities(&self) -> BridgeCapabilities {
        BridgeCapabilities {
            provider: "relay".into(),
            chains: self.config.chains.keys().copied().collect(),
            native_eth: true,
            erc20: true,
            protocol_versions: ["protocol.v2".to_owned()].into_iter().collect(),
        }
    }
    async fn quote(&self, request: &BridgeQuoteRequest) -> Result<BridgeQuoteBatch> {
        let config = self.config.with_intent_assets(&request.intent)?;
        let cross_vm =
            request.intent.solana_source.is_some() || request.intent.solana_destination.is_some();
        if cross_vm {
            validate_cross_vm_intent(&config, &request.intent, &request.bounds)?;
        } else {
            validate_intent(&config, &request.intent, &request.bounds)?;
        }
        let mut batch = BridgeQuoteBatch {
            quotes: Vec::new(),
            failures: Vec::new(),
        };
        if let Some(destination) = &request.intent.solana_destination {
            let raw = self.quote_http(request).await;
            match raw.and_then(|raw| parse_cross_vm_order_response(&raw, request, &config)) {
                Ok(quote) => batch.quotes.push(quote),
                Err(error) => batch.failures.push(BridgeQuoteFailure {
                    provider: Some("relay".into()),
                    destination_asset: None,
                    solana_destination_asset: Some(crate::SolanaAsset {
                        network: destination.network.clone(),
                        mint: (destination.asset.reference != "native")
                            .then(|| destination.asset.reference.clone()),
                        decimals: destination.asset.decimals,
                    }),
                    error,
                }),
            }
            return Ok(batch);
        }
        for asset in &request.intent.destination_assets {
            let mut one = request.clone();
            one.intent.destination_assets = vec![asset.clone()];
            match self.quote_http(&one).await.and_then(|raw| {
                if cross_vm {
                    parse_cross_vm_order_response(&raw, &one, &config)
                } else {
                    parse_order_response(&raw, &one, &config)
                }
            }) {
                Ok(quote) => batch.quotes.push(quote),
                Err(error) => batch.failures.push(BridgeQuoteFailure {
                    provider: Some("relay".into()),
                    destination_asset: Some(asset.clone()),
                    solana_destination_asset: None,
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
        let config = self.config.with_intent_assets(&quote.intent)?;
        let cross_vm =
            quote.intent.solana_source.is_some() || quote.intent.solana_destination.is_some();
        if cross_vm {
            validate_cross_vm_intent(&config, &quote.intent, bounds)?;
        } else {
            validate_intent(&config, &quote.intent, bounds)?;
        }
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
                "Request another quote. GoFin cannot execute the legacy deposit format.",
            )
        })?;
        let _order = parse_b256(order_id)?;
        if cross_vm {
            return prepare_cross_vm_quote(quote, bounds, wallet, &config);
        }
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
        validate_prepared_calls(&config, quote, &steps, parse_b256(order_id)?)?;
        let fixed_scope_digest = fixed_scope_digest(&quote.intent, &config)?;
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
        let mut status_request = self
            .client
            .get(format!("{}/intents/status", self.api_base))
            .query(&[("requestId", request_id)]);
        if let Some(key) = self.api_key.as_deref() {
            status_request = status_request.header("x-api-key", key);
        }
        let response = status_request.send().await.map_err(|_| {
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
                "Retry later; GoFin will reconcile the original order using its saved identity.",
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
        let mut evidence = parse_evidence_candidates(&value, transfer)?;
        if transfer.source_chain_id == RELAY_SOLANA_CHAIN_ID
            || transfer.destination_network.as_deref() == Some("solana:mainnet-beta")
        {
            let request_version = if self.api_key.is_some() { "v3" } else { "v2" };
            let mut lookup = self
                .client
                .get(format!("{}/requests/{request_version}", self.api_base))
                .query(&[("id", request_id)]);
            if let Some(key) = self.api_key.as_deref() {
                lookup = lookup.header("x-api-key", key);
            }
            let response = lookup.send().await.map_err(|_| {
                bridge_error(
                    "BRIDGE_PROVIDER_UNAVAILABLE",
                    "reconcile",
                    "Relay request lookup is unavailable.",
                    "Retry status; preserve the original transfer identity.",
                )
            })?;
            if !response.status().is_success() {
                return Err(bridge_error(
                    "BRIDGE_PROVIDER_UNAVAILABLE",
                    "reconcile",
                    format!(
                        "Relay request lookup failed with HTTP {}; Hub settlement identity is unavailable.",
                        response.status()
                    ),
                    "Retry status; preserve the original transfer and do not submit another deposit.",
                ));
            }
            let requests: Value = response
                .json()
                .await
                .map_err(|_| invalid_status("Relay request lookup returned malformed JSON"))?;
            append_hub_request_candidate(&mut evidence.candidates, &requests, transfer)?;
            evidence.candidates.sort_by(|a, b| {
                (&a.kind, a.chain_id, &a.transaction_hash).cmp(&(
                    &b.kind,
                    b.chain_id,
                    &b.transaction_hash,
                ))
            });
            evidence.candidates.dedup_by(|a, b| {
                a.kind == b.kind
                    && a.chain_id == b.chain_id
                    && a.transaction_hash == b.transaction_hash
            });
        }
        Ok(evidence)
    }
}

/// Provider currency metadata is a bounded, partial identity candidate list.
/// Exact source matching gates related-chain results; symbols never match assets.
fn parse_currency_candidates(
    raw: &Value,
    request: &crate::BridgeRouteRequest,
) -> Result<Vec<crate::BridgeRouteCandidate>> {
    let entries = raw
        .as_array()
        .ok_or_else(|| invalid_order("currency discovery is not an array"))?;
    ensure(
        entries.len() <= 100,
        "BRIDGE_RESPONSE_INVALID",
        "discovery",
        "Relay currency discovery exceeds its declared bound.",
    )?;
    let source = request.evm_source()?;
    let source_token = source
        .token
        .as_deref()
        .ok_or_else(|| invalid_order("currency discovery requires an exact source token"))?;
    let source_matches = entries.iter().any(|entry| {
        entry["chainId"].as_u64() == Some(source.chain_id)
            && entry["vmType"].as_str() == Some("evm")
            && entry["address"]
                .as_str()
                .is_some_and(|address| address.eq_ignore_ascii_case(source_token))
            && entry["decimals"].as_u64() == Some(u64::from(source.decimals))
    });
    if !source_matches {
        return Ok(vec![]);
    }
    let mut seen = BTreeMap::new();
    let mut candidates = Vec::new();
    for entry in entries {
        if entry["chainId"].as_u64() != Some(request.destination_chain_id)
            || entry["vmType"].as_str() != Some("evm")
        {
            continue;
        }
        let token = entry["address"]
            .as_str()
            .ok_or_else(|| invalid_order("candidate currency has no exact address"))?;
        let address = parse_address(token)?;
        let decimals: u8 = entry["decimals"]
            .as_u64()
            .and_then(|n| n.try_into().ok())
            .ok_or_else(|| invalid_order("candidate currency precision is invalid"))?;
        ensure(
            address != Address::ZERO || decimals == 18,
            "BRIDGE_RESPONSE_INVALID",
            "discovery",
            "Native ETH candidate precision must be exactly 18 decimals.",
        )?;
        if let Some(previous) = seen.insert(address, decimals) {
            ensure(
                previous == decimals,
                "BRIDGE_RESPONSE_INVALID",
                "discovery",
                "Exact candidate token identity has conflicting precision.",
            )?;
            continue;
        }
        candidates.push(crate::BridgeRouteCandidate {
            destination: Some(crate::BridgeAssetInput {
                chain_id: request.destination_chain_id,
                token: (address != Address::ZERO).then(|| address_string(address)),
            }),
            solana_destination: None,
            reported_decimals: Some(decimals),
        });
    }
    Ok(candidates)
}

async fn bounded_provider_json(mut response: reqwest::Response) -> Result<Value> {
    const MAX_BYTES: usize = 256 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| invalid_order("provider response body failed"))?
    {
        ensure(
            bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
            "BRIDGE_RESPONSE_INVALID",
            "discovery",
            "Provider discovery response exceeds its byte bound.",
        )?;
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| invalid_order("provider discovery is malformed JSON"))
}

fn validate_quote_bounds(quote: &BridgeQuote, bounds: &BridgeBounds) -> Result<()> {
    ensure(
        quote.gross_debit.asset == quote.intent.source_asset_id()?
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
            if fee.amount.atoms > cap.maximum {
                return Err(fee_cap_error(
                    "prepare",
                    &fee.amount.asset,
                    &fee.kind,
                    &fee.amount.atoms,
                    bounds,
                ));
            }
        } else if !bounds.fee_caps.is_empty() {
            return Err(fee_cap_error(
                "prepare",
                &fee.amount.asset,
                &fee.kind,
                &fee.amount.atoms,
                bounds,
            ));
        }
    }
    validate_minimum_ratio_decimals(
        quote.gross_debit.asset.decimals,
        quote.minimum_receipt.asset.decimals,
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
    let depository = parse_address(&config.chain(intent.evm_source()?.chain_id)?.depository)?;
    let expected_count = if intent.evm_source()?.token.is_some() {
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
    let offset = if intent.evm_source()?.token.is_some() && expected_count == 2 {
        1
    } else {
        0
    };
    if let Some(token) = &intent.evm_source()?.token {
        let token_addr = parse_address(token)?;
        if expected_count == 2 {
            let approval = &steps[0];
            ensure(
                approval.kind == BridgeStepKind::Approval
                    && approval.ordinal == 0
                    && approval.evm_call()?.chain_id == intent.evm_source()?.chain_id
                    && parse_address(&approval.evm_call()?.to)? == token_addr
                    && parse_address(&approval.expected_sender)? == depositor
                    && approval.evm_call()?.value.is_zero(),
                "BRIDGE_ORDER_INVALID",
                "prepare",
                "Prepared approval is outside the exact source token, signer or chain.",
            )?;
            let bytes = parse_calldata(&approval.evm_call()?.data)?;
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
    let value = if intent.evm_source()?.token.is_some() {
        U256::ZERO
    } else {
        amount
    };
    let expected = if let Some(token) = &intent.evm_source()?.token {
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
            && deposit.evm_call()?.chain_id == intent.evm_source()?.chain_id
            && parse_address(&deposit.evm_call()?.to)? == depository
            && parse_address(&deposit.expected_sender)? == depositor
            && parse_u256(&deposit.evm_call()?.value)? == value
            && parse_calldata(&deposit.evm_call()?.data)? == expected,
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
        intent.evm_source()?.chain_id != intent.evm_destination_chain_id()?,
        "BRIDGE_ROUTE_UNAVAILABLE",
        "validation",
        "Source and destination chains must differ.",
    )?;
    config.chain(intent.evm_source()?.chain_id)?;
    config.chain(intent.evm_destination_chain_id()?)?;
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
    if let Some(token) = &intent.evm_source()?.token {
        parse_address(token)?;
        let decimals = config
            .token_decimals(intent.evm_source()?.chain_id, token)
            .ok_or_else(|| {
                bridge_error(
                    "BRIDGE_ROUTE_UNAVAILABLE",
                    "validation",
                    "Source token is not explicitly reviewed for this network.",
                    "Choose a reviewed asset.",
                )
            })?;
        ensure(
            decimals == intent.evm_source()?.decimals,
            "BRIDGE_ASSET_INVALID",
            "validation",
            "Source token precision differs from reviewed asset metadata.",
        )?;
    } else {
        ensure(
            intent.evm_source()?.decimals == 18,
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
            asset.chain_id == intent.evm_destination_chain_id()?,
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
            asset.chain_id == intent.evm_source()?.chain_id
                || asset.chain_id == intent.evm_destination_chain_id()?,
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
            fee.asset.network == format!("ethereum:{}", intent.evm_source()?.chain_id)
                || fee.asset.network == format!("ethereum:{}", intent.evm_destination_chain_id()?),
            "BRIDGE_BOUNDS_INVALID",
            "validation",
            "Fee cap asset must be on the source or destination network.",
        )?;
    }
    Ok(())
}

fn validate_cross_vm_intent(
    config: &ReviewedConfig,
    intent: &BridgeIntent,
    bounds: &BridgeBounds,
) -> Result<()> {
    ensure(
        intent.source.is_some() != intent.solana_source.is_some(),
        "INVALID_INPUT",
        "validation",
        "Select exactly one bridge source asset.",
    )?;
    ensure(
        intent.solana_source.is_some() != intent.solana_destination.is_some(),
        "INVALID_INPUT",
        "validation",
        "Relay Solana routes must cross between Solana and a reviewed EVM network.",
    )?;
    ensure(
        !intent.amount.is_zero(),
        "INVALID_INPUT",
        "validation",
        "Bridge amount must be greater than zero.",
    )?;
    if let Some(source) = &intent.solana_source {
        ensure(
            source.network == "solana:mainnet-beta" && source.decimals <= 18,
            "BRIDGE_ASSET_INVALID",
            "validation",
            "Solana source identity is invalid.",
        )?;
        gofin_solana::parse_address(&intent.source_address)?;
        gofin_solana::parse_address(&intent.refund_to)?;
        if let Some(mint) = &source.mint {
            gofin_solana::parse_address(mint)?;
        } else {
            ensure(
                source.decimals == 9,
                "BRIDGE_ASSET_INVALID",
                "validation",
                "Native SOL precision must be nine.",
            )?;
        }
        config.chain(intent.evm_destination_chain_id()?)?;
        ensure(
            !intent.destination_assets.is_empty() && intent.destination_assets.len() <= 8,
            "BRIDGE_ROUTE_UNAVAILABLE",
            "validation",
            "Select exact EVM destination assets.",
        )?;
        parse_address(&intent.recipient)?;
    } else {
        let source = intent.evm_source()?;
        config.chain(source.chain_id)?;
        parse_address(&intent.source_address)?;
        parse_address(&intent.refund_to)?;
        validate_cross_vm_evm_asset(config, source)?;
        let destination = intent
            .solana_destination
            .as_ref()
            .ok_or_else(|| invalid_order("Solana destination missing"))?;
        ensure(
            intent.destination_chain_id.is_none()
                && intent.destination_assets.is_empty()
                && destination.network == "solana:mainnet-beta"
                && destination.asset.network == "solana:mainnet-beta"
                && destination.recipient == intent.recipient,
            "INVALID_INPUT",
            "validation",
            "Solana destination identity is ambiguous.",
        )?;
        gofin_solana::parse_address(&intent.recipient)?;
        if destination.asset.reference != "native" {
            gofin_solana::parse_address(&destination.asset.reference)?;
        } else {
            ensure(
                destination.asset.decimals == 9,
                "BRIDGE_ASSET_INVALID",
                "validation",
                "Native SOL precision must be nine.",
            )?;
        }
    }
    for asset in &intent.destination_assets {
        ensure(
            asset.chain_id == intent.evm_destination_chain_id()?,
            "BRIDGE_ASSET_INVALID",
            "validation",
            "EVM destination asset is on a different chain.",
        )?;
        validate_cross_vm_evm_asset(config, asset)?;
    }
    for asset in &intent.allowed_refund_assets {
        ensure(
            intent
                .source
                .as_ref()
                .is_some_and(|source| source.chain_id == asset.chain_id)
                || intent.destination_chain_id == Some(asset.chain_id),
            "BRIDGE_BOUNDS_INVALID",
            "validation",
            "Refund EVM asset must be on the selected source or destination network.",
        )?;
        validate_cross_vm_evm_asset(config, asset)?;
    }
    for asset in &intent.allowed_solana_refund_assets {
        ensure(
            asset.network == "solana:mainnet-beta",
            "BRIDGE_ASSET_INVALID",
            "validation",
            "Solana refund asset must be on mainnet-beta.",
        )?;
        if let Some(mint) = &asset.mint {
            gofin_solana::parse_address(mint)?;
        } else {
            ensure(
                asset.decimals == 9,
                "BRIDGE_ASSET_INVALID",
                "validation",
                "Native SOL precision must be nine.",
            )?;
        }
    }
    ensure(
        !intent.allowed_refund_assets.is_empty() || !intent.allowed_solana_refund_assets.is_empty(),
        "BRIDGE_BOUNDS_INVALID",
        "validation",
        "At least one exact refund asset is required.",
    )?;
    let maximum = bounds
        .maximum_source_debit
        .as_ref()
        .unwrap_or(&intent.amount);
    ensure(
        intent.amount <= *maximum,
        "BRIDGE_BOUNDS_VIOLATED",
        "validation",
        "Input exceeds maximum source debit.",
    )?;
    if let Some(interval) = &bounds.input {
        ensure(
            interval.minimum <= intent.amount && intent.amount <= interval.maximum,
            "BRIDGE_BOUNDS_VIOLATED",
            "validation",
            "Input is outside the approved interval.",
        )?;
    }
    Ok(())
}

fn validate_cross_vm_evm_asset(config: &ReviewedConfig, asset: &crate::EvmAsset) -> Result<()> {
    config.chain(asset.chain_id)?;
    if let Some(token) = &asset.token {
        parse_address(token)?;
        ensure(
            config.token_decimals(asset.chain_id, token) == Some(asset.decimals),
            "BRIDGE_ASSET_INVALID",
            "validation",
            "EVM token identity or precision is not independently reviewed.",
        )
    } else {
        ensure(
            asset.decimals == 18,
            "BRIDGE_ASSET_INVALID",
            "validation",
            "Native ETH precision must be 18.",
        )
    }
}

fn prepare_cross_vm_quote(
    quote: &BridgeQuote,
    bounds: &BridgeBounds,
    wallet: &str,
    config: &ReviewedConfig,
) -> Result<PreparedBridge> {
    let order_id = quote
        .protocol_order_id
        .as_deref()
        .ok_or_else(missing_protocol)?;
    let order_hash = parse_b256(order_id)?;
    let steps = quote.protocol_steps.clone();
    ensure(
        !steps.is_empty(),
        "BRIDGE_PROTOCOL_DATA_MISSING",
        "prepare",
        "Relay quote has no validated source steps.",
    )?;
    if quote.intent.solana_source.is_some() {
        gofin_solana::parse_address(wallet)?;
        ensure(
            wallet == quote.intent.source_address && steps.len() == 1,
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Solana signer or source steps differ from the selected wallet.",
        )?;
        let step = &steps[0];
        let tx = step
            .solana_transaction
            .as_ref()
            .ok_or_else(|| invalid_order("missing Solana transaction"))?;
        ensure(
            step.call.is_none()
                && step.solana_mint.is_none()
                && step.kind == BridgeStepKind::Deposit
                && step.expected_sender == wallet
                && tx.source_address == wallet
                && tx.order_id.eq_ignore_ascii_case(order_id)
                && quote.provider_request_id.as_deref() == Some(tx.request_id.as_str())
                && tx.amount_atoms == quote.intent.amount
                && tx.mint
                    == quote
                        .intent
                        .solana_source
                        .as_ref()
                        .and_then(|a| a.mint.clone())
                && step.maximum_debit
                    == vec![quote.intent.source_amount(quote.intent.amount.clone())?]
                && !tx.instructions.is_empty()
                && tx.transaction_base64.is_none(),
            "BRIDGE_QUOTE_INVALID",
            "prepare",
            "Solana source step differs from the saved exact order and wallet.",
        )?;
    } else {
        let signer = parse_address(wallet)?;
        for step in &steps {
            ensure(
                parse_address(&step.expected_sender)? == signer,
                "BRIDGE_QUOTE_INVALID",
                "prepare",
                "Relay sender differs from selected wallet.",
            )?;
        }
        validate_prepared_calls(config, quote, &steps, order_hash)?;
    }
    validate_quote_bounds(quote, bounds)?;
    Ok(PreparedBridge {
        quote: quote.clone(),
        bounds: bounds.clone(),
        steps,
        source_order_id: Some(order_id.to_ascii_lowercase()),
        fixed_scope_digest: fixed_scope_digest_cross_vm(&quote.intent, config)?,
    })
}

fn fixed_scope_digest_cross_vm(intent: &BridgeIntent, config: &ReviewedConfig) -> Result<String> {
    let source_depository = if let Some(source) = &intent.source {
        config.chain(source.chain_id)?.depository.clone()
    } else {
        solana::DEPOSITORY_PROGRAM.into()
    };
    let destination_depository = if let Some(chain) = intent.destination_chain_id {
        config.chain(chain)?.depository.clone()
    } else {
        solana::DEPOSITORY_PROGRAM.into()
    };
    let fixed = json!({"provider":"relay","wallet":intent.wallet,"source_address":intent.source_address,
        "source":intent.source,"solana_source":intent.solana_source,"destination_chain_id":intent.destination_chain_id,
        "solana_destination":intent.solana_destination,"destination_assets":intent.destination_assets,
        "recipient":intent.recipient,"refund_to":intent.refund_to,
        "allowed_refund_assets":intent.allowed_refund_assets,"allowed_solana_refund_assets":intent.allowed_solana_refund_assets,
        "source_depository":source_depository,"destination_depository":destination_depository});
    let encoded = serde_json::to_vec(&fixed)
        .map_err(|_| invalid_order("fixed scope cannot be serialized"))?;
    Ok(format!("0x{}", hex::encode(keccak256(encoded))))
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
    let protocol = value.pointer("/protocol/v2").ok_or_else(missing_protocol)?;
    let order = protocol.get("orderData").ok_or_else(missing_protocol)?;
    ensure(
        order.get("version").and_then(Value::as_str) == Some("v1"),
        "BRIDGE_PROTOCOL_UNSUPPORTED",
        "quote",
        "Relay protocol.v2 order must use the reviewed EVM Order v1 encoding.",
    )?;
    let order_id = protocol
        .get("orderId")
        .and_then(Value::as_str)
        .ok_or_else(missing_protocol)?;
    let stored_hash = parse_b256(order_id)?;
    let request_id = value
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_order("missing Relay requestId"))?;
    parse_b256(request_id)?;
    let source = request.intent.evm_source()?;
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
                .chain(request.intent.evm_destination_chain_id()?)?
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
        request.intent.evm_destination_chain_id()?,
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
        cctp: None,
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

fn parse_cross_vm_order_response(
    value: &Value,
    request: &BridgeQuoteRequest,
    config: &ReviewedConfig,
) -> Result<BridgeQuote> {
    parse_cross_vm_order_response_at(value, request, config, Utc::now())
}

fn parse_cross_vm_order_response_at(
    value: &Value,
    request: &BridgeQuoteRequest,
    config: &ReviewedConfig,
    now: DateTime<Utc>,
) -> Result<BridgeQuote> {
    let protocol = value.pointer("/protocol/v2").ok_or_else(missing_protocol)?;
    let order = protocol.get("orderData").ok_or_else(missing_protocol)?;
    ensure(
        order.get("version").and_then(Value::as_str) == Some("v1"),
        "BRIDGE_PROTOCOL_UNSUPPORTED",
        "quote",
        "Relay Solana route requires reviewed Order v1 protocol data.",
    )?;
    let order_id = field_string(protocol, &["orderId"])?;
    let order_hash = parse_b256(order_id)?;
    let request_id = field_string(value, &["requestId"])?;
    parse_b256(request_id)?;
    ensure(
        canonical_order_id(order)? == order_hash,
        "BRIDGE_ORDER_ID_MISMATCH",
        "quote",
        "Relay order ID differs from the locally recomputed protocol hash.",
    )?;
    let intent = &request.intent;
    let source_chain = intent
        .source
        .as_ref()
        .map_or(RELAY_SOLANA_CHAIN_ID, |a| a.chain_id);
    let destination_chain = intent.destination_chain_id.unwrap_or(RELAY_SOLANA_CHAIN_ID);
    let source_asset = intent.source_asset_id()?;
    let destination_asset = if let Some(destination) = &intent.solana_destination {
        destination.asset.clone()
    } else {
        intent
            .destination_assets
            .first()
            .ok_or_else(|| invalid_order("EVM destination asset missing"))?
            .asset_id()
    };
    let inputs = order
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing input payments"))?;
    ensure(
        inputs.len() == 1,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Only one explicit source payment is supported.",
    )?;
    let payment = inputs[0]
        .get("payment")
        .ok_or_else(|| invalid_order("missing input payment"))?;
    ensure(
        field_string(payment, &["chainId"])? == protocol_chain_identity(config, source_chain)?
            && protocol_address_matches(
                field_string(payment, &["currency"])?,
                &source_asset,
                source_chain,
            )?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay protocol input network or asset differs from verified source.",
    )?;
    let gross = Quantity::new(field_string(payment, &["amount"])?)?;
    ensure(
        gross == intent.amount && !Quantity::new(field_string(payment, &["weight"])?)?.is_zero(),
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay protocol input amount or weight is invalid.",
    )?;
    let output = order
        .get("output")
        .ok_or_else(|| invalid_order("missing protocol output"))?;
    ensure(
        field_string(output, &["chainId"])? == protocol_chain_identity(config, destination_chain)?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay protocol output network differs from selected destination.",
    )?;
    let payments = output
        .get("payments")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("missing output payments"))?;
    ensure(
        payments.len() == 1,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Only one exact destination payment is supported.",
    )?;
    let payout = &payments[0];
    ensure(
        protocol_address_matches(
            field_string(payout, &["currency"])?,
            &destination_asset,
            destination_chain,
        )? && protocol_recipient_matches(
            field_string(payout, &["recipient"])?,
            &intent.recipient,
            destination_chain,
        )?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay protocol payout asset or recipient differs from approved destination.",
    )?;
    let expected = Quantity::new(field_string(payout, &["expectedAmount"])?)?;
    let minimum = Quantity::new(field_string(payout, &["minimumAmount"])?)?;
    ensure(
        !minimum.is_zero() && minimum <= expected,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay minimum payout is invalid.",
    )?;
    if let Some(bound) = &request.bounds.minimum_net_receipt {
        ensure(
            bound.asset == destination_asset && minimum >= bound.atoms,
            "BRIDGE_BOUNDS_VIOLATED",
            "quote",
            "Relay minimum payout is below approved floor.",
        )?;
    }
    ensure(
        output
            .get("calls")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty),
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay payout contains unsupported calls.",
    )?;
    validate_cross_vm_extra_data(
        config,
        destination_chain,
        field_string(output, &["extraData"])?,
    )?;
    let deadline = field_u64(output, &["deadline"])?;
    let valid_until: DateTime<Utc> = DateTime::from_timestamp(
        i64::try_from(deadline).map_err(|_| invalid_order("protocol deadline overflows"))?,
        0,
    )
    .ok_or_else(|| invalid_order("protocol deadline is invalid"))?;
    ensure(
        valid_until > now,
        "BRIDGE_QUOTE_EXPIRED",
        "quote",
        "Relay protocol order is expired.",
    )?;
    if let Some(latest) = request.bounds.latest_protocol_deadline {
        ensure(
            deadline <= latest,
            "BRIDGE_BOUNDS_VIOLATED",
            "quote",
            "Relay protocol deadline exceeds approved limit.",
        )?;
    }
    ensure(
        field_string(order, &["solverChainId"])? == config.relay_solver_protocol_chain_id
            && parse_address(field_string(order, &["solver"])?)?
                == parse_address(RELAY_FILL_AUTHORITY)?,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Relay solver differs from reviewed fill authority.",
    )?;
    let refund_paths = parse_cross_vm_refunds(&inputs[0], intent, config, &gross)?;
    let fees = parse_cross_vm_fees(value, intent, config)?;
    validate_fee_caps(&fees, &request.bounds)?;
    validate_minimum_ratio_decimals(
        source_asset.decimals,
        destination_asset.decimals,
        &gross,
        &expected,
        request.bounds.minimum_output_input_ratio.as_ref(),
    )?;
    let (approval, steps) = if intent.solana_source.is_some() {
        (
            None,
            parse_solana_source_steps(
                value,
                intent,
                (order_id, request_id),
                &gross,
                &fees,
                &request.bounds,
                &valid_until,
            )?,
        )
    } else {
        parse_transaction_steps(value, order, intent, config, &gross, order_hash)?
    };
    let route_id = fixed_scope_digest_cross_vm(intent, config)?;
    Ok(BridgeQuote {
        cctp: None,
        quote_id: format!("relay:{}:{}", order_id.to_ascii_lowercase(), route_id),
        provider: "relay".into(),
        route_id,
        intent: intent.clone(),
        gross_debit: Amount {
            asset: source_asset,
            atoms: gross,
        },
        expected_receipt: Amount {
            asset: destination_asset.clone(),
            atoms: expected,
        },
        minimum_receipt: Amount {
            asset: destination_asset,
            atoms: minimum,
        },
        fees,
        approval,
        protocol_steps: steps,
        estimated_arrival_seconds: parse_arrival_estimate(
            value.get("details").and_then(|v| v.get("timeEstimate")),
        )?,
        observed_at: now.to_rfc3339(),
        valid_until: valid_until.to_rfc3339(),
        class: QuoteClass::Executable,
        refund_paths,
        order_solver: Some(RELAY_FILL_AUTHORITY.to_ascii_lowercase()),
        protocol_order_id: Some(order_id.into()),
        provider_request_id: Some(request_id.into()),
    })
}

fn protocol_chain_identity(config: &ReviewedConfig, chain: u64) -> Result<&str> {
    if chain == RELAY_SOLANA_CHAIN_ID {
        Ok(RELAY_SOLANA_PROTOCOL_CHAIN_ID)
    } else {
        Ok(config.chain(chain)?.protocol_chain_id.as_str())
    }
}
fn protocol_solana_pubkey(value: &str) -> Result<String> {
    let bytes = if value.starts_with("0x") {
        parse_calldata(value)?
    } else {
        bs58::decode(value)
            .into_vec()
            .map_err(|_| invalid_order("Solana protocol address is not base58"))?
    };
    ensure(
        bytes.len() == 32,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Solana protocol address must contain 32 bytes.",
    )?;
    Ok(bs58::encode(bytes).into_string())
}
fn protocol_address_matches(encoded: &str, asset: &AssetId, chain: u64) -> Result<bool> {
    if chain == RELAY_SOLANA_CHAIN_ID {
        let expected = if asset.reference == "native" {
            RELAY_NATIVE_SOL_CURRENCY
        } else {
            &asset.reference
        };
        Ok(protocol_solana_pubkey(encoded)? == expected)
    } else {
        let expected = if asset.reference == "native" {
            Address::ZERO
        } else {
            parse_address(&asset.reference)?
        };
        Ok(decode_evm_currency(encoded)? == expected)
    }
}
fn protocol_recipient_matches(encoded: &str, recipient: &str, chain: u64) -> Result<bool> {
    if chain == RELAY_SOLANA_CHAIN_ID {
        Ok(protocol_solana_pubkey(encoded)? == recipient)
    } else {
        Ok(decode_evm_currency(encoded)? == parse_address(recipient)?)
    }
}
fn validate_cross_vm_extra_data(config: &ReviewedConfig, chain: u64, value: &str) -> Result<()> {
    if chain == RELAY_SOLANA_CHAIN_ID {
        ensure(
            value == "0x",
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Unreviewed Solana protocol extraData is unsupported.",
        )
    } else {
        validate_router_extra_data(config, chain, value)
    }
}

fn parse_cross_vm_refunds(
    input: &Value,
    intent: &BridgeIntent,
    config: &ReviewedConfig,
    gross: &Quantity,
) -> Result<Vec<RefundPath>> {
    let refunds = input
        .get("refunds")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("refund paths missing"))?;
    ensure(
        !refunds.is_empty() && refunds.len() <= 16,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Refund paths are missing or unbounded.",
    )?;
    let mut paths = Vec::with_capacity(refunds.len());
    let source_chain = intent
        .source
        .as_ref()
        .map_or(RELAY_SOLANA_CHAIN_ID, |a| a.chain_id);
    let destination_chain = intent.destination_chain_id.unwrap_or(RELAY_SOLANA_CHAIN_ID);
    for refund in refunds {
        let chain = protocol_chain_to_id(config, field_string(refund, &["chainId"])?)?;
        ensure(
            chain == source_chain || chain == destination_chain,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Refund network is outside the approved route.",
        )?;
        let encoded_asset = field_string(refund, &["currency"])?;
        let allowed = if chain == RELAY_SOLANA_CHAIN_ID {
            intent
                .allowed_solana_refund_assets
                .iter()
                .map(crate::SolanaAsset::asset_id)
                .find(|asset| {
                    protocol_address_matches(encoded_asset, asset, chain).unwrap_or(false)
                })
        } else {
            intent
                .allowed_refund_assets
                .iter()
                .filter(|a| a.chain_id == chain)
                .map(crate::EvmAsset::asset_id)
                .find(|asset| {
                    protocol_address_matches(encoded_asset, asset, chain).unwrap_or(false)
                })
        }
        .ok_or_else(|| invalid_order("refund asset is not explicitly approved"))?;
        let recipient = if chain == source_chain {
            &intent.refund_to
        } else {
            &intent.recipient
        };
        ensure(
            protocol_recipient_matches(field_string(refund, &["recipient"])?, recipient, chain)?,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Refund recipient differs from approved address.",
        )?;
        validate_cross_vm_extra_data(config, chain, field_string(refund, &["extraData"])?)?;
        let minimum = Quantity::new(field_string(refund, &["minimumAmount"])?)?;
        ensure(
            minimum <= *gross,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Refund minimum exceeds at-risk source amount.",
        )?;
        paths.push(RefundPath {
            chain_id: chain,
            network: (chain == RELAY_SOLANA_CHAIN_ID).then(|| "solana:mainnet-beta".into()),
            asset: allowed,
            recipient: recipient.clone(),
            minimum,
            maximum: gross.clone(),
        });
    }
    Ok(paths)
}

fn parse_cross_vm_fees(
    value: &Value,
    intent: &BridgeIntent,
    config: &ReviewedConfig,
) -> Result<Vec<BridgeFee>> {
    let entries = value
        .get("fees")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_order("fee schedule missing"))?;
    let mut fees = Vec::new();
    for (name, fee) in entries {
        if (name == "relayerGas" || name == "relayerService") && entries.contains_key("relayer") {
            continue;
        }
        let Some(currency) = fee.get("currency") else {
            continue;
        };
        let chain = field_u64(currency, &["chainId"])?;
        let address = field_string(currency, &["address"])?;
        let decimals: u8 = field_u64(currency, &["decimals"])?
            .try_into()
            .map_err(|_| invalid_order("fee precision is invalid"))?;
        let atoms = Quantity::new(field_string(fee, &["amount"])?)?;
        if atoms.is_zero() {
            continue;
        }
        let asset = if chain == RELAY_SOLANA_CHAIN_ID {
            gofin_solana::parse_address(address)?;
            let reference = if address == RELAY_NATIVE_SOL_CURRENCY {
                "native".into()
            } else {
                address.into()
            };
            let exact = AssetId {
                network: "solana:mainnet-beta".into(),
                reference,
                decimals,
            };
            ensure(
                (address == RELAY_NATIVE_SOL_CURRENCY && decimals == 9)
                    || intent
                        .solana_source
                        .as_ref()
                        .is_some_and(|a| a.asset_id() == exact)
                    || intent
                        .solana_destination
                        .as_ref()
                        .is_some_and(|d| d.asset == exact)
                    || intent
                        .allowed_solana_refund_assets
                        .iter()
                        .any(|a| a.asset_id() == exact),
                "BRIDGE_ORDER_INVALID",
                "quote",
                "Fee Solana mint or decimals lack independent verification.",
            )?;
            exact
        } else {
            config.chain(chain)?;
            let token = parse_address(address)?;
            if token == Address::ZERO {
                ensure(
                    decimals == 18,
                    "BRIDGE_ORDER_INVALID",
                    "quote",
                    "Native ETH fee precision must be 18.",
                )?;
                crate::EvmAsset {
                    chain_id: chain,
                    token: None,
                    decimals,
                }
                .asset_id()
            } else {
                ensure(
                    config.token_decimals(chain, address) == Some(decimals),
                    "BRIDGE_ORDER_INVALID",
                    "quote",
                    "Fee token precision differs from verified asset.",
                )?;
                crate::EvmAsset {
                    chain_id: chain,
                    token: Some(address.to_ascii_lowercase()),
                    decimals,
                }
                .asset_id()
            }
        };
        let (kind, payer) = match name.as_str() {
            "gas" => (FeeKind::Gas, "sender"),
            "relayerGas" => (FeeKind::Gas, "relay"),
            "relayer" | "relayerService" => (FeeKind::WithheldFromOutput, "relay"),
            _ => (FeeKind::AdditionalDebit, "unknown"),
        };
        fees.push(BridgeFee {
            amount: Amount { asset, atoms },
            kind,
            payer: payer.into(),
        });
    }
    Ok(fees)
}

fn parse_solana_source_steps(
    value: &Value,
    intent: &BridgeIntent,
    identities: (&str, &str),
    gross: &Quantity,
    fees: &[BridgeFee],
    bounds: &BridgeBounds,
    valid_until: &DateTime<Utc>,
) -> Result<Vec<BridgeStep>> {
    let (order_id, request_id) = identities;
    let steps = value
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("Solana source steps missing"))?;
    ensure(
        steps.len() == 1
            && steps[0]["kind"] == "transaction"
            && steps[0]["id"] == "deposit"
            && steps[0]["requestId"] == request_id,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Solana route must contain one exact deposit step.",
    )?;
    let items = steps[0]["items"]
        .as_array()
        .ok_or_else(|| invalid_order("Solana transaction item missing"))?;
    ensure(
        items.len() == 1 && items[0]["status"] == "incomplete",
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Solana route must contain one incomplete transaction item.",
    )?;
    let data = items[0]
        .get("data")
        .ok_or_else(|| invalid_order("Solana transaction data missing"))?;
    ensure(
        data.get("transactionVersion")
            .is_none_or(|v| v == 0 || v == "v0" || v == "legacy"),
        "BRIDGE_PROTOCOL_UNSUPPORTED",
        "quote",
        "Unsupported Solana transaction version.",
    )?;
    let raw_instructions = data
        .get("instructions")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_order("Solana instructions missing"))?;
    ensure(
        !raw_instructions.is_empty() && raw_instructions.len() <= 64,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Solana instruction count is invalid.",
    )?;
    let mut instructions = Vec::with_capacity(raw_instructions.len());
    for raw in raw_instructions {
        let program_id = field_string(raw, &["programId"])?;
        gofin_solana::parse_address(program_id)?;
        ensure(
            matches!(
                program_id,
                solana::DEPOSITORY_PROGRAM
                    | "ComputeBudget111111111111111111111111111111"
                    | "11111111111111111111111111111111"
                    | "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
                    | "TokenzQdBNbLqP5VEhdkAS6EPF6CvaCnThQLHAcgHbR"
                    | "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
            ),
            "BRIDGE_PROTOCOL_UNSUPPORTED",
            "quote",
            "Solana source swap or unreviewed instruction is unsupported.",
        )?;
        let keys = raw
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_order("Solana instruction keys missing"))?;
        ensure(
            keys.len() <= 64,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Solana instruction has too many keys.",
        )?;
        let mut accounts = Vec::with_capacity(keys.len());
        for key in keys {
            let address = field_string(key, &["pubkey"])?;
            gofin_solana::parse_address(address)?;
            accounts.push(gofin_solana::transaction::NormalizedAccountMeta {
                address: address.into(),
                signer: key
                    .get("isSigner")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| invalid_order("Solana signer flag missing"))?,
                writable: key
                    .get("isWritable")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| invalid_order("Solana writable flag missing"))?,
            });
        }
        let data_hex = field_string(raw, &["data"])?;
        let bytes = hex::decode(data_hex.strip_prefix("0x").unwrap_or(data_hex))
            .map_err(|_| invalid_order("Solana instruction data is invalid hex"))?;
        ensure(
            bytes.len() <= 1024,
            "BRIDGE_ORDER_INVALID",
            "quote",
            "Solana instruction data is oversized.",
        )?;
        instructions.push(gofin_solana::NormalizedInstruction {
            program_id: program_id.into(),
            accounts,
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    let order_bytes = parse_b256(order_id)?;
    let deposit_instructions = instructions
        .iter()
        .filter(|ix| ix.program_id == solana::DEPOSITORY_PROGRAM)
        .collect::<Vec<_>>();
    ensure(
        deposit_instructions.len() == 1,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Solana source needs one reviewed depository instruction.",
    )?;
    let deposit_bytes = base64::engine::general_purpose::STANDARD
        .decode(&deposit_instructions[0].data_base64)
        .map_err(|_| invalid_order("Solana deposit instruction is malformed"))?;
    ensure(
        deposit_bytes.len() >= 48
            && deposit_bytes[deposit_bytes.len() - 32..] == order_bytes.as_slice()[..],
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Solana deposit instruction does not bind the canonical order ID.",
    )?;
    let lookup_tables = data
        .get("addressLookupTableAddresses")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid_order("Solana lookup table address invalid"))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    ensure(
        lookup_tables.len() <= 8,
        "BRIDGE_ORDER_INVALID",
        "quote",
        "Solana lookup table count is unbounded.",
    )?;
    for table in &lookup_tables {
        gofin_solana::parse_address(table)?;
    }
    let source = intent
        .solana_source
        .as_ref()
        .ok_or_else(|| invalid_order("Solana source asset missing"))?;
    let sol_gas = AssetId {
        network: "solana:mainnet-beta".into(),
        reference: "native".into(),
        decimals: 9,
    };
    let estimated_network_fee_lamports = fees
        .iter()
        .filter(|f| f.amount.asset == sol_gas && f.kind == FeeKind::Gas && f.payer == "sender")
        .try_fold(0u64, |sum, fee| {
            fee.amount
                .atoms
                .as_str()
                .parse::<u64>()
                .ok()
                .and_then(|v| sum.checked_add(v))
                .ok_or_else(|| invalid_order("Solana network fee exceeds u64"))
        })?;
    let estimated_rent_lamports = fees
        .iter()
        .filter(|f| f.amount.asset == sol_gas && f.kind == FeeKind::AdditionalDebit)
        .try_fold(0u64, |sum, fee| {
            fee.amount
                .atoms
                .as_str()
                .parse::<u64>()
                .ok()
                .and_then(|v| sum.checked_add(v))
                .ok_or_else(|| invalid_order("Solana rent exceeds u64"))
        })?;
    let cap = |kind: FeeKind| -> Result<u64> {
        bounds
            .fee_caps
            .iter()
            .find(|cap| cap.asset == sol_gas && cap.kind == kind)
            .ok_or_else(|| {
                let required = Quantity::from_u64(if kind == FeeKind::Gas {
                    estimated_network_fee_lamports
                } else {
                    estimated_rent_lamports
                });
                bridge_error(
                    "BRIDGE_ORDER_INVALID",
                    "quote",
                    "Solana source needs explicit gas and rent caps.",
                    "Inspect the exact asset, fee_kind and required amount in details, then review finite source gas and rent caps within the intended budget. Gas requires a positive cap; the observed estimate is not a guaranteed ceiling for later quotes.",
                ).with_details(fee_cap_details(&sol_gas, &kind, &required, bounds))
            })?
            .maximum
            .as_str()
            .parse::<u64>()
            .map_err(|_| invalid_order("Solana fee cap exceeds u64"))
    };
    let max_network_fee_lamports = cap(FeeKind::Gas)?;
    let max_rent_lamports = cap(FeeKind::AdditionalDebit)?;
    ensure(
        max_network_fee_lamports > 0
            && estimated_network_fee_lamports <= max_network_fee_lamports
            && estimated_rent_lamports <= max_rent_lamports,
        "BRIDGE_BOUNDS_INVALID",
        "quote",
        "Solana source fee estimate exceeds its exact finite caps.",
    )?;
    Ok(vec![BridgeStep {
        ordinal: 0,
        kind: BridgeStepKind::Deposit,
        call: None,
        solana_mint: None,
        solana_transaction: Some(crate::SolanaSourceTransaction {
            transaction_base64: None,
            instructions,
            lookup_tables,
            source_address: intent.source_address.clone(),
            order_id: order_id.into(),
            request_id: request_id.into(),
            mint: source.mint.clone(),
            amount_atoms: gross.clone(),
            max_network_fee_lamports,
            max_rent_lamports,
            last_valid_block_height: data.get("lastValidBlockHeight").and_then(Value::as_u64),
            expire_at: Some(valid_until.to_rfc3339()),
        }),
        expected_sender: intent.source_address.clone(),
        maximum_debit: vec![source.amount(gross.clone())],
        dependencies: vec![],
    }])
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
        if step.get("kind").and_then(Value::as_str) == Some("transaction")
            && let Some(items) = step.get("items").and_then(Value::as_array)
        {
            for item in items {
                if item.get("status").and_then(Value::as_str) == Some("incomplete")
                    && let Some(data) = item.get("data")
                {
                    txs.push(data);
                }
            }
        }
    }
    if intent.evm_source()?.token.is_some() {
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
    let depository = parse_address(&config.chain(intent.evm_source()?.chain_id)?.depository)?;
    let wallet = parse_address(&intent.source_address)?;
    let amount = parse_u256(gross)?;
    let mut out = Vec::new();
    let mut approval = None;
    if let Some(token_text) = &intent.evm_source()?.token {
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
            validate_tx_common(tx, intent.evm_source()?.chain_id, token, U256::ZERO)?;
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
                solana_mint: None,
                solana_transaction: None,
                call: Some(EvmCall {
                    chain_id: intent.evm_source()?.chain_id,
                    to: address_string(token),
                    value: Quantity::zero(),
                    data: hex_data(bytes),
                }),
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
    let deposit_value = if intent.evm_source()?.token.is_some() {
        Quantity::zero()
    } else {
        gross.clone()
    };
    validate_tx_common(
        deposit_tx,
        intent.evm_source()?.chain_id,
        depository,
        parse_u256(&deposit_value)?,
    )?;
    let bytes = parse_calldata(field_string(deposit_tx, &["data"])?)?;
    let expected = if intent.evm_source()?.token.is_some() {
        IRelayDepository::depositErc20Call {
            depositor: wallet,
            token: parse_address(intent.evm_source()?.token.as_deref().expect("token"))?,
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
        solana_mint: None,
        solana_transaction: None,
        call: Some(EvmCall {
            chain_id: intent.evm_source()?.chain_id,
            to: address_string(depository),
            value: deposit_value,
            data: hex_data(bytes),
        }),
        expected_sender: field_string(deposit_tx, &["from"])?.to_ascii_lowercase(),
        maximum_debit: vec![intent.evm_source()?.amount(gross.clone())],
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
    if name == RELAY_SOLANA_PROTOCOL_CHAIN_ID {
        return Ok(RELAY_SOLANA_CHAIN_ID);
    }
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
        let atoms = Quantity::new(amount)?;
        // A validated zero fee grants no spending authority. Anonymous
        // responses include zero app/subsidy rows; requiring a positive cap
        // for those rows would invent an additional debit.
        if atoms.is_zero() {
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
            amount: Amount { asset, atoms },
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
    validate_minimum_ratio_decimals(source.decimals, destination.decimals, input, output, ratio)
}
fn validate_minimum_ratio_decimals(
    source_decimals: u8,
    destination_decimals: u8,
    input: &Quantity,
    output: &Quantity,
    ratio: Option<&crate::RatioBound>,
) -> Result<()> {
    let Some(ratio) = ratio else { return Ok(()) };
    let input_scale = num_bigint::BigUint::from(10u8).pow(source_decimals as u32);
    let output_scale = num_bigint::BigUint::from(10u8).pow(destination_decimals as u32);
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
fn fee_cap_details(
    asset: &AssetId,
    kind: &FeeKind,
    required: &Quantity,
    bounds: &BridgeBounds,
) -> Value {
    let matching = bounds
        .fee_caps
        .iter()
        .find(|cap| cap.asset == *asset && cap.kind == *kind);
    json!({
        "provider": "relay",
        "violation": if matching.is_some() { "fee_cap_exceeded" } else { "missing_fee_cap" },
        "asset": asset,
        "fee_kind": kind,
        "required_atoms": required,
        "required_decimal": required.to_decimal(asset.decimals),
        "matching_cap": matching,
        "supplied_fee_caps": bounds.fee_caps,
    })
}
fn fee_cap_error(
    stage: &str,
    asset: &AssetId,
    kind: &FeeKind,
    required: &Quantity,
    bounds: &BridgeBounds,
) -> gofin_domain::GoFinError {
    let matching = bounds
        .fee_caps
        .iter()
        .any(|cap| cap.asset == *asset && cap.kind == *kind);
    bridge_error(
        "BRIDGE_BOUNDS_VIOLATED",
        stage,
        if matching {
            "Relay fees exceed the explicit asset and fee-kind cap."
        } else {
            "Relay returned a fee without a matching explicit asset and fee-kind cap."
        },
        "Inspect the exact asset, fee_kind and required amount in details. Review a finite cap within the intended budget or choose another quote; the observed requirement is not a guaranteed ceiling for a later quote.",
    ).with_details(fee_cap_details(asset, kind, required, bounds))
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
            if amount > cap.maximum {
                return Err(fee_cap_error("quote", &asset, &kind, &amount, bounds));
            }
        } else if !bounds.fee_caps.is_empty() {
            return Err(fee_cap_error("quote", &asset, &kind, &amount, bounds));
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
                network: None,
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
fn invalid_order(reason: &str) -> gofin_domain::GoFinError {
    bridge_error(
        "BRIDGE_ORDER_INVALID",
        "quote",
        format!("Relay protocol order is invalid: {reason}."),
        "Request a new quote; GoFin will not execute unverifiable order data.",
    )
}
fn missing_protocol() -> gofin_domain::GoFinError {
    bridge_error(
        "BRIDGE_PROTOCOL_DATA_MISSING",
        "quote",
        "Relay quote omitted required protocol.v2 order data.",
        "Request another quote. GoFin cannot execute the legacy deposit format.",
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
    let fixed = json!({"provider":"relay","wallet":intent.wallet,"source_address":intent.source_address.to_ascii_lowercase(),"source":intent.source,"destination_chain_id":intent.evm_destination_chain_id()?,"destination_assets":intent.destination_assets,"recipient":intent.recipient.to_ascii_lowercase(),"refund_to":intent.refund_to.to_ascii_lowercase(),"allowed_refund_assets":intent.allowed_refund_assets,"reviewed_source_depository":config.chain(intent.evm_source()?.chain_id)?.depository.to_ascii_lowercase(),"reviewed_destination_depository":config.chain(intent.evm_destination_chain_id()?)?.depository.to_ascii_lowercase(),"source_router_v3":config.chain(intent.evm_source()?.chain_id)?.router_v3,"destination_router_v3":config.chain(intent.evm_destination_chain_id()?)?.router_v3});
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
    let saved_destination_chain =
        if transfer.destination_network.as_deref() == Some("solana:mainnet-beta") {
            RELAY_SOLANA_CHAIN_ID
        } else {
            transfer.evm_destination_chain_id()?
        };
    let destination = value
        .get("destinationChainId")
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid_status("malformed destinationChainId"))
        })
        .transpose()?
        .unwrap_or(saved_destination_chain);
    ensure(
        origin == transfer.source_chain_id && destination == saved_destination_chain,
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
        EvidenceKind::DestinationFill => {
            chain_id
                == transfer
                    .destination_chain_id
                    .unwrap_or(RELAY_SOLANA_CHAIN_ID)
        }
        EvidenceKind::Refund => {
            chain_id == transfer.source_chain_id
                || chain_id
                    == transfer
                        .destination_chain_id
                        .unwrap_or(RELAY_SOLANA_CHAIN_ID)
        }
        EvidenceKind::SettlementHub => chain_id == 537_713,
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
        let valid_hash = if chain_id == RELAY_SOLANA_CHAIN_ID {
            bs58::decode(tx)
                .into_vec()
                .is_ok_and(|bytes| bytes.len() == 64 && bs58::encode(bytes).into_string() == tx)
        } else {
            tx.len() == 66 && tx.starts_with("0x") && tx[2..].bytes().all(|b| b.is_ascii_hexdigit())
        };
        if !valid_hash {
            return Err(invalid_status("malformed transaction hash"));
        }
        out.push(BridgeEvidenceCandidate {
            order_id: transfer.order_id.clone(),
            kind: kind.clone(),
            chain_id,
            transaction_hash: if chain_id == RELAY_SOLANA_CHAIN_ID {
                tx.into()
            } else {
                tx.to_ascii_lowercase()
            },
            correlation: None,
        });
    }
    Ok(())
}

fn append_hub_request_candidate(
    out: &mut Vec<BridgeEvidenceCandidate>,
    value: &Value,
    transfer: &BridgeTransferRef,
) -> Result<()> {
    let requests = value
        .get("requests")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_status("Relay request lookup is missing requests"))?;
    ensure(
        requests.len() <= 1,
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        "Relay request lookup returned multiple identities for one exact request ID.",
    )?;
    let Some(request) = requests.first() else {
        return Ok(());
    };
    ensure(
        request.get("id").and_then(Value::as_str).is_some_and(|id| {
            transfer
                .provider_request_id
                .as_deref()
                .is_some_and(|saved| id.eq_ignore_ascii_case(saved))
        }),
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        "Relay request lookup returned a different request ID.",
    )?;
    let Some(relay) = request.pointer("/protocol/settlement/relay") else {
        return Ok(());
    };
    ensure(
        relay.get("chainId").and_then(Value::as_u64) == Some(537_713),
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        "Relay settlement locator is not on the reviewed Hub chain.",
    )?;
    let hash = relay
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_status("Relay Hub settlement locator is missing its hash"))?;
    ensure(
        hash.len() == 66
            && hash.starts_with("0x")
            && hash[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "BRIDGE_RESPONSE_INVALID",
        "reconcile",
        "Relay Hub settlement locator has an invalid hash.",
    )?;
    out.push(BridgeEvidenceCandidate {
        order_id: transfer.order_id.clone(),
        kind: EvidenceKind::SettlementHub,
        chain_id: 537_713,
        transaction_hash: hash.to_ascii_lowercase(),
        correlation: None,
    });
    Ok(())
}

fn invalid_status(reason: &str) -> gofin_domain::GoFinError {
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
            destination_network: None,
            destination_chain_id: Some(42161),
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
    async fn loopback_retirement_is_actionable_with_optional_protected_key() {
        let (base, task) = fixture(404, "{}").await;
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let provider = RelayProvider {
            client,
            config: config(),
            api_base: base,
            api_key: Some(Zeroizing::new("fixture-secret".into())),
        };
        let intent = BridgeIntent {
            wallet: gofin_domain::ResourceId("evm:wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: Some(crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }),
            solana_source: None,
            solana_destination: None,
            destination_chain_id: Some(42161),
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
            allowed_solana_refund_assets: vec![],
            amount: Quantity::from_u64(1),
        };
        let quote_request = BridgeQuoteRequest {
            cctp: None,
            intent,
            bounds: BridgeBounds::default(),
        };
        let err = provider.quote_http(&quote_request).await.unwrap_err();
        assert_eq!(err.code, "BRIDGE_PROVIDER_UNAVAILABLE");
        assert!(err.message.contains("unavailable"));
        assert!(!err.message.contains("fixture-secret"));
        let request = task.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post /quote http/1.1"));
        assert!(request.contains("x-api-key: fixture-secret"));
        assert!(request.contains("includeprotocoldata"));
        assert!(request.contains("explicitdeposit"));
        assert!(request.contains("usepermit"));
        assert!(request.contains("usedepositaddress"));
        assert!(request.contains("disableoriginswaps"));
        let (base, task) = fixture(404, "{}").await;
        let no_key = RelayProvider {
            client: Client::new(),
            config: config(),
            api_base: base,
            api_key: None,
        };
        assert_eq!(
            no_key.quote_http(&quote_request).await.unwrap_err().code,
            "BRIDGE_PROVIDER_UNAVAILABLE"
        );
        let request = task.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post /quote http/1.1"));
        assert!(!request.contains("x-api-key"));
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
            api_key: None,
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

    #[tokio::test]
    async fn solana_status_uses_keyless_v2_request_lookup() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let request_id = format!("0x{}", "22".repeat(32));
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in [
                format!(
                    "{{\"status\":\"waiting\",\"requestId\":\"{request_id}\",\"originChainId\":792703809,\"destinationChainId\":42161,\"inTxHashes\":[],\"txHashes\":[],\"outTxHashes\":[]}}"
                ),
                format!(
                    "{{\"requests\":[{{\"id\":\"{request_id}\",\"protocol\":{{\"settlement\":{{\"relay\":{{\"chainId\":537713,\"hash\":\"0x{}\"}}}}}}}}]}}",
                    "55".repeat(32)
                ),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = vec![0; 4096];
                let n = stream.read(&mut data).await.unwrap();
                requests.push(String::from_utf8_lossy(&data[..n]).to_ascii_lowercase());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let provider = RelayProvider {
            client: Client::new(),
            config: config(),
            api_base: base,
            api_key: None,
        };
        let mut transfer = transfer();
        transfer.source_chain_id = RELAY_SOLANA_CHAIN_ID;
        let status = provider.reconcile(&transfer).await.unwrap();
        assert_eq!(status.candidates.len(), 1);
        assert_eq!(status.candidates[0].kind, EvidenceKind::SettlementHub);
        assert_eq!(status.candidates[0].chain_id, 537713);
        assert_eq!(status.candidates[0].order_id, transfer.order_id);
        let requests = task.await.unwrap();
        assert!(requests[0].starts_with("get /intents/status?requestid="));
        assert!(requests[1].starts_with("get /requests/v2?id="));
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("x-api-key"))
        );
    }

    #[tokio::test]
    async fn solana_lookup_http_failure_preserves_unresolved_order_and_reports_status() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for (status, body) in [
                (
                    200,
                    r#"{"status":"success","requestId":"0x2222222222222222222222222222222222222222222222222222222222222222","originChainId":1,"destinationChainId":792703809,"inTxHashes":[],"outTxHashes":[]}"#,
                ),
                (429, r#"{"error":"rate limited"}"#),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = [0; 4096];
                let read = stream.read(&mut data).await.unwrap();
                assert!(read > 0);
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let provider = RelayProvider {
            client: Client::new(),
            config: config(),
            api_base: base,
            api_key: None,
        };
        let mut transfer = transfer();
        transfer.destination_chain_id = None;
        transfer.destination_network = Some("solana:mainnet-beta".into());
        let error = provider.reconcile(&transfer).await.unwrap_err();
        assert_eq!(error.code, "BRIDGE_PROVIDER_UNAVAILABLE");
        assert!(error.message.contains("HTTP 429"));
        assert!(
            error
                .message
                .contains("Hub settlement identity is unavailable")
        );
        task.await.unwrap();
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
    fn hub_request_locator_requires_exact_saved_request_identity() {
        let mut transfer = transfer();
        transfer.destination_chain_id = None;
        transfer.destination_network = Some("solana:mainnet-beta".into());
        let id = transfer.provider_request_id.clone().unwrap();
        let hash = format!("0x{}", "ab".repeat(32));
        let request = json!({"requests":[{"id":id,"protocol":{"settlement":{"relay":{
            "chainId":537713,"hash":hash,"operation":"transfer"}}}}]});
        let mut candidates = vec![];
        append_hub_request_candidate(&mut candidates, &request, &transfer).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, EvidenceKind::SettlementHub);
        assert_eq!(candidates[0].chain_id, 537713);
        assert_eq!(candidates[0].transaction_hash, hash);
        let mut mismatch = request.clone();
        mismatch["requests"][0]["id"] = "other".into();
        assert!(append_hub_request_candidate(&mut vec![], &mismatch, &transfer).is_err());
        mismatch = request;
        mismatch["requests"][0]["protocol"]["settlement"]["relay"]["chainId"] = 1.into();
        assert!(append_hub_request_candidate(&mut vec![], &mismatch, &transfer).is_err());
    }

    #[test]
    fn anonymous_protocol_v2_fixture_parses_and_binds_both_ids() {
        let config = config();
        let raw: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anonymous-synthetic-quote.json"
        ))
        .unwrap();
        let intent = BridgeIntent {
            wallet: gofin_domain::ResourceId("evm:wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: Some(crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }),
            solana_source: None,
            solana_destination: None,
            destination_chain_id: Some(42161),
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
            allowed_solana_refund_assets: vec![],
            amount: Quantity::new("3000000000000000").unwrap(),
        };
        let fixture_now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let request = BridgeQuoteRequest {
            cctp: None,
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
            quote.protocol_steps[0].evm_call().unwrap().value.as_str(),
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
        let exact_deadline = BridgeBounds {
            latest_protocol_deadline: Some(protocol_deadline),
            ..BridgeBounds::default()
        };
        assert!(
            provider
                .prepare(&quote, &exact_deadline, &prepared.steps[0].expected_sender)
                .is_ok()
        );
        let past_cap = BridgeBounds {
            latest_protocol_deadline: Some(protocol_deadline - 1),
            ..BridgeBounds::default()
        };
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
        altered.protocol_steps[0].evm_call_mut().unwrap().value =
            Quantity::new("3000000000000001").unwrap();
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
    fn solana_destination_order_binds_exact_network_asset_recipient_and_floor() {
        let config = config();
        let mut raw: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anonymous-synthetic-quote.json"
        ))
        .unwrap();
        let recipient = RELAY_NATIVE_SOL_CURRENCY;
        let asset = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "native".into(),
            decimals: 9,
        };
        let intent = BridgeIntent {
            wallet: gofin_domain::ResourceId("evm:wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: Some(crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }),
            solana_source: None,
            destination_chain_id: None,
            solana_destination: Some(crate::SolanaDestination {
                network: "solana:mainnet-beta".into(),
                wallet: None,
                wallet_resource: gofin_domain::ResourceId(format!(
                    "solana:mainnet-beta:{recipient}"
                )),
                wallet_address: recipient.into(),
                recipient: recipient.into(),
                asset: asset.clone(),
                recipient_token_account: String::new(),
                destination_caller: String::new(),
            }),
            destination_assets: vec![],
            recipient: recipient.into(),
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
            allowed_solana_refund_assets: vec![],
            amount: Quantity::new("3000000000000000").unwrap(),
        };
        let order = &mut raw["protocol"]["v2"]["orderData"];
        order["inputs"][0]["refunds"]
            .as_array_mut()
            .unwrap()
            .truncate(1);
        order["output"]["chainId"] = RELAY_SOLANA_PROTOCOL_CHAIN_ID.into();
        order["output"]["payments"][0]["recipient"] = format!("0x{}", "00".repeat(32)).into();
        order["output"]["payments"][0]["currency"] = format!("0x{}", "00".repeat(32)).into();
        order["output"]["extraData"] = "0x".into();
        let hash = canonical_order_id(order).unwrap();
        let hash_text = format!("{hash:#x}");
        raw["protocol"]["v2"]["orderId"] = hash_text.clone().into();
        let old_data = raw["steps"][0]["items"][0]["data"]["data"]
            .as_str()
            .unwrap();
        raw["steps"][0]["items"][0]["data"]["data"] =
            format!("{}{}", &old_data[..old_data.len() - 64], &hash_text[2..]).into();
        let request = BridgeQuoteRequest {
            cctp: None,
            intent,
            bounds: BridgeBounds {
                minimum_net_receipt: Some(Amount {
                    asset,
                    atoms: Quantity::new("2929632005258326").unwrap(),
                }),
                ..BridgeBounds::default()
            },
        };
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let parsed = parse_cross_vm_order_response_at(&raw, &request, &config, now).unwrap();
        assert_eq!(
            parsed.protocol_order_id.as_deref(),
            Some(hash_text.as_str())
        );
        assert_eq!(parsed.minimum_receipt.asset.network, "solana:mainnet-beta");

        for (field, value) in [
            ("recipient", format!("0x{}", "01".repeat(32))),
            ("currency", format!("0x{}", "02".repeat(32))),
        ] {
            let mut changed = raw.clone();
            changed["protocol"]["v2"]["orderData"]["output"]["payments"][0][field] = value.into();
            changed["protocol"]["v2"]["orderId"] = format!(
                "{:#x}",
                canonical_order_id(&changed["protocol"]["v2"]["orderData"]).unwrap()
            )
            .into();
            assert!(parse_cross_vm_order_response_at(&changed, &request, &config, now).is_err());
        }
        let mut below_floor = raw.clone();
        below_floor["protocol"]["v2"]["orderData"]["output"]["payments"][0]["minimumAmount"] =
            "1".into();
        below_floor["protocol"]["v2"]["orderId"] = format!(
            "{:#x}",
            canonical_order_id(&below_floor["protocol"]["v2"]["orderData"]).unwrap()
        )
        .into();
        assert!(parse_cross_vm_order_response_at(&below_floor, &request, &config, now).is_err());
    }

    #[test]
    fn solana_source_order_extracts_exact_instruction_envelope() {
        let config = config();
        let mut raw: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anonymous-synthetic-quote.json"
        ))
        .unwrap();
        let solana = crate::SolanaAsset {
            network: "solana:mainnet-beta".into(),
            mint: None,
            decimals: 9,
        };
        let intent = BridgeIntent {
            wallet: gofin_domain::ResourceId("solana:wallet".into()),
            source_address: RELAY_NATIVE_SOL_CURRENCY.into(),
            source: None,
            solana_source: Some(solana.clone()),
            destination_chain_id: Some(42161),
            solana_destination: None,
            destination_assets: vec![crate::EvmAsset {
                chain_id: 42161,
                token: None,
                decimals: 18,
            }],
            recipient: "0x1111111111111111111111111111111111111111".into(),
            refund_to: RELAY_NATIVE_SOL_CURRENCY.into(),
            allowed_refund_assets: vec![],
            allowed_solana_refund_assets: vec![solana.clone()],
            amount: Quantity::new("3000000000000000").unwrap(),
        };
        let order = &mut raw["protocol"]["v2"]["orderData"];
        order["inputs"][0]["payment"]["chainId"] = RELAY_SOLANA_PROTOCOL_CHAIN_ID.into();
        order["inputs"][0]["payment"]["currency"] = format!("0x{}", "00".repeat(32)).into();
        order["inputs"][0]["refunds"] = json!([{ "chainId": RELAY_SOLANA_PROTOCOL_CHAIN_ID,
            "recipient": format!("0x{}", "00".repeat(32)),
            "currency": format!("0x{}", "00".repeat(32)),
            "minimumAmount": "0", "deadline": 1792088190, "extraData": "0x" }]);
        let hash = canonical_order_id(order).unwrap();
        let hash_text = format!("{hash:#x}");
        raw["protocol"]["v2"]["orderId"] = hash_text.clone().into();
        raw["fees"] = json!({"gas":{"currency":{"chainId":RELAY_SOLANA_CHAIN_ID,
            "address":RELAY_NATIVE_SOL_CURRENCY,"decimals":9},"amount":"5000"}});
        let mut instruction = vec![0u8; 16];
        instruction.extend_from_slice(hash.as_slice());
        raw["steps"][0]["items"][0]["data"] = json!({
            "instructions":[{"programId":solana::DEPOSITORY_PROGRAM,"keys":[],
                "data":format!("0x{}",hex::encode(instruction))}],
            "addressLookupTableAddresses":[],"transactionVersion":"v0"
        });
        let request = BridgeQuoteRequest {
            cctp: None,
            intent,
            bounds: BridgeBounds {
                fee_caps: vec![
                    crate::FeeCap {
                        asset: solana.asset_id(),
                        kind: FeeKind::Gas,
                        maximum: Quantity::from_u64(5000),
                        maximum_usd_micros: None,
                    },
                    crate::FeeCap {
                        asset: solana.asset_id(),
                        kind: FeeKind::AdditionalDebit,
                        maximum: Quantity::zero(),
                        maximum_usd_micros: None,
                    },
                ],
                ..BridgeBounds::default()
            },
        };
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let parsed = parse_cross_vm_order_response_at(&raw, &request, &config, now).unwrap();
        let tx = parsed.protocol_steps[0]
            .solana_transaction
            .as_ref()
            .unwrap();
        assert_eq!(tx.order_id, hash_text);
        assert_eq!(tx.amount_atoms, Quantity::new("3000000000000000").unwrap());
        assert_eq!(tx.max_network_fee_lamports, 5000);
        assert_eq!(parsed.gross_debit.asset.network, "solana:mainnet-beta");
        let mut wrong = raw.clone();
        wrong["steps"][0]["items"][0]["data"]["instructions"][0]["programId"] =
            "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5mD5KKdA".into();
        assert!(parse_cross_vm_order_response_at(&wrong, &request, &config, now).is_err());
        let mut wrong = raw.clone();
        wrong["steps"][0]["items"][0]["data"]["instructions"][0]["data"] =
            format!("0x{}", "00".repeat(48)).into();
        assert!(parse_cross_vm_order_response_at(&wrong, &request, &config, now).is_err());
    }

    #[test]
    fn solana_protocol_currency_matches_exact_verified_mint_bytes() {
        for seed in [7u8, 19u8] {
            let bytes = [seed; 32];
            let mint = bs58::encode(bytes).into_string();
            let asset = AssetId {
                network: "solana:mainnet-beta".into(),
                reference: mint,
                decimals: if seed == 7 { 6 } else { 9 },
            };
            let encoded = format!("0x{}", hex::encode(bytes));
            assert!(protocol_address_matches(&encoded, &asset, RELAY_SOLANA_CHAIN_ID).unwrap());
            let other = format!("0x{}", hex::encode([seed + 1; 32]));
            assert!(!protocol_address_matches(&other, &asset, RELAY_SOLANA_CHAIN_ID).unwrap());
        }
        let sol = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "native".into(),
            decimals: 9,
        };
        assert!(
            protocol_address_matches(
                &format!("0x{}", "00".repeat(32)),
                &sol,
                RELAY_SOLANA_CHAIN_ID
            )
            .unwrap()
        );
        assert!(protocol_address_matches("0x1234", &sol, RELAY_SOLANA_CHAIN_ID).is_err());
    }

    #[test]
    fn protocol_parser_rejects_tampered_identity_debit_refund_and_missing_data() {
        let config = config();
        let base: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/anonymous-synthetic-quote.json"
        ))
        .unwrap();
        let intent = BridgeIntent {
            wallet: gofin_domain::ResourceId("evm:wallet".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: Some(crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }),
            solana_source: None,
            solana_destination: None,
            destination_chain_id: Some(42161),
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
            allowed_solana_refund_assets: vec![],
            amount: Quantity::new("3000000000000000").unwrap(),
        };
        let request = BridgeQuoteRequest {
            cctp: None,
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
            wallet: gofin_domain::ResourceId("evm:w".into()),
            source_address: "0x1111111111111111111111111111111111111111".into(),
            source: Some(crate::EvmAsset {
                chain_id: 1,
                token: None,
                decimals: 18,
            }),
            solana_source: None,
            solana_destination: None,
            destination_chain_id: Some(8453),
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
            allowed_solana_refund_assets: vec![],
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
        let error = validate_fee_caps(&fees, &bounds).unwrap_err();
        let details = error.details.unwrap();
        assert_eq!(details["violation"], "fee_cap_exceeded");
        assert_eq!(details["required_atoms"], "11");
        assert_eq!(details["matching_cap"]["maximum"], "10");
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

#[cfg(test)]
mod dynamic_tests;
