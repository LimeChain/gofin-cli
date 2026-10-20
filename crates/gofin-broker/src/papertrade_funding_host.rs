//! Protected runtime adapter for the Papertrade funding planner.
//!
//! The planner owns route sequencing and immutable plan identity. This host
//! boundary is intentionally closed: the application composition supplies a
//! crate-private implementation backed by enrolled credentials, configured
//! RPCs and the protected durable journal. No arbitrary endpoint, signer,
//! calldata or transport is accepted from a funding intent.

use std::sync::Arc;

use gofin_bridge::{EvmCall, cctp};
use gofin_domain::GoFinError;
use gofin_evm::alloy::primitives::{Bytes, U256};
use gofin_papertrade::{FundingDirection, FundingIntent, FundingReceipt, FundingRoute};
use serde_json::{Value, json};

fn transaction_matches(
    reported: &Value,
    expected: &gofin_evm::FrozenTransaction,
) -> Result<bool, GoFinError> {
    let parse_u256 = |field: &str| -> Result<U256, GoFinError> {
        let raw = reported[field].as_str().ok_or_else(|| {
            GoFinError::validation("Mined transaction omitted a required envelope field.")
        })?;
        U256::from_str_radix(raw.strip_prefix("0x").unwrap_or(raw), 16)
            .map_err(|_| GoFinError::validation("Mined transaction quantity is malformed."))
    };
    let parse_u64 = |field: &str| -> Result<u64, GoFinError> {
        let raw = reported[field].as_str().ok_or_else(|| {
            GoFinError::validation("Mined transaction omitted a required envelope field.")
        })?;
        u64::from_str_radix(raw.strip_prefix("0x").unwrap_or(raw), 16)
            .map_err(|_| GoFinError::validation("Mined transaction quantity is malformed."))
    };
    let expected_value = U256::from_str_radix(expected.value.as_str(), 10)
        .map_err(|_| GoFinError::validation("Frozen transaction value is malformed."))?;
    let expected_max_fee = U256::from_str_radix(expected.max_fee_per_gas.as_str(), 10)
        .map_err(|_| GoFinError::validation("Frozen transaction fee is malformed."))?;
    let expected_priority = U256::from_str_radix(expected.max_priority_fee_per_gas.as_str(), 10)
        .map_err(|_| GoFinError::validation("Frozen transaction priority fee is malformed."))?;
    let input = reported["input"]
        .as_str()
        .or_else(|| reported["data"].as_str());
    Ok(reported["from"]
        .as_str()
        .is_some_and(|v| v.eq_ignore_ascii_case(&expected.sender))
        && reported["to"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(&expected.to))
        && input.is_some_and(|v| v.eq_ignore_ascii_case(&expected.calldata))
        && parse_u256("value")? == expected_value
        && parse_u256("maxFeePerGas")? == expected_max_fee
        && parse_u256("maxPriorityFeePerGas")? == expected_priority
        && parse_u64("chainId")? == expected.chain_id
        && parse_u64("nonce")? == expected.nonce
        && parse_u64("gas")? == expected.gas)
}

fn has_exact_usdc_transfer(
    receipt: &Value,
    owner: &str,
    recipient: &str,
    atoms: u64,
) -> Result<bool, GoFinError> {
    let expected_owner = format!("0x{}{}", "00".repeat(12), &owner[2..]);
    let expected_recipient = format!("0x{}{}", "00".repeat(12), &recipient[2..]);
    let logs = receipt["logs"]
        .as_array()
        .ok_or_else(|| GoFinError::validation("Canonical HyperEVM receipt omitted its logs."))?;
    let matches =
        logs.iter()
            .filter(|log| {
                log["address"]
                    .as_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case(cctp::hyperevm::NATIVE_USDC))
                    && log["topics"][0].as_str().is_some_and(|v| {
                        v.eq_ignore_ascii_case(
                            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                        )
                    })
                    && log["topics"][1]
                        .as_str()
                        .is_some_and(|v| v.eq_ignore_ascii_case(&expected_owner))
                    && log["topics"][2]
                        .as_str()
                        .is_some_and(|v| v.eq_ignore_ascii_case(&expected_recipient))
                    && log["data"].as_str().and_then(|v| {
                        U256::from_str_radix(v.strip_prefix("0x").unwrap_or(v), 16).ok()
                    }) == Some(U256::from(atoms))
            })
            .count();
    Ok(matches == 1)
}

fn load_plan_for_submission(
    credentials: &dyn crate::BrokerCredentialProvider,
    submission_id: &str,
) -> Result<BoundFunding, GoFinError> {
    let operation_id = submission_id
        .strip_prefix("papertrade-funding:")
        .and_then(|rest| rest.rsplit_once(':').map(|(operation, _)| operation))
        .ok_or_else(|| GoFinError::validation("Funding leg identity is malformed."))?;
    let value = credentials
        .papertrade_runtime_record(&format!("papertrade-funding-plan:{operation_id}"))?
        .ok_or_else(|| GoFinError::validation("Durable funding plan is missing."))?;
    serde_json::from_value(value)
        .map_err(|_| GoFinError::validation("Durable funding plan is malformed."))
}

fn sweep_finalize_matches(transaction: &Value, owner: &str) -> bool {
    let expected_owner = format!(
        "{}{}",
        "0".repeat(24),
        owner.trim_start_matches("0x").to_ascii_lowercase()
    );
    transaction["to"]
        .as_str()
        .is_some_and(|to| to.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE))
        && transaction["input"].as_str().is_some_and(|input| {
            input.len() == 10 + 64
                && input.starts_with("0x6d142da1")
                && input[10..].eq_ignore_ascii_case(&expected_owner)
        })
}

fn parse_usdc_atoms(value: &str) -> Result<u64, GoFinError> {
    let quantity = gofin_domain::Quantity::from_decimal(value, 6)
        .map_err(|_| GoFinError::validation("Hyperliquid USDC balance is malformed."))?;
    u64::try_from(quantity.integer())
        .map_err(|_| GoFinError::validation("Hyperliquid USDC balance exceeds supported range."))
}

fn parse_hyperliquid_usdc_atoms(value: &str) -> Result<u64, GoFinError> {
    let quantity = gofin_domain::Quantity::from_decimal(value, 8)
        .map_err(|_| GoFinError::validation("Hyperliquid USDC balance is malformed."))?;
    u64::try_from(quantity.integer() / 100u32)
        .map_err(|_| GoFinError::validation("Hyperliquid USDC balance exceeds supported range."))
}

fn core_available_covers_source_bound(
    available_atoms: u64,
    source_atoms: u64,
    maximum_source_fee_atoms: u64,
) -> bool {
    source_atoms
        .checked_add(maximum_source_fee_atoms)
        .is_some_and(|required| available_atoms >= required)
}

fn observed_activation_fee_atoms(
    gross_atoms: u64,
    credited_atoms: u64,
    maximum_fee_atoms: u64,
) -> Result<u64, GoFinError> {
    gross_atoms
        .checked_sub(credited_atoms)
        .filter(|fee| *fee <= maximum_fee_atoms)
        .ok_or_else(|| {
            GoFinError::validation(
                "HyperEVM bridge activation debit exceeds its approved USDC bound.",
            )
        })
}

fn event_position(event: &Value) -> Option<(u64, u64)> {
    let parse = |value: &Value| {
        let raw = value.as_str()?;
        u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok()
    };
    Some((
        parse(&event["blockNumber"])?,
        parse(&event["transactionIndex"])?,
    ))
}

fn receipt_transaction_index(receipt: &Value) -> Option<u64> {
    let raw = receipt.get("transactionIndex")?.as_str()?;
    u64::from_str_radix(raw.strip_prefix("0x")?, 16).ok()
}

use crate::papertrade_funding::{
    BoundFunding, CctpQuoteRequest, FundingAction, FundingCctpQuote, FundingLeg, FundingLegState,
    FundingPreflight, PapertradeFundingHost, ProxyRegistrationProof,
};

/// Narrow protected-services port consumed by the host adapter. Its concrete
/// implementation is owned by broker composition and must resolve signers
/// only from enrolled connection references. Route methods accept typed
/// funding objects rather than caller-controlled URLs or raw transaction
/// payloads.
#[async_trait::async_trait]
#[allow(clippy::too_many_arguments)] // Closed typed hooks retain exact journal/authority identities.
pub(crate) trait PapertradeFundingServices: Send + Sync {
    fn attach_runtime(
        &self,
        _runtime: std::sync::Weak<crate::papertrade_runtime::BrokerPapertradeRuntime>,
    ) -> Result<(), GoFinError> {
        Ok(())
    }

    fn attach_execution_state(
        &self,
        _state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        Ok(())
    }

    async fn preflight(&self, intent: &FundingIntent) -> Result<FundingPreflight, GoFinError>;

    /// POST the official funding-check request for `owner`, then compare its
    /// returned proxy with the factory call on chain. `request_id` is a local
    /// durable identity because the endpoint does not return one.
    async fn check_proxy_registration(
        &self,
        operation_id: &str,
        owner: &str,
        factory_proxy: &str,
    ) -> Result<ProxyRegistrationProof, GoFinError>;

    async fn cctp_quote(&self, request: &CctpQuoteRequest) -> Result<FundingCctpQuote, GoFinError>;
    async fn prepare_evm_call(
        &self,
        submission_id: &str,
        intent: &gofin_papertrade::FundingIntent,
        call: &EvmCall,
        maximum_total_fee_atoms: &str,
    ) -> Result<gofin_evm::FrozenTransaction, GoFinError>;

    async fn solana_burn_request(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
        metadata: &cctp::CctpQuoteMetadata,
        proxy: &str,
    ) -> Result<cctp::solana::hypercore_source::BurnRequest, GoFinError>;

    async fn prepare_solana_burn_transaction(
        &self,
        _submission_id: &str,
        _instruction: &gofin_solana::NormalizedInstruction,
        _maximum_network_fee_lamports: u64,
    ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_ROUTE_UNAVAILABLE",
            "configuration",
            "This route has no protected Solana transaction preparer.",
            "Use an enrolled Solana source after its frozen transaction service is configured.",
        ))
    }

    /// Reserve the Hyperliquid nonce in the durable execution-state lane.
    async fn reserve_hyperliquid_nonce(&self, submission_id: &str) -> Result<u64, GoFinError>;

    /// Obtain the exact typed Papertrade session call for internal cash to
    /// Core USDC withdrawal. The implementation must bind the enrolled owner
    /// and persisted operation identity before signing.
    async fn papertrade_withdraw_to_core_intent(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
    ) -> Result<gofin_papertrade::BoundIntent, GoFinError>;

    async fn persist_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError>;
    async fn assert_persisted_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError>;

    /// Submit one pre-persisted route leg. EVM implementations must bind the
    /// exact transaction to a per-leg SubmissionJournal before calling
    /// `EvmRuntime::submit`; Hyperliquid actions use the enrolled owner signer;
    /// Solana CCTP burns use the protected Solana signer; Papertrade cash
    /// withdrawals use the protected Papertrade session signer.
    async fn dispatch_persisted_leg(
        &self,
        leg: &FundingLeg,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<FundingLegState, GoFinError>;

    /// Observe the original submission only. Implementations must never sign
    /// or send a replacement transaction here.
    async fn reconcile_persisted_leg(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
    ) -> Result<FundingLegState, GoFinError>;

    async fn prove_destination(
        &self,
        plan: &BoundFunding,
        settled_legs: &[FundingLegState],
    ) -> Result<Option<FundingReceipt>, GoFinError>;

    /// Consume the exact nonce of a protected, already-attempted Core deposit
    /// with an approved child Noop. The operation and child identities are
    /// supplied by the broker runtime, while this service independently
    /// reloads the immutable funding plan and binds its exact owner/action.
    async fn invalidate_core_deposit_nonce(
        &self,
        _parent_operation_id: &str,
        _parent_record_id: &str,
        _expected_owner: &str,
        _expected_nonce: u64,
        _child_operation_id: &str,
        _child_submission: &gofin_domain::Submission,
        _validated: &gofin_domain::RevalidatedOperation,
        _journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value, GoFinError> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_ROUTE_UNAVAILABLE",
            "recovery",
            "Core nonce invalidation is unavailable for this funding route.",
            "Keep the parent unresolved and use its approved recovery workflow.",
        ))
    }
}

/// Concrete service implementation for the native HyperEVM USDC route. It
/// uses the configured chain-999 runtime and the broker's protected record
/// store; it does not create an approval transaction. Other routes are
/// rejected until their own protected transport is supplied.
pub(crate) struct NativeHyperEvmFundingServices {
    credentials: Arc<dyn crate::BrokerCredentialProvider>,
    runtime: Arc<gofin_evm::EvmRuntime>,
    owner: String,
    account_mode: gofin_hyperliquid::funding::HyperliquidAccountMode,
    maximum_hype_fee_atoms: String,
    confirmations: u64,
    require_finalized: bool,
    http: reqwest::Client,
    hyperliquid_info_url: String,
    papertrade_http: Arc<crate::papertrade_runtime::PapertradeHttp>,
}

impl NativeHyperEvmFundingServices {
    #[allow(clippy::too_many_arguments)] // Explicit protected composition inputs; no caller-controlled route data.
    pub(crate) fn new(
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        runtime: Arc<gofin_evm::EvmRuntime>,
        owner: &str,
        account_mode: gofin_hyperliquid::funding::HyperliquidAccountMode,
        maximum_hype_fee_atoms: &str,
        papertrade_http: Arc<crate::papertrade_runtime::PapertradeHttp>,
        confirmations: u64,
        require_finalized: bool,
    ) -> Result<Self, GoFinError> {
        let owner = gofin_evm::address(owner)
            .map_err(|_| GoFinError::validation("Papertrade owner address is invalid."))?;
        let fee_cap = U256::from_str_radix(maximum_hype_fee_atoms, 10).map_err(|_| {
            GoFinError::validation("Papertrade HYPE fee cap must be a positive decimal integer.")
        })?;
        if runtime.chain_id != 999 || fee_cap.is_zero() || confirmations == 0 {
            return Err(GoFinError::validation(
                "Native HyperEVM funding runtime is missing a reviewed chain, fee, finality, or build bound.",
            ));
        }
        let http = reqwest::Client::builder()
            .user_agent("GoFin/0.1 (Papertrade integration)")
            .timeout(std::time::Duration::from_secs(12))
            .build()
            .map_err(|_| {
                GoFinError::validation("Papertrade funding HTTP client is unavailable.")
            })?;
        Ok(Self {
            credentials,
            runtime,
            owner: format!("{owner:#x}"),
            account_mode,
            maximum_hype_fee_atoms: fee_cap.to_string(),
            confirmations,
            require_finalized,
            http,
            hyperliquid_info_url: "https://api.hyperliquid.xyz/info".into(),
            papertrade_http,
        })
    }

    #[cfg(test)]
    fn test_with_hyperliquid_info_url(mut self, url: &str) -> Self {
        self.hyperliquid_info_url = url.to_owned();
        self
    }

    fn record_key(operation_id: &str) -> String {
        format!("papertrade-funding-plan:{operation_id}")
    }

    fn submission_key(submission_id: &str, suffix: &str) -> String {
        format!("papertrade-funding-leg:{submission_id}:{suffix}")
    }

    fn persist_immutable(&self, key: &str, value: &Value) -> Result<(), GoFinError> {
        if let Some(existing) = self.credentials.papertrade_runtime_record(key)? {
            if existing != *value {
                return Err(GoFinError::new(
                    "PAPERTRADE_FUNDING_IDENTITY_CONFLICT",
                    "journal",
                    "A persisted Papertrade funding identity already contains different data.",
                    "Keep the original funding operation and reconcile it before starting another.",
                ));
            }
            return Ok(());
        }
        self.credentials.put_papertrade_runtime_record(key, value)
    }

    fn unsupported<T>() -> Result<T, GoFinError> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_ROUTE_UNAVAILABLE",
            "configuration",
            "This Papertrade route has no configured protected funding transport.",
            "Use a configured mainnet funding route after its signer, fee policy, and canonical observer are available.",
        ))
    }

    async fn verify_proxy(&self, owner: &str, expected: &str) -> Result<(), GoFinError> {
        let call = crate::papertrade_funding::compute_proxy_address_call(owner)?;
        let result = self
            .runtime
            .rpc
            .request(
                "eth_call",
                json!([{"to":call.to,"data":call.data},"latest"]),
            )
            .await?;
        let actual =
            crate::papertrade_funding::decode_proxy_address_result(result.as_str().ok_or_else(
                || GoFinError::validation("Papertrade factory returned malformed proxy data."),
            )?)?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(GoFinError::validation(
                "Papertrade factory proxy differs from the enrolled proxy identity.",
            ));
        }
        Ok(())
    }

    async fn allowance_atoms(&self, owner: &str) -> Result<u64, GoFinError> {
        let owner = gofin_evm::address(owner)?;
        let spender = gofin_evm::address(cctp::hyperevm::CORE_DEPOSIT_WALLET)?;
        let selector = gofin_evm::alloy::primitives::keccak256(b"allowance(address,address)");
        let mut calldata = Vec::with_capacity(68);
        calldata.extend_from_slice(&selector[..4]);
        calldata.extend_from_slice(&[0; 12]);
        calldata.extend_from_slice(owner.as_slice());
        calldata.extend_from_slice(&[0; 12]);
        calldata.extend_from_slice(spender.as_slice());
        let result = self
            .runtime
            .rpc
            .request(
                "eth_call",
                json!([{
            "to": cctp::hyperevm::NATIVE_USDC,
            "data": format!("0x{}", hex::encode(calldata)),
        }, "latest"]),
            )
            .await?;
        let raw = result
            .as_str()
            .and_then(|value| value.strip_prefix("0x"))
            .ok_or_else(|| {
                GoFinError::validation("HyperEVM USDC allowance response is malformed.")
            })?;
        if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(GoFinError::validation(
                "HyperEVM USDC allowance response is malformed.",
            ));
        }
        let allowance = U256::from_str_radix(raw, 16)
            .map_err(|_| GoFinError::validation("HyperEVM USDC allowance is malformed."))?;
        if allowance > U256::from(u64::MAX) {
            return Err(GoFinError::validation(
                "HyperEVM USDC allowance is unbounded; set a finite allowance before funding.",
            ));
        }
        Ok(allowance.to::<u64>())
    }

    async fn require_zero_proxy_spot_balance(&self, proxy: &str) -> Result<u64, GoFinError> {
        let mut response = self
            .http
            .post(&self.hyperliquid_info_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({"type":"spotClearinghouseState","user":proxy}))
            .send()
            .await
            .map_err(|_| {
                GoFinError::new(
                    "PAPERTRADE_PROXY_BALANCE_UNAVAILABLE",
                    "preview",
                    "Could not verify the Papertrade proxy's existing Core USDC balance.",
                    "Retry after the configured Hyperliquid read service is available.",
                )
            })?;
        if !response.status().is_success() {
            return Err(GoFinError::validation(
                "Hyperliquid proxy spot state request failed.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > 64 * 1024)
        {
            return Err(GoFinError::validation(
                "Hyperliquid spot state exceeded its response bound.",
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| GoFinError::validation("Hyperliquid spot state response body failed."))?
        {
            if body.len().saturating_add(chunk.len()) > 64 * 1024 {
                return Err(GoFinError::validation(
                    "Hyperliquid spot state exceeded its response bound.",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&body).map_err(|_| {
            GoFinError::validation("Hyperliquid proxy spot state response is malformed.")
        })?;
        let balances = value
            .get("balances")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                GoFinError::validation("Hyperliquid proxy spot state omitted balances.")
            })?;
        let mut balance_atoms = 0;
        for balance in balances {
            if balance.get("coin").and_then(Value::as_str) == Some("USDC") {
                let total = balance
                    .get("total")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        GoFinError::validation("Hyperliquid proxy USDC balance omitted total.")
                    })?;
                balance_atoms = parse_usdc_atoms(total)?;
                if balance_atoms != 0 {
                    return Err(GoFinError::new(
                        "PAPERTRADE_PROXY_BALANCE_AMBIGUOUS",
                        "preview",
                        "The Papertrade proxy already has Core USDC awaiting sweep.",
                        "Resolve that existing proxy balance before starting another deposit.",
                    ));
                }
            }
        }
        Ok(balance_atoms)
    }
}

/// Native HyperEVM services plus the enrolled Hyperliquid owner transport used
/// for direct Core deposits. Core account mode and signer role are observed
/// from the official typed read on every preflight.
pub(crate) struct MultiRouteFundingServices {
    native: NativeHyperEvmFundingServices,
    other: Option<Arc<dyn PapertradeFundingServices>>,
    credentials: Arc<dyn crate::BrokerCredentialProvider>,
    hyperliquid_transport: Arc<dyn gofin_hyperliquid::HyperliquidTransport>,
    hyperliquid_account: gofin_hyperliquid::HyperliquidAccount,
}

impl MultiRouteFundingServices {
    pub(crate) fn new(
        native: NativeHyperEvmFundingServices,
        credentials: Arc<dyn crate::BrokerCredentialProvider>,
        hyperliquid_transport: Arc<dyn gofin_hyperliquid::HyperliquidTransport>,
        hyperliquid_account: gofin_hyperliquid::HyperliquidAccount,
    ) -> Result<Self, GoFinError> {
        hyperliquid_account.validate()?;
        if !hyperliquid_account
            .owner
            .eq_ignore_ascii_case(&native.owner)
            || !hyperliquid_account.signing_enabled
            || hyperliquid_account.execution_scope
                != gofin_hyperliquid::HyperliquidExecutionScope::Owner
        {
            return Err(GoFinError::validation(
                "Papertrade Core funding requires the enrolled signing master-owner account.",
            ));
        }
        Ok(Self {
            native,
            other: None,
            credentials,
            hyperliquid_transport,
            hyperliquid_account,
        })
    }

    pub(crate) fn with_other_routes(mut self, other: Arc<dyn PapertradeFundingServices>) -> Self {
        self.other = Some(other);
        self
    }

    fn other(&self) -> Result<&Arc<dyn PapertradeFundingServices>, GoFinError> {
        self.other.as_ref().ok_or_else(|| {
            GoFinError::new(
                "PAPERTRADE_FUNDING_ROUTE_UNAVAILABLE",
                "preflight",
                "This Papertrade funding route is not available in the configured broker.",
                "Use a configured, supported funding route and retry preparation.",
            )
        })
    }

    async fn core_preflight(&self, intent: &FundingIntent) -> Result<FundingPreflight, GoFinError> {
        if intent.route != FundingRoute::Core
            || intent.direction != FundingDirection::Deposit
            || !intent.owner.eq_ignore_ascii_case(&self.native.owner)
        {
            return Self::unavailable();
        }
        let account_mode = gofin_hyperliquid::read_account_mode(
            self.hyperliquid_transport.as_ref(),
            &self.hyperliquid_account,
        )
        .await?;
        let mode = match account_mode.mode {
            gofin_hyperliquid::AccountMode::Separate => {
                gofin_hyperliquid::funding::HyperliquidAccountMode::Default
            }
            gofin_hyperliquid::AccountMode::Unified => {
                gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount
            }
            gofin_hyperliquid::AccountMode::PortfolioMargin
            | gofin_hyperliquid::AccountMode::DexAbstraction => {
                return Err(GoFinError::validation(
                    "Papertrade Core funding does not support this Hyperliquid account mode.",
                ));
            }
        };
        let factory = self
            .native
            .runtime
            .rpc
            .request(
                "eth_call",
                json!([{
                    "to": crate::papertrade_funding::PAPERTRADE_PROXY_FACTORY,
                    "data": crate::papertrade_funding::compute_proxy_address_call(&self.native.owner)?.data,
                }, "latest"]),
            )
            .await?;
        let factory_proxy =
            crate::papertrade_funding::decode_proxy_address_result(factory.as_str().ok_or_else(
                || GoFinError::validation("Papertrade factory proxy is malformed."),
            )?)?;
        let proxy_core_usdc_before_atoms = self
            .native
            .require_zero_proxy_spot_balance(&factory_proxy)
            .await?;
        let core_owner_usdc_before_atoms = self
            .core_available_usdc_atoms(&self.native.owner, mode)
            .await?;
        let source_fee_cap = 1_000_000;
        if !core_available_covers_source_bound(
            core_owner_usdc_before_atoms,
            intent.amount_usdc_atoms,
            source_fee_cap,
        ) {
            intent
                .amount_usdc_atoms
                .checked_add(source_fee_cap)
                .ok_or_else(|| GoFinError::validation("Core source debit bound overflows."))?;
            return Err(GoFinError::new(
                "HYPERCORE_USDC_INSUFFICIENT",
                "preview",
                "The enrolled HyperCore spot wallet does not have enough available USDC for this deposit.",
                "Choose an amount within the owner's available Core spot USDC balance.",
            ));
        }
        let latest = self
            .native
            .runtime
            .rpc
            .request("eth_blockNumber", json!([]))
            .await?;
        let core_sweep_from_block = latest
            .as_str()
            .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| GoFinError::validation("HyperEVM returned an invalid latest block."))?;
        Ok(FundingPreflight {
            owner: self.native.owner.clone(),
            factory_proxy,
            account_mode: mode,
            hyperevm_usdc_allowance_atoms: None,
            native_fee_asset: None,
            maximum_native_fee_atoms: None,
            proxy_core_usdc_before_atoms: Some(proxy_core_usdc_before_atoms),
            core_owner_usdc_before_atoms: Some(core_owner_usdc_before_atoms),
            core_sweep_from_block: Some(core_sweep_from_block),
            solana_owner: None,
        })
    }

    async fn core_available_usdc_atoms(
        &self,
        owner: &str,
        account_mode: gofin_hyperliquid::funding::HyperliquidAccountMode,
    ) -> Result<u64, GoFinError> {
        let value = self
            .hyperliquid_transport
            .info(
                gofin_exchange::ccxt::HyperliquidInfoRequest::SpotClearinghouseState {
                    owner: owner.to_ascii_lowercase(),
                },
            )
            .await?;
        let balances = value
            .get("balances")
            .and_then(Value::as_array)
            .ok_or_else(|| GoFinError::validation("Hyperliquid spot state omitted balances."))?;
        let mut matching = Vec::new();
        for balance in balances {
            if balance.get("coin").and_then(Value::as_str) != Some("USDC")
                && balance.get("token").and_then(Value::as_u64) != Some(0)
            {
                continue;
            }
            if balance.get("coin").and_then(Value::as_str) != Some("USDC")
                || balance.get("token").and_then(Value::as_u64) != Some(0)
            {
                return Err(GoFinError::validation(
                    "Hyperliquid spot USDC token identity is inconsistent.",
                ));
            }
            matching.push(balance);
        }
        if matching.len() > 1 {
            return Err(GoFinError::validation(
                "Hyperliquid spot state contains duplicate native USDC balances.",
            ));
        }
        if account_mode == gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount {
            if matching.len() != 1 {
                return Err(GoFinError::validation(
                    "Unified Hyperliquid state omitted the exact native USDC balance identity.",
                ));
            }
            let rows = value
                .get("tokenToAvailableAfterMaintenance")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Unified Hyperliquid state omitted available USDC after maintenance.",
                    )
                })?;
            let mut matches = rows.iter().filter_map(|row| {
                let pair = row.as_array()?;
                (pair.first()?.as_u64()? == 0)
                    .then(|| pair.get(1)?.as_str())
                    .flatten()
            });
            let amount = matches.next().ok_or_else(|| {
                GoFinError::validation(
                    "Unified Hyperliquid state omitted native USDC available capacity.",
                )
            })?;
            if matches.next().is_some() {
                return Err(GoFinError::validation(
                    "Unified Hyperliquid state returned duplicate USDC maintenance capacity.",
                ));
            }
            return parse_hyperliquid_usdc_atoms(amount);
        }
        let Some(balance) = matching.first() else {
            return Ok(0);
        };
        let total = balance
            .get("total")
            .and_then(Value::as_str)
            .ok_or_else(|| GoFinError::validation("Hyperliquid USDC balance omitted its total."))?;
        let hold = balance.get("hold").and_then(Value::as_str).unwrap_or("0");
        let total = gofin_domain::Quantity::from_decimal(total, 8)
            .map_err(|_| GoFinError::validation("Hyperliquid USDC total is malformed."))?;
        let hold = gofin_domain::Quantity::from_decimal(hold, 8)
            .map_err(|_| GoFinError::validation("Hyperliquid USDC hold is malformed."))?;
        let available = total.checked_sub(&hold).map_err(|_| {
            GoFinError::validation("Hyperliquid USDC hold exceeds the spot balance.")
        })?;
        u64::try_from(available.integer() / 100u32).map_err(|_| {
            GoFinError::validation("Hyperliquid USDC balance exceeds supported range.")
        })
    }

    async fn core_action_evidence(
        &self,
        plan: &BoundFunding,
        nonce: u64,
    ) -> Result<Option<Value>, GoFinError> {
        let leg = plan.legs.first().ok_or_else(|| {
            GoFinError::validation("Core deposit plan omitted its source action.")
        })?;
        let (mode, destination, amount, _) = Self::core_action_fields(leg, plan)?;
        let action_type = if mode == "default" {
            "spotSend"
        } else {
            "sendAsset"
        };
        let hash_key = Self::record_key(&plan.legs[0].submission_id, "discovered_action_hash");
        let known_hash = self
            .credentials
            .papertrade_runtime_record(&hash_key)?
            .and_then(|record| {
                record
                    .get("hash")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let hash = if let Some(hash) = known_hash {
            hash
        } else {
            let history = self
                .hyperliquid_transport
                .explorer(
                    gofin_exchange::ccxt::HyperliquidExplorerRequest::UserDetails {
                        user: plan.intent.owner.to_ascii_lowercase(),
                    },
                )
                .await?;
            let txs = history
                .get("txs")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    GoFinError::validation("Hyperliquid explorer omitted owner action history.")
                })?;
            let candidates = txs
                .iter()
                .filter(|tx| {
                    let action = &tx["action"];
                    tx["user"]
                        .as_str()
                        .is_some_and(|user| user.eq_ignore_ascii_case(&plan.intent.owner))
                        && (action["nonce"].as_u64() == Some(nonce)
                            || action["time"].as_u64() == Some(nonce))
                        && action["type"].as_str() == Some(action_type)
                        && action["destination"]
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case(&destination))
                        && action["amount"].as_str() == Some(amount.as_str())
                        && action["token"].as_str() == Some("USDC")
                        && action["hyperliquidChain"].as_str() == Some("Mainnet")
                        && action["signatureChainId"].as_str() == Some("0x3e7")
                        && (action_type != "sendAsset"
                            || (action["sourceDex"].as_str() == Some("spot")
                                && action["destinationDex"].as_str() == Some("spot")
                                && action["fromSubAccount"].as_str() == Some("")))
                })
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                return Ok(None);
            }
            if candidates.len() != 1 {
                return Err(GoFinError::new(
                    "PAPERTRADE_CORE_ACTION_AMBIGUOUS",
                    "reconciliation",
                    "More than one Hyperliquid action matches the persisted Core deposit nonce.",
                    "Keep the original funding operation unresolved and inspect native action history.",
                ));
            }
            let hash = candidates[0]["hash"]
                .as_str()
                .ok_or_else(|| {
                    GoFinError::validation("Hyperliquid explorer action omitted its hash.")
                })?
                .to_owned();
            self.persist_record(&hash_key, &json!({"version":1,"hash":hash}))?;
            hash
        };
        let detail = self
            .hyperliquid_transport
            .explorer(
                gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails {
                    hash: hash.to_owned(),
                },
            )
            .await?;
        let tx = &detail["tx"];
        let expected_action_type = match &plan.legs[0].action {
            FundingAction::CoreDeposit { action } => match action {
                gofin_hyperliquid::funding::CoreToPapertradeAction::SpotSend(_) => "spotSend",
                gofin_hyperliquid::funding::CoreToPapertradeAction::SendAsset(_) => "sendAsset",
            },
            _ => return Ok(None),
        };
        let core_action = &tx["action"];
        if tx["hash"].as_str() != Some(&hash)
            || !tx["user"]
                .as_str()
                .is_some_and(|user| user.eq_ignore_ascii_case(&plan.intent.owner))
            || core_action["type"].as_str() != Some(expected_action_type)
            || (core_action["nonce"].as_u64() != Some(nonce)
                && core_action["time"].as_u64() != Some(nonce))
            || core_action["hyperliquidChain"].as_str() != Some("Mainnet")
            || core_action["signatureChainId"].as_str() != Some("0x3e7")
            || !core_action["destination"]
                .as_str()
                .is_some_and(|value| value.eq_ignore_ascii_case(&destination))
            || core_action["amount"].as_str() != Some(amount.as_str())
            || core_action["token"].as_str() != Some("USDC")
            || (expected_action_type == "sendAsset"
                && (core_action["sourceDex"].as_str() != Some("spot")
                    || core_action["destinationDex"].as_str() != Some("spot")
                    || core_action["fromSubAccount"].as_str() != Some("")))
        {
            return Ok(None);
        }
        let Some(block) = tx["block"].as_u64().filter(|block| *block != 0) else {
            return Ok(None);
        };
        let end_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| GoFinError::validation("System clock is invalid."))?
            .as_millis() as u64;
        let start_time_ms = nonce.saturating_sub(60_000);
        let source_ledger = self
            .hyperliquid_transport
            .info(
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner: plan.intent.owner.to_ascii_lowercase(),
                    start_time_ms,
                    end_time_ms: Some(end_time_ms),
                },
            )
            .await?;
        let destination_ledger = self
            .hyperliquid_transport
            .info(
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner: destination.to_ascii_lowercase(),
                    start_time_ms,
                    end_time_ms: Some(end_time_ms),
                },
            )
            .await?;
        let source_rows = source_ledger
            .as_array()
            .or_else(|| source_ledger.get("rows").and_then(Value::as_array))
            .ok_or_else(|| GoFinError::validation("Core source ledger response is malformed."))?;
        let destination_rows = destination_ledger
            .as_array()
            .or_else(|| destination_ledger.get("rows").and_then(Value::as_array))
            .ok_or_else(|| {
                GoFinError::validation("Core destination ledger response is malformed.")
            })?;
        let matches_hash = |records: &[Value]| {
            records
                .iter()
                .filter(|row| {
                    row["delta"]["type"].as_str()
                        == Some(if action_type == "sendAsset" {
                            "send"
                        } else {
                            "spotTransfer"
                        })
                        && row["hash"]
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case(&hash))
                        && row["time"]
                            .as_u64()
                            .is_some_and(|time| time >= start_time_ms && time <= end_time_ms)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let source_matches = matches_hash(source_rows);
        let destination_matches = matches_hash(destination_rows);
        if !tx["error"].is_null() {
            // An explorer error is a rejection proof only when it is attached to
            // the exact nonce-bound canonical action and neither account has a
            // ledger effect for that hash. Do not infer rejection from HTTP or
            // venue response text alone.
            if source_matches.is_empty() && destination_matches.is_empty() {
                let has_activation = source_rows
                    .iter()
                    .chain(destination_rows.iter())
                    .any(|row| {
                        row["hash"]
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case(&hash))
                            && row["delta"]["type"].as_str() == Some("accountActivationGas")
                    });
                if !has_activation {
                    return Ok(Some(json!({
                        "kind":"canonical_hyperliquid_action_rejection",
                        "action_hash":hash,
                        "nonce":nonce,
                        "block":block,
                        "action_type":expected_action_type,
                        "error_present":true,
                        "source_ledger_effects":0,
                        "destination_ledger_effects":0
                    })));
                }
            }
            return Ok(None);
        }
        if source_matches.len() != 1 || destination_matches.len() != 1 {
            return Ok(None);
        }
        let source_delta = &source_matches[0]["delta"];
        let destination_delta = &destination_matches[0]["delta"];
        let activation_rows = source_rows
            .iter()
            .chain(destination_rows.iter())
            .filter(|row| {
                row["delta"]["type"].as_str() == Some("accountActivationGas")
                    && row["hash"]
                        .as_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case(&hash))
            })
            .cloned()
            .collect::<Vec<_>>();
        // Hyperliquid's userNonFundingLedgerUpdates endpoint returns the same
        // owner-oriented `send` row when queried for both sender and
        // recipient. The independent proxy-side evidence is the later exact
        // proxy->Exchange spot transfer, followed by the canonical Papertrade
        // Swept credit in prove_destination; do not require a mirrored send
        // row that the venue does not emit.
        let ledger_effects = if source_matches[0] == destination_matches[0] {
            verify_core_send_single_ledger(
                source_delta,
                &plan.intent.owner,
                &destination,
                plan.bounds.source_usdc_atoms,
                plan.bounds.maximum_source_fee_usdc_atoms,
                action_type,
                nonce,
            )
        } else {
            verify_core_send_ledger(
                source_delta,
                destination_delta,
                &plan.intent.owner,
                &destination,
                plan.bounds.source_usdc_atoms,
                plan.bounds.maximum_source_fee_usdc_atoms,
                action_type,
            )
        };
        let Some(ledger_effects) = ledger_effects else {
            return Ok(None);
        };
        if !activation_rows.is_empty() {
            return Ok(None);
        }
        let source_time_ms = source_matches[0]["time"]
            .as_u64()
            .ok_or_else(|| GoFinError::validation("Core source ledger omitted its event time."))?;
        let proxy_outflows = destination_rows
            .iter()
            .filter(|row| {
                let delta = &row["delta"];
                delta["type"].as_str() == Some("spotTransfer")
                    && delta["user"]
                        .as_str()
                        .is_some_and(|v| v.eq_ignore_ascii_case(&destination))
                    && delta["destination"].as_str().is_some_and(|v| {
                        v.eq_ignore_ascii_case("0x6cd5661646289fb6e65ea5c032310fded797d0a2")
                    })
                    && delta["token"].as_str() == Some("USDC")
                    && delta["amount"]
                        .as_str()
                        .and_then(|value| parse_usdc_atoms(value).ok())
                        == parse_usdc_atoms(&amount).ok()
                    && row["time"]
                        .as_u64()
                        .is_some_and(|time| time >= source_time_ms)
            })
            .cloned()
            .collect::<Vec<_>>();
        if proxy_outflows.len() != 1 {
            return Ok(None);
        }
        Ok(Some(json!({
            "kind":"hyperliquid_core_usdc_debit",
            "action_hash":hash,
            "action":tx["action"],
            "block":tx["block"],
            "source_time_ms":source_time_ms,
            "source_ledger":source_matches[0],
            "destination_ledger":destination_matches[0],
            "proxy_sweep_transfer":proxy_outflows[0],
            "source_debit_atoms":ledger_effects.source_debit_atoms,
            "source_fee_atoms":ledger_effects.source_fee_atoms,
            "destination_credit_atoms":ledger_effects.destination_credit_atoms,
            "activation_fee_atoms":0,
        })))
    }

    async fn reserve_core_nonce(&self, submission_id: &str) -> Result<u64, GoFinError> {
        let key_prefix = format!(
            "papertrade-funding-nonce:{}:",
            self.native
                .owner
                .trim_start_matches("0x")
                .to_ascii_lowercase()
        );
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| GoFinError::validation("System clock is invalid."))?
            .as_millis() as u64;
        for offset in 0..256u64 {
            let nonce = now_ms.saturating_add(offset);
            let key = format!("{key_prefix}{nonce}");
            let value = json!({"version":1,"submission_id":submission_id,"nonce":nonce});
            match self.credentials.put_papertrade_runtime_record(&key, &value) {
                Ok(()) => return Ok(nonce),
                Err(error) if error.code == "PAPERTRADE_IDENTITY_CONFLICT" => continue,
                Err(error) => return Err(error),
            }
        }
        Err(GoFinError::validation(
            "Could not reserve a unique Hyperliquid funding nonce.",
        ))
    }

    fn unavailable<T>() -> Result<T, GoFinError> {
        NativeHyperEvmFundingServices::unsupported()
    }

    fn record_key(submission_id: &str, suffix: &str) -> String {
        format!(
            "papertrade-funding:{}:{suffix}",
            hex::encode(submission_id.as_bytes())
        )
    }

    fn persist_record(&self, key: &str, value: &Value) -> Result<(), GoFinError> {
        match self.credentials.put_papertrade_runtime_record(key, value) {
            Ok(()) => Ok(()),
            Err(error) if error.code == "PAPERTRADE_IDENTITY_CONFLICT" => {
                let existing = self.credentials.papertrade_runtime_record(key)?;
                if existing.as_ref() == Some(value) {
                    Ok(())
                } else {
                    Err(GoFinError::validation(
                        "Funding dispatch identity conflicts with its durable record.",
                    ))
                }
            }
            Err(error) => Err(error),
        }
    }

    fn sanitized_core_dispatch_outcome(result: &Result<Value, GoFinError>) -> Value {
        match result {
            Ok(response) => {
                let status = response
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                // Native response text can contain arbitrary venue material. Keep
                // only bounded classifications; canonical reconciliation is the
                // source of truth for acceptance or rejection.
                json!({
                    "version":1,
                    "kind":"venue_response",
                    "status":if status.len() <= 24 { status } else { "other" },
                    "response_present":response.get("response").is_some(),
                })
            }
            Err(error) => {
                let details = error.details.as_deref();
                let http_status = details
                    .and_then(|value| value.get("http_status"))
                    .and_then(Value::as_u64)
                    .filter(|status| *status <= u16::MAX as u64);
                let safe_code = |candidate: &str| {
                    (!candidate.is_empty()
                        && candidate.len() <= 64
                        && candidate.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
                        }))
                    .then(|| candidate.to_owned())
                };
                let exchange_code = details
                    .and_then(|value| value.get("exchange_code"))
                    .and_then(Value::as_str)
                    .and_then(safe_code);
                let code = safe_code(&error.code)
                    .unwrap_or_else(|| "SANITIZED_TRANSPORT_ERROR".to_owned());
                let stage = match error.stage.as_str() {
                    "submission" | "authentication" | "signing" | "read" | "validation" => {
                        error.stage.as_str()
                    }
                    _ => "other",
                };
                json!({
                    "version":1,
                    "kind":"transport_error",
                    "code":code,
                    "stage":stage,
                    "http_status":http_status,
                    "exchange_code":exchange_code,
                })
            }
        }
    }

    fn core_action_fields(
        leg: &FundingLeg,
        plan: &BoundFunding,
    ) -> Result<(String, String, String, u64), GoFinError> {
        let FundingAction::CoreDeposit { action } = &leg.action else {
            return Err(GoFinError::validation("Funding leg is not a Core deposit."));
        };
        let (mode, destination, token, amount, nonce) = match action {
            gofin_hyperliquid::funding::CoreToPapertradeAction::SpotSend(action) => (
                "default",
                action.destination.as_str(),
                action.token.as_str(),
                action.amount.as_str(),
                action.time,
            ),
            gofin_hyperliquid::funding::CoreToPapertradeAction::SendAsset(action) => {
                if action.action_type != "sendAsset"
                    || action.hyperliquid_chain != "Mainnet"
                    || action.signature_chain_id != "0x3e7"
                    || action.source_dex != "spot"
                    || action.destination_dex != "spot"
                    || !action.from_sub_account.is_empty()
                {
                    return Err(GoFinError::validation(
                        "Core deposit sendAsset action differs from the reviewed mainnet shape.",
                    ));
                }
                (
                    "unified_account",
                    action.destination.as_str(),
                    action.token.as_str(),
                    action.amount.as_str(),
                    action.nonce,
                )
            }
        };
        if token != "USDC"
            || !destination.eq_ignore_ascii_case(&plan.proxy)
            || nonce == 0
            || parse_usdc_atoms(amount)? != plan.intent.amount_usdc_atoms
        {
            return Err(GoFinError::validation(
                "Core deposit action differs from the exact persisted route binding.",
            ));
        }
        Ok((
            mode.to_owned(),
            destination.to_owned(),
            amount.to_owned(),
            nonce,
        ))
    }

    pub(crate) fn core_action_fingerprint(leg: &FundingLeg) -> Result<String, GoFinError> {
        if !matches!(leg.action, FundingAction::CoreDeposit { .. }) {
            return Err(GoFinError::validation(
                "Nonce invalidation fingerprint requires an exact Core deposit action.",
            ));
        }
        let bytes = serde_json::to_vec(&leg.action)
            .map_err(|_| GoFinError::validation("Core action fingerprint could not be encoded."))?;
        Ok(format!(
            "{:#x}",
            gofin_evm::alloy::primitives::keccak256(bytes)
        ))
    }

    #[allow(clippy::too_many_arguments)] // Child authority is independently bound to the exact parent and journal.
    async fn dispatch_core_deposit_nonce_noop(
        &self,
        parent_operation_id: &str,
        parent_record_id: &str,
        expected_owner: &str,
        expected_nonce: u64,
        child_operation_id: &str,
        child_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value, GoFinError> {
        journal.assert_candidate(validated.prepared())?;
        validated.assert_fresh()?;
        if child_operation_id.is_empty()
            || child_operation_id.len() > 256
            || !child_operation_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_:".contains(&byte))
            || child_submission.kind.is_empty()
            || child_submission.reference.is_empty()
            || child_submission.reference.len() > 512
            || parent_record_id.is_empty()
            || parent_record_id.len() > 128
            || validated.prepared().operation != "papertrade.deposit.invalidate_nonce"
        {
            return Err(GoFinError::validation(
                "The approved nonce-recovery child identity is malformed.",
            ));
        }
        let plan_value = self
            .credentials
            .papertrade_runtime_record(&format!("papertrade-funding-plan:{parent_operation_id}"))?
            .ok_or_else(|| GoFinError::validation("The original funding plan is unavailable."))?;
        let plan: BoundFunding = serde_json::from_value(plan_value)
            .map_err(|_| GoFinError::validation("The original funding plan is malformed."))?;
        if plan.operation_id != parent_operation_id
            || plan.intent.route != FundingRoute::Core
            || plan.intent.direction != FundingDirection::Deposit
            || !plan.intent.owner.eq_ignore_ascii_case(expected_owner)
            || !plan
                .intent
                .owner
                .eq_ignore_ascii_case(&self.hyperliquid_account.owner)
        {
            return Err(GoFinError::validation(
                "The recovery child does not match the protected Core deposit owner and route.",
            ));
        }
        let leg = plan
            .legs
            .first()
            .filter(|leg| {
                leg.submission_id == format!("papertrade-funding:{parent_operation_id}:0")
            })
            .ok_or_else(|| {
                GoFinError::validation("The protected Core deposit leg is unavailable.")
            })?;
        let (_, _, _, nonce) = Self::core_action_fields(leg, &plan)?;
        if nonce != expected_nonce {
            return Err(GoFinError::validation(
                "The recovery nonce differs from the exact protected Core deposit action.",
            ));
        }
        let fingerprint = Self::core_action_fingerprint(leg)?;
        let attempted_key = Self::record_key(&leg.submission_id, "dispatch_attempted");
        let attempted = self
            .credentials
            .papertrade_runtime_record(&attempted_key)?
            .ok_or_else(|| {
                GoFinError::validation("The original Core deposit has no durable dispatch attempt.")
            })?;
        if attempted["nonce"].as_u64() != Some(expected_nonce) {
            return Err(GoFinError::validation(
                "The durable Core deposit attempt does not match the recovery nonce.",
            ));
        }
        let recovery: gofin_papertrade::CoreDepositNonceRecoveryBinding = serde_json::from_value(
            validated
                .prepared()
                .binding
                .get("core_deposit_nonce_recovery")
                .cloned()
                .ok_or_else(|| {
                    GoFinError::validation("The approved Core Noop binding is missing.")
                })?,
        )
        .map_err(|_| GoFinError::validation("The approved Core Noop binding is malformed."))?;
        recovery.validate()?;
        let parent_link =
            gofin_domain::RecoveryBinding::from_prepared_binding(&validated.prepared().binding)?
                .ok_or_else(|| {
                    GoFinError::validation("The approved Noop has no durable parent link.")
                })?;
        use sha2::Digest;
        let serialized = serde_json::to_vec(&(&plan, leg)).map_err(|_| {
            GoFinError::validation("Funding plan fingerprint could not be verified.")
        })?;
        let plan_fingerprint = format!("0x{}", hex::encode(sha2::Sha256::digest(serialized)));
        if recovery.child_operation_id != child_operation_id
            || recovery.parent_operation_id != parent_record_id
            || recovery.route_id != parent_operation_id
            || !recovery.owner.eq_ignore_ascii_case(&plan.intent.owner)
            || recovery.plan_fingerprint != plan_fingerprint
            || parent_link.parent_operation_id != parent_record_id
            || parent_link.parent_submission != recovery.parent_submission
            || attempted["outer_submission"]
                != serde_json::to_value(&recovery.parent_submission).unwrap_or(Value::Null)
        {
            return Err(GoFinError::validation(
                "The approved Noop does not bind this exact Core deposit submission.",
            ));
        }
        validated.assert_fresh()?;
        journal.assert_authorized()?;
        let attempt_key = Self::record_key(&leg.submission_id, "nonce_invalidation_attempted");
        if self
            .credentials
            .papertrade_runtime_record(&attempt_key)?
            .is_some()
        {
            return Err(GoFinError::new(
                "PAPERTRADE_CORE_NONCE_INVALIDATION_ALREADY_ATTEMPTED",
                "recovery",
                "A Noop recovery attempt already exists for this Core deposit nonce.",
                "Reconcile the existing child; never dispatch a second Noop or replay the deposit.",
            ));
        }
        self.persist_record(
            &attempt_key,
            &json!({
                "version":1,
                "kind":"hyperliquid_core_deposit_nonce_invalidation_attempted",
                "parent_operation_id":parent_record_id,
                "funding_plan_id":parent_operation_id,
                "owner":plan.intent.owner.to_ascii_lowercase(),
                "nonce":expected_nonce,
                "action_fingerprint":fingerprint,
                "child_operation_id":child_operation_id,
                "child_submission_id":child_submission.reference
            }),
        )?;
        let envelope = gofin_exchange::ccxt::HyperliquidActionEnvelope {
            nonce_ms: expected_nonce,
            vault_address: self.hyperliquid_account.vault_address().map(str::to_owned),
            action: gofin_exchange::ccxt::HyperliquidAction::Noop {},
        };
        let response = self
            .hyperliquid_transport
            .dispatch(&self.hyperliquid_account, &envelope, journal)
            .await;
        let sanitized = match &response {
            Ok(value) => json!({
                "version":1,
                "kind":"venue_response",
                "status":value.get("status").and_then(Value::as_str).filter(|status| status.len() <= 24).unwrap_or("other"),
                "response_type":value.pointer("/response/type").and_then(Value::as_str).filter(|value| value.len() <= 24).unwrap_or("other")
            }),
            Err(error) => json!({
                "version":1,
                "kind":"transport_error",
                "code":if error.code.len() <= 64 && error.code.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') { error.code.as_str() } else { "SANITIZED_TRANSPORT_ERROR" },
                "stage":match error.stage.as_str() { "submission"|"authentication"|"signing"|"read"|"validation" => error.stage.as_str(), _ => "other" }
            }),
        };
        self.persist_record(
            &Self::record_key(&leg.submission_id, "nonce_invalidation_response"),
            &sanitized,
        )?;
        match response {
            Ok(value)
                if value.as_object().is_some_and(|object| object.len() == 2)
                    && value.get("status").and_then(Value::as_str) == Some("ok")
                    && value
                        .pointer("/response")
                        .and_then(Value::as_object)
                        .is_some_and(|object| {
                            object.len() == 1
                                && object.get("type").and_then(Value::as_str) == Some("default")
                        }) =>
            {
                let proof = json!({
                    "version":1,
                    "kind":"hyperliquid_core_deposit_nonce_invalidated",
                    "parent_operation_id":parent_record_id,
                    "funding_plan_id":parent_operation_id,
                    "owner":plan.intent.owner.to_ascii_lowercase(),
                    "nonce":expected_nonce,
                    "action_fingerprint":fingerprint,
                    "child_operation_id":child_operation_id,
                    "child_submission_id":child_submission.reference
                });
                self.persist_record(
                    &Self::record_key(&leg.submission_id, "nonce_invalidation_proof"),
                    &proof,
                )?;
                Ok(json!({
                    "kind":"hyperliquid_noop_nonce_consumed",
                    "owner":plan.intent.owner.to_ascii_lowercase(),
                    "nonce":expected_nonce,
                    "response_status":"ok",
                    "response_type":"default",
                    "action_fingerprint":fingerprint,
                    "child_operation_id":child_operation_id,
                    "child_submission_id":child_submission.reference
                }))
            }
            Ok(_) => Err(GoFinError::new(
                "HYPERLIQUID_NONCE_RECOVERY_UNRESOLVED",
                "recovery",
                "The Noop response did not prove nonce consumption.",
                "Reconcile this exact recovery child; do not create another Noop or replay the deposit.",
            )),
            Err(error) => Err(error),
        }
    }
}

#[async_trait::async_trait]
impl PapertradeFundingServices for MultiRouteFundingServices {
    fn attach_runtime(
        &self,
        runtime: std::sync::Weak<crate::papertrade_runtime::BrokerPapertradeRuntime>,
    ) -> Result<(), GoFinError> {
        if let Some(other) = &self.other {
            other.attach_runtime(runtime)?;
        }
        Ok(())
    }

    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        if let Some(other) = &self.other {
            other.attach_execution_state(state)?;
        }
        Ok(())
    }

    async fn preflight(&self, intent: &FundingIntent) -> Result<FundingPreflight, GoFinError> {
        if intent.route == FundingRoute::Core && intent.direction == FundingDirection::Deposit {
            self.core_preflight(intent).await
        } else if intent.route == FundingRoute::Core
            && intent.direction == FundingDirection::Withdraw
        {
            // Core withdrawals still need the factory-derived Papertrade proxy
            // for the typed owner-signed WithdrawToCore intent. Keep this
            // common chain-999 identity check here; the route service owns the
            // signed intent, dispatch, and canonical settlement proof.
            self.native.preflight(intent).await
        } else if intent.route == FundingRoute::Hyperevm
            && intent.direction == FundingDirection::Deposit
        {
            self.native.preflight(intent).await
        } else {
            self.other()?.preflight(intent).await
        }
    }

    async fn check_proxy_registration(
        &self,
        operation_id: &str,
        owner: &str,
        factory_proxy: &str,
    ) -> Result<ProxyRegistrationProof, GoFinError> {
        self.native
            .check_proxy_registration(operation_id, owner, factory_proxy)
            .await
    }

    async fn cctp_quote(&self, request: &CctpQuoteRequest) -> Result<FundingCctpQuote, GoFinError> {
        self.other()?.cctp_quote(request).await
    }

    async fn prepare_evm_call(
        &self,
        submission_id: &str,
        intent: &FundingIntent,
        call: &EvmCall,
        maximum_total_fee_atoms: &str,
    ) -> Result<gofin_evm::FrozenTransaction, GoFinError> {
        if intent.route == FundingRoute::Hyperevm && intent.direction == FundingDirection::Deposit {
            self.native
                .prepare_evm_call(submission_id, intent, call, maximum_total_fee_atoms)
                .await
        } else {
            self.other()?
                .prepare_evm_call(submission_id, intent, call, maximum_total_fee_atoms)
                .await
        }
    }

    async fn solana_burn_request(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
        metadata: &cctp::CctpQuoteMetadata,
        proxy: &str,
    ) -> Result<cctp::solana::hypercore_source::BurnRequest, GoFinError> {
        self.other()?
            .solana_burn_request(operation_id, intent, metadata, proxy)
            .await
    }

    async fn prepare_solana_burn_transaction(
        &self,
        submission_id: &str,
        instruction: &gofin_solana::NormalizedInstruction,
        maximum_network_fee_lamports: u64,
    ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError> {
        self.other()?
            .prepare_solana_burn_transaction(
                submission_id,
                instruction,
                maximum_network_fee_lamports,
            )
            .await
    }

    async fn reserve_hyperliquid_nonce(&self, submission_id: &str) -> Result<u64, GoFinError> {
        self.reserve_core_nonce(submission_id).await
    }

    async fn papertrade_withdraw_to_core_intent(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
    ) -> Result<gofin_papertrade::BoundIntent, GoFinError> {
        self.other()?
            .papertrade_withdraw_to_core_intent(operation_id, intent)
            .await
    }

    async fn persist_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        self.native.persist_plan(plan).await
    }

    async fn assert_persisted_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        self.native.assert_persisted_plan(plan).await
    }

    async fn dispatch_persisted_leg(
        &self,
        leg: &FundingLeg,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<FundingLegState, GoFinError> {
        if !matches!(leg.action, FundingAction::CoreDeposit { .. }) {
            let plan = load_plan_for_submission(self.credentials.as_ref(), &leg.submission_id)?;
            if plan.intent.route == FundingRoute::Hyperevm
                && plan.intent.direction == FundingDirection::Deposit
            {
                return self
                    .native
                    .dispatch_persisted_leg(leg, outer_submission, journal, validated)
                    .await;
            }
            return self
                .other()?
                .dispatch_persisted_leg(leg, outer_submission, journal, validated)
                .await;
        }
        journal.assert_authorized()?;
        let plan = load_plan_for_submission(self.credentials.as_ref(), &leg.submission_id)?;
        self.native.assert_persisted_plan(&plan).await?;
        if plan.intent.route != FundingRoute::Core
            || plan.intent.direction != FundingDirection::Deposit
            || !plan
                .intent
                .owner
                .eq_ignore_ascii_case(&self.hyperliquid_account.owner)
        {
            return Err(GoFinError::validation(
                "Core deposit binding differs from the enrolled Hyperliquid owner.",
            ));
        }
        let (account_mode, destination, amount, nonce) = Self::core_action_fields(leg, &plan)?;
        if plan.core_owner_usdc_before_atoms.is_none()
            || plan.proxy_core_usdc_before_atoms != Some(0)
            || plan.core_sweep_from_block.is_none()
        {
            return Err(GoFinError::validation(
                "Core deposit is missing its persisted balance and sweep baselines.",
            ));
        }
        let current_mode = gofin_hyperliquid::read_account_mode(
            self.hyperliquid_transport.as_ref(),
            &self.hyperliquid_account,
        )
        .await?;
        let expected_mode = match account_mode.as_str() {
            "default" => gofin_hyperliquid::AccountMode::Separate,
            "unified_account" => gofin_hyperliquid::AccountMode::Unified,
            _ => {
                return Err(GoFinError::validation(
                    "Persisted Core deposit account mode is unsupported.",
                ));
            }
        };
        if current_mode.mode != expected_mode {
            return Err(GoFinError::new(
                "HYPERLIQUID_ACCOUNT_MODE_CHANGED",
                "preflight",
                "The enrolled Hyperliquid account mode changed after funding was prepared.",
                "Prepare a new Papertrade funding operation using the current account mode.",
            ));
        }
        let current_available = self
            .core_available_usdc_atoms(
                &plan.intent.owner,
                if expected_mode == gofin_hyperliquid::AccountMode::Unified {
                    gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount
                } else {
                    gofin_hyperliquid::funding::HyperliquidAccountMode::Default
                },
            )
            .await?;
        if !core_available_covers_source_bound(
            current_available,
            plan.bounds.source_usdc_atoms,
            plan.bounds.maximum_source_fee_usdc_atoms,
        ) {
            plan.bounds
                .source_usdc_atoms
                .checked_add(plan.bounds.maximum_source_fee_usdc_atoms)
                .ok_or_else(|| GoFinError::validation("Core source debit bound overflows."))?;
            return Err(GoFinError::new(
                "HYPERCORE_USDC_INSUFFICIENT",
                "preflight",
                "The enrolled HyperCore spot wallet no longer has enough available USDC.",
                "Refresh the Papertrade funding preview before submitting.",
            ));
        }
        if self
            .native
            .require_zero_proxy_spot_balance(&plan.proxy)
            .await?
            != 0
        {
            return Err(GoFinError::new(
                "PAPERTRADE_PROXY_BALANCE_AMBIGUOUS",
                "preflight",
                "The Papertrade proxy received USDC after this funding operation was prepared.",
                "Reconcile the original operation before starting another deposit.",
            ));
        }
        let attempted_key = Self::record_key(&leg.submission_id, "dispatch_attempted");
        if self
            .credentials
            .papertrade_runtime_record(&attempted_key)?
            .is_some()
        {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Core deposit dispatch was already attempted; reconcile original action"}),
            });
        }
        self.persist_record(
            &attempted_key,
            &json!({"version":1,"outer_submission":outer_submission,"nonce":nonce,"destination":destination,"amount":amount}),
        )?;
        let action = match &leg.action {
            FundingAction::CoreDeposit { .. } => {
                gofin_exchange::ccxt::HyperliquidAction::PapertradeCoreDeposit {
                    account_mode,
                    destination,
                    amount,
                    nonce,
                }
            }
            _ => unreachable!("matched CoreDeposit above"),
        };
        let envelope = gofin_exchange::ccxt::HyperliquidActionEnvelope {
            nonce_ms: nonce,
            vault_address: None,
            action,
        };
        let dispatch_result = self
            .hyperliquid_transport
            .dispatch(&self.hyperliquid_account, &envelope, journal)
            .await;
        let safe_outcome = Self::sanitized_core_dispatch_outcome(&dispatch_result);
        let response_key = Self::record_key(&leg.submission_id, "dispatch_response");
        self.persist_record(&response_key, &safe_outcome)?;
        match dispatch_result {
            Ok(response) if response["status"] == "ok" => Ok(FundingLegState::Submitted {
                evidence: json!({"kind":"hyperliquid_core_deposit","nonce":nonce,"response_status":"ok"}),
            }),
            Ok(_) | Err(_) => Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Core deposit submission outcome is unresolved; reconcile original action","dispatch_outcome":safe_outcome}),
            }),
        }
    }

    async fn reconcile_persisted_leg(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
    ) -> Result<FundingLegState, GoFinError> {
        if !matches!(leg.action, FundingAction::CoreDeposit { .. }) {
            if plan.intent.route == FundingRoute::Hyperevm
                && plan.intent.direction == FundingDirection::Deposit
            {
                return self.native.reconcile_persisted_leg(plan, leg).await;
            }
            return self.other()?.reconcile_persisted_leg(plan, leg).await;
        }
        let attempted_key = Self::record_key(&leg.submission_id, "dispatch_attempted");
        let Some(attempted) = self.credentials.papertrade_runtime_record(&attempted_key)? else {
            return Ok(FundingLegState::NotSubmitted);
        };
        let nonce = attempted["nonce"]
            .as_u64()
            .ok_or_else(|| GoFinError::validation("Persisted Core action nonce is malformed."))?;
        let invalidation_key = Self::record_key(&leg.submission_id, "nonce_invalidation_proof");
        if let Some(proof) = self
            .credentials
            .papertrade_runtime_record(&invalidation_key)?
        {
            let expected_fingerprint = Self::core_action_fingerprint(leg)?;
            if proof["version"].as_u64() != Some(1)
                || proof["kind"].as_str() != Some("hyperliquid_core_deposit_nonce_invalidated")
                || proof["owner"]
                    .as_str()
                    .is_none_or(|owner| !owner.eq_ignore_ascii_case(&plan.intent.owner))
                || proof["nonce"].as_u64() != Some(nonce)
                || proof["action_fingerprint"].as_str() != Some(expected_fingerprint.as_str())
                || proof["parent_operation_id"]
                    .as_str()
                    .is_none_or(str::is_empty)
                || proof["funding_plan_id"].as_str() != Some(plan.operation_id.as_str())
                || proof["child_operation_id"]
                    .as_str()
                    .is_none_or(str::is_empty)
                || proof["child_submission_id"]
                    .as_str()
                    .is_none_or(str::is_empty)
            {
                return Err(GoFinError::validation(
                    "Persisted Core nonce invalidation proof differs from the exact funding action.",
                ));
            }
            return Ok(FundingLegState::Rejected {
                evidence: json!({
                    "kind":"hyperliquid_core_deposit_nonce_invalidated",
                    "owner":plan.intent.owner.to_ascii_lowercase(),
                    "nonce":nonce,
                    "action_fingerprint":expected_fingerprint,
                    "child_operation_id":proof["child_operation_id"],
                    "child_submission_id":proof["child_submission_id"]
                }),
            });
        }
        let Some(evidence) = self.core_action_evidence(plan, nonce).await? else {
            return Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"Exact nonce-bound Core action and two-sided ledger proof are not yet available; do not replay","nonce":nonce}),
            });
        };
        if evidence["kind"] == "canonical_hyperliquid_action_rejection" {
            Ok(FundingLegState::Rejected { evidence })
        } else {
            Ok(FundingLegState::Settled { evidence })
        }
    }

    async fn prove_destination(
        &self,
        plan: &BoundFunding,
        settled_legs: &[FundingLegState],
    ) -> Result<Option<FundingReceipt>, GoFinError> {
        if matches!(
            (&plan.intent.route, &plan.intent.direction),
            (&FundingRoute::Core, &FundingDirection::Deposit)
                | (&FundingRoute::Hyperevm, &FundingDirection::Deposit)
        ) {
            self.native.prove_destination(plan, settled_legs).await
        } else {
            self.other()?.prove_destination(plan, settled_legs).await
        }
    }

    async fn invalidate_core_deposit_nonce(
        &self,
        parent_operation_id: &str,
        parent_record_id: &str,
        expected_owner: &str,
        expected_nonce: u64,
        child_operation_id: &str,
        child_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value, GoFinError> {
        self.dispatch_core_deposit_nonce_noop(
            parent_operation_id,
            parent_record_id,
            expected_owner,
            expected_nonce,
            child_operation_id,
            child_submission,
            validated,
            journal,
        )
        .await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CoreSendLedgerEffects {
    pub(crate) source_debit_atoms: u64,
    pub(crate) source_fee_atoms: u64,
    pub(crate) source_native_fee_atoms: u64,
    pub(crate) destination_credit_atoms: u64,
}

/// Validate the two owner-scoped ledger views of one direct HyperCore send.
/// The action amount is credited in full; `fee` is a separate source debit.
fn verify_core_send_ledger(
    source: &Value,
    destination: &Value,
    owner: &str,
    proxy: &str,
    amount_atoms: u64,
    maximum_source_fee_atoms: u64,
    action_type: &str,
) -> Option<CoreSendLedgerEffects> {
    let source_amount = source["amount"]
        .as_str()
        .and_then(|v| gofin_domain::Quantity::from_decimal(v, 6).ok())?;
    let destination_amount = destination["amount"]
        .as_str()
        .and_then(|v| gofin_domain::Quantity::from_decimal(v, 6).ok())?;
    let source_fee = source["fee"]
        .as_str()
        .and_then(|v| gofin_domain::Quantity::from_decimal(v, 6).ok())?;
    let destination_fee = destination["fee"]
        .as_str()
        .and_then(|v| gofin_domain::Quantity::from_decimal(v, 6).ok())?;
    if source_amount != gofin_domain::Quantity::from_u64(amount_atoms)
        || destination_amount != gofin_domain::Quantity::from_u64(amount_atoms)
        || source_fee != destination_fee
    {
        return None;
    }
    let fee_atoms = u64::try_from(source_fee.integer()).ok()?;
    let endpoints_match = source["user"]
        .as_str()
        .is_some_and(|v| v.eq_ignore_ascii_case(owner))
        && source["destination"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(proxy))
        && destination["user"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(proxy))
        && destination["destination"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(owner));
    let dex_match = action_type != "sendAsset"
        || (source["sourceDex"].as_str() == Some("spot")
            && source["destinationDex"].as_str() == Some("spot")
            && destination["sourceDex"].as_str() == Some("spot")
            && destination["destinationDex"].as_str() == Some("spot"));
    let fee_token_match = if fee_atoms == 0 {
        source["feeToken"]
            .as_str()
            .is_some_and(|v| v.is_empty() || v == "USDC")
            && destination["feeToken"]
                .as_str()
                .is_some_and(|v| v.is_empty() || v == "USDC")
    } else {
        source["feeToken"].as_str() == Some("USDC")
            && destination["feeToken"].as_str() == Some("USDC")
    };
    if !endpoints_match
        || !dex_match
        || source["token"].as_str() != Some("USDC")
        || destination["token"].as_str() != Some("USDC")
        || source["usdcValue"].as_str() != source["amount"].as_str()
        || destination["usdcValue"].as_str() != destination["amount"].as_str()
        || !decimal_is_zero(source["nativeTokenFee"].as_str()?, 18)
        || !decimal_is_zero(destination["nativeTokenFee"].as_str()?, 18)
        || !fee_token_match
        || fee_atoms > maximum_source_fee_atoms
    {
        return None;
    }
    Some(CoreSendLedgerEffects {
        source_debit_atoms: amount_atoms.checked_add(fee_atoms)?,
        source_fee_atoms: fee_atoms,
        source_native_fee_atoms: 0,
        destination_credit_atoms: amount_atoms,
    })
}

fn decimal_is_zero(value: &str, decimals: u8) -> bool {
    gofin_domain::Quantity::from_decimal(value, decimals)
        .is_ok_and(|quantity| quantity.integer().to_string() == "0")
}

/// Validate the owner-oriented ledger row returned for unified `sendAsset`
/// actions. Hyperliquid returns the same canonical row for sender and
/// recipient queries, so the later proxy outflow and Papertrade Swept event
/// provide the independent destination-side credit evidence.
pub(crate) fn verify_core_send_single_ledger(
    row: &Value,
    owner: &str,
    proxy: &str,
    amount_atoms: u64,
    maximum_source_fee_atoms: u64,
    action_type: &str,
    nonce: u64,
) -> Option<CoreSendLedgerEffects> {
    verify_core_send_single_ledger_with_native_fee(
        row,
        owner,
        proxy,
        amount_atoms,
        maximum_source_fee_atoms,
        0,
        action_type,
        nonce,
    )
}

/// Validate the owner-oriented sendAsset/spotSend row while allowing either
/// the separately bounded USDC fee or an additional bounded HYPE fee. The
/// venue charges exactly one fee asset per action; callers must value both
/// possible fee assets before dispatch because the action itself has no fee
/// asset selector.
#[allow(clippy::too_many_arguments)] // Verifies a closed ledger row against each independent fee and identity bound.
pub(crate) fn verify_core_send_single_ledger_with_native_fee(
    row: &Value,
    owner: &str,
    proxy: &str,
    amount_atoms: u64,
    maximum_source_fee_atoms: u64,
    maximum_native_fee_atoms: u64,
    action_type: &str,
    nonce: u64,
) -> Option<CoreSendLedgerEffects> {
    let amount = row["amount"]
        .as_str()
        .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())?;
    let fee = row["fee"]
        .as_str()
        .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())?;
    let fee_atoms = u64::try_from(fee.integer()).ok()?;
    let native_fee_atoms = row["nativeTokenFee"]
        .as_str()
        .and_then(|value| gofin_domain::Quantity::from_decimal(value, 8).ok())
        .and_then(|value| u64::try_from(value.integer()).ok())?;
    let expected_type = if action_type == "sendAsset" {
        "send"
    } else {
        "spotTransfer"
    };
    if row["type"].as_str() != Some(expected_type)
        || row["user"]
            .as_str()
            .is_none_or(|value| !value.eq_ignore_ascii_case(owner))
        || row["destination"]
            .as_str()
            .is_none_or(|value| !value.eq_ignore_ascii_case(proxy))
        || row["token"].as_str() != Some("USDC")
        || amount != gofin_domain::Quantity::from_u64(amount_atoms)
        || row["usdcValue"]
            .as_str()
            .and_then(|value| gofin_domain::Quantity::from_decimal(value, 6).ok())
            != Some(amount)
        || row["nonce"].as_u64() != Some(nonce)
        || native_fee_atoms > maximum_native_fee_atoms
        || fee_atoms > maximum_source_fee_atoms
        || (fee_atoms > 0 && (native_fee_atoms > 0 || row["feeToken"].as_str() != Some("USDC")))
        || (native_fee_atoms > 0 && (fee_atoms > 0 || row["feeToken"].as_str() != Some("")))
        || (fee_atoms == 0
            && native_fee_atoms == 0
            && row["feeToken"]
                .as_str()
                .is_none_or(|value| !value.is_empty() && value != "USDC"))
        || (action_type == "sendAsset"
            && (row["sourceDex"].as_str() != Some("spot")
                || row["destinationDex"].as_str() != Some("spot")))
    {
        return None;
    }
    Some(CoreSendLedgerEffects {
        source_debit_atoms: amount_atoms.checked_add(fee_atoms)?,
        source_fee_atoms: fee_atoms,
        source_native_fee_atoms: native_fee_atoms,
        destination_credit_atoms: amount_atoms,
    })
}

/// EVM creates a transaction-level child identity for a nested envelope.
/// Keep that identity in the protected funding journal while preserving the
/// exact outer Papertrade submission in the engine journal.
struct NestedFundingJournal {
    credentials: Arc<dyn crate::BrokerCredentialProvider>,
    outer: gofin_domain::Submission,
    child_key: String,
    outer_journal: Arc<dyn gofin_domain::SubmissionJournal>,
}

/// Construct the protected nested identity journal for any single frozen EVM
/// leg in an approved Papertrade funding operation. The outer journal remains
/// authoritative; this wrapper only stores the exact child transaction and
/// redacted dispatch evidence under the funding leg identity.
pub(crate) fn nested_funding_journal(
    credentials: Arc<dyn crate::BrokerCredentialProvider>,
    outer: gofin_domain::Submission,
    leg_submission_id: &str,
    outer_journal: Arc<dyn gofin_domain::SubmissionJournal>,
) -> Arc<dyn gofin_domain::SubmissionJournal> {
    Arc::new(NestedFundingJournal {
        credentials,
        outer,
        child_key: format!(
            "papertrade-funding-leg:{}:child_submission",
            hex::encode(leg_submission_id.as_bytes())
        ),
        outer_journal,
    })
}

impl gofin_domain::SubmissionJournal for NestedFundingJournal {
    fn assert_authorized(&self) -> gofin_domain::Result<()> {
        self.outer_journal.assert_authorized()
    }

    fn assert_candidate(
        &self,
        prepared: &gofin_domain::PreparedOperation,
    ) -> gofin_domain::Result<()> {
        self.outer_journal.assert_candidate(prepared)
    }

    fn persist_submission(
        &self,
        submission: &gofin_domain::Submission,
    ) -> gofin_domain::Result<()> {
        let value = serde_json::to_value(submission).map_err(|_| {
            GoFinError::validation("Nested EVM funding identity could not be serialized.")
        })?;
        if let Some(existing) = self
            .credentials
            .papertrade_runtime_record(&self.child_key)?
        {
            if existing != value {
                return Err(GoFinError::new(
                    "PAPERTRADE_FUNDING_IDENTITY_CONFLICT",
                    "journal",
                    "A nested EVM funding identity already contains different data.",
                    "Reconcile the original Papertrade funding submission.",
                ));
            }
            return Ok(());
        }
        self.credentials
            .put_papertrade_runtime_record(&self.child_key, &value)
    }

    fn persist_dispatch_evidence(
        &self,
        submission: &gofin_domain::Submission,
        evidence: &Value,
    ) -> gofin_domain::Result<()> {
        let key = format!("{}:evidence", self.child_key);
        let record = json!({"version":1,"submission":submission,"evidence":evidence});
        if let Some(existing) = self.credentials.papertrade_runtime_record(&key)? {
            if existing != record {
                return Err(GoFinError::validation(
                    "Nested EVM funding evidence conflicts with its durable record.",
                ));
            }
        } else {
            self.credentials
                .put_papertrade_runtime_record(&key, &record)?;
        }
        self.outer_journal.persist_dispatch_evidence(
            &self.outer,
            &json!({"kind":"nested_evm_funding","submission":submission,"evidence":evidence}),
        )
    }
}

#[async_trait::async_trait]
impl PapertradeFundingServices for NativeHyperEvmFundingServices {
    async fn preflight(&self, intent: &FundingIntent) -> Result<FundingPreflight, GoFinError> {
        let hyperevm_deposit =
            intent.route == FundingRoute::Hyperevm && intent.direction == FundingDirection::Deposit;
        let core_withdrawal =
            intent.route == FundingRoute::Core && intent.direction == FundingDirection::Withdraw;
        if (!hyperevm_deposit && !core_withdrawal)
            || self.runtime.chain_id != 999
            || !self.owner.eq_ignore_ascii_case(&intent.owner)
        {
            return Self::unsupported();
        }
        let factory = self
            .runtime
            .rpc
            .request(
                "eth_call",
                json!([{"to":crate::papertrade_funding::PAPERTRADE_PROXY_FACTORY,
                "data":crate::papertrade_funding::compute_proxy_address_call(&self.owner)?.data},
                "latest"]),
            )
            .await?;
        let factory_proxy =
            crate::papertrade_funding::decode_proxy_address_result(factory.as_str().ok_or_else(
                || GoFinError::validation("Papertrade factory returned malformed proxy data."),
            )?)?;
        let (proxy_core_usdc_before_atoms, allowance) = if hyperevm_deposit {
            (
                Some(self.require_zero_proxy_spot_balance(&factory_proxy).await?),
                Some(self.allowance_atoms(&self.owner).await?),
            )
        } else {
            (None, None)
        };
        let (native_fee_asset, maximum_native_fee_atoms) = if hyperevm_deposit {
            (
                Some("ethereum:999:native".into()),
                Some(self.maximum_hype_fee_atoms.clone()),
            )
        } else {
            // Core withdrawals are owner-signed Papertrade relayer intents.
            // They do not submit an EVM transaction from the owner's HYPE
            // wallet, so do not reserve a fictitious native gas fee.
            (None, None)
        };
        Ok(FundingPreflight {
            owner: self.owner.clone(),
            factory_proxy,
            account_mode: self.account_mode,
            solana_owner: None,
            hyperevm_usdc_allowance_atoms: allowance,
            native_fee_asset,
            maximum_native_fee_atoms,
            proxy_core_usdc_before_atoms,
            core_owner_usdc_before_atoms: None,
            core_sweep_from_block: None,
        })
    }

    async fn check_proxy_registration(
        &self,
        operation_id: &str,
        owner: &str,
        factory_proxy: &str,
    ) -> Result<ProxyRegistrationProof, GoFinError> {
        if !owner.eq_ignore_ascii_case(&self.owner) {
            return Err(GoFinError::validation(
                "Funding registration owner differs from the protected wallet.",
            ));
        }
        self.verify_proxy(owner, factory_proxy).await?;
        let api_proxy = self
            .papertrade_http
            .check_deposit_proxy(owner)
            .await
            .map_err(|_| {
                GoFinError::new(
                    "PAPERTRADE_REGISTRATION_UNAVAILABLE",
                    "preview",
                    "Papertrade proxy registration check failed.",
                    "Resolve the official funding-check response before submitting a source transfer.",
                )
            })?;
        if !api_proxy.eq_ignore_ascii_case(factory_proxy) {
            return Err(GoFinError::validation(
                "Papertrade API proxy differs from the chain-derived proxy.",
            ));
        }
        Ok(ProxyRegistrationProof {
            owner: self.owner.clone(),
            api_proxy,
            factory_proxy: factory_proxy.to_ascii_lowercase(),
            request_id: format!("papertrade-proxy-check:{operation_id}"),
        })
    }

    async fn cctp_quote(&self, _: &CctpQuoteRequest) -> Result<FundingCctpQuote, GoFinError> {
        Self::unsupported()
    }

    async fn prepare_evm_call(
        &self,
        submission_id: &str,
        intent: &FundingIntent,
        call: &EvmCall,
        maximum_total_fee_atoms: &str,
    ) -> Result<gofin_evm::FrozenTransaction, GoFinError> {
        let deposit = intent.route == FundingRoute::Hyperevm
            && intent.direction == FundingDirection::Deposit
            && call
                .to
                .eq_ignore_ascii_case(cctp::hyperevm::CORE_DEPOSIT_WALLET);
        let withdrawal = intent.route == FundingRoute::Core
            && intent.direction == FundingDirection::Withdraw
            && call.to.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE);
        if (!deposit && !withdrawal)
            || call.chain_id != 999
            || !gofin_evm::alloy::primitives::U256::from_str_radix(maximum_total_fee_atoms, 10)
                .is_ok_and(|cap| {
                    cap <= U256::from_str_radix(&self.maximum_hype_fee_atoms, 10)
                        .unwrap_or_default()
                })
        {
            return Err(GoFinError::validation(
                "EVM funding transaction exceeds the configured native HyperEVM route.",
            ));
        }
        let sender = gofin_evm::address(&self.owner)?;
        let target = gofin_evm::address(&call.to)?;
        let value = U256::from_str_radix(&call.value.to_string(), 10)
            .map_err(|_| GoFinError::validation("EVM call value is malformed."))?;
        let data = hex::decode(
            call.data
                .strip_prefix("0x")
                .ok_or_else(|| GoFinError::validation("EVM funding calldata is malformed."))?,
        )
        .map_err(|_| GoFinError::validation("EVM funding calldata is malformed."))?;
        let cap = U256::from_str_radix(maximum_total_fee_atoms, 10)
            .map_err(|_| GoFinError::validation("EVM gas fee cap is malformed."))?;
        let prepared = self
            .runtime
            .prepare(sender, target, value, Bytes::from(data), cap)
            .await?;
        let key = Self::submission_key(submission_id, "prepared");
        self.persist_immutable(
            &key,
            &json!({"version":1,"transaction":prepared.transaction}),
        )?;
        Ok(prepared.transaction)
    }

    async fn solana_burn_request(
        &self,
        _: &str,
        _: &FundingIntent,
        _: &cctp::CctpQuoteMetadata,
        _: &str,
    ) -> Result<cctp::solana::hypercore_source::BurnRequest, GoFinError> {
        Self::unsupported()
    }

    async fn prepare_solana_burn_transaction(
        &self,
        _: &str,
        _: &gofin_solana::NormalizedInstruction,
        _: u64,
    ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError> {
        Self::unsupported()
    }

    async fn reserve_hyperliquid_nonce(&self, _: &str) -> Result<u64, GoFinError> {
        Self::unsupported()
    }

    async fn papertrade_withdraw_to_core_intent(
        &self,
        _: &str,
        _: &FundingIntent,
    ) -> Result<gofin_papertrade::BoundIntent, GoFinError> {
        Self::unsupported()
    }

    async fn persist_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        let value = serde_json::to_value(plan).map_err(|_| {
            GoFinError::validation("Papertrade funding plan could not be serialized.")
        })?;
        self.persist_immutable(&Self::record_key(&plan.operation_id), &value)
    }

    async fn assert_persisted_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        let key = Self::record_key(&plan.operation_id);
        let existing = self
            .credentials
            .papertrade_runtime_record(&key)?
            .ok_or_else(|| {
                GoFinError::validation("Papertrade funding plan is not durably persisted.")
            })?;
        let expected = serde_json::to_value(plan).map_err(|_| {
            GoFinError::validation("Papertrade funding plan could not be serialized.")
        })?;
        if existing != expected {
            return Err(GoFinError::validation(
                "Papertrade funding plan differs from its durable binding.",
            ));
        }
        Ok(())
    }

    async fn dispatch_persisted_leg(
        &self,
        leg: &FundingLeg,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<FundingLegState, GoFinError> {
        let FundingAction::EvmCall { call, transaction } = &leg.action else {
            return Self::unsupported();
        };
        if call.chain_id != 999
            || !call
                .to
                .eq_ignore_ascii_case(cctp::hyperevm::CORE_DEPOSIT_WALLET)
            || !transaction.sender.eq_ignore_ascii_case(&self.owner)
        {
            return Self::unsupported();
        }
        let bound_tx = validated
            .prepared()
            .binding
            .pointer("/funding_binding/transaction")
            .ok_or_else(|| {
                GoFinError::validation(
                    "Authorized Papertrade operation omits its exact frozen EVM transaction.",
                )
            })?;
        let authorized: gofin_evm::FrozenTransaction = serde_json::from_value(bound_tx.clone())
            .map_err(|_| GoFinError::validation("Authorized EVM funding envelope is malformed."))?;
        if &authorized != transaction {
            return Err(GoFinError::validation(
                "Frozen EVM funding transaction differs from the authorized binding.",
            ));
        }
        let prepared_key = Self::submission_key(&leg.submission_id, "prepared");
        let prepared_record = self
            .credentials
            .papertrade_runtime_record(&prepared_key)?
            .ok_or_else(|| {
                GoFinError::validation("Frozen EVM funding transaction is not durably persisted.")
            })?;
        if prepared_record.get("transaction")
            != Some(&serde_json::to_value(transaction).unwrap_or(Value::Null))
        {
            return Err(GoFinError::validation(
                "Frozen EVM funding transaction differs from its durable record.",
            ));
        }
        let marker = Self::submission_key(&leg.submission_id, "dispatch_attempted");
        self.persist_immutable(
            &marker,
            &json!({"version":1,"operation_id":leg.submission_id}),
        )?;
        let child_journal = nested_funding_journal(
            self.credentials.clone(),
            outer_submission.clone(),
            &leg.submission_id,
            journal,
        );
        match self
            .runtime
            .submit_nested_revalidated(
                transaction,
                json!({"operation":"papertrade.deposit","route":"hyperevm"}),
                child_journal,
                validated,
                "/funding_binding",
            )
            .await
        {
            Ok(submission) => {
                self.persist_immutable(
                    &Self::submission_key(&leg.submission_id, "submission"),
                    &serde_json::to_value(&submission).map_err(|_| {
                        GoFinError::validation(
                            "EVM funding submission identity could not be serialized.",
                        )
                    })?,
                )?;
                Ok(FundingLegState::Submitted {
                    evidence: json!({"kind":submission.kind,"reference":submission.reference}),
                })
            }
            Err(_) => Ok(FundingLegState::Unknown {
                evidence: json!({"reason":"dispatch outcome requires original-submission reconciliation"}),
            }),
        }
    }

    async fn reconcile_persisted_leg(
        &self,
        _plan: &BoundFunding,
        leg: &FundingLeg,
    ) -> Result<FundingLegState, GoFinError> {
        let key = Self::submission_key(&leg.submission_id, "submission");
        let child_key = Self::submission_key(&leg.submission_id, "child_submission");
        let Some(value) = self
            .credentials
            .papertrade_runtime_record(&key)?
            .or(self.credentials.papertrade_runtime_record(&child_key)?)
        else {
            let attempted = self
                .credentials
                .papertrade_runtime_record(&Self::submission_key(
                    &leg.submission_id,
                    "dispatch_attempted",
                ))?;
            return Ok(if attempted.is_some() {
                FundingLegState::Unknown {
                    evidence: json!({"reason":"dispatch identity is durable but submission response is unresolved"}),
                }
            } else {
                FundingLegState::NotSubmitted
            });
        };
        let submission: gofin_domain::Submission = serde_json::from_value(value)
            .map_err(|_| GoFinError::validation("Persisted EVM submission is malformed."))?;
        let FundingAction::EvmCall { transaction, .. } = &leg.action else {
            return Self::unsupported();
        };
        let Some(receipt) = self
            .runtime
            .observe_receipt(
                &submission.reference,
                self.confirmations,
                self.require_finalized,
            )
            .await?
        else {
            return Ok(FundingLegState::Submitted {
                evidence: json!({"kind":submission.kind,"reference":submission.reference}),
            });
        };
        if !receipt.success {
            return Ok(FundingLegState::Rejected {
                evidence: json!({"reference":submission.reference,"block":receipt.block_number}),
            });
        }
        let reported = self
            .runtime
            .rpc
            .request("eth_getTransactionByHash", json!([submission.reference]))
            .await?;
        if !transaction_matches(&reported, transaction)? {
            return Err(GoFinError::validation(
                "Mined HyperEVM transaction differs from the exact persisted funding envelope.",
            ));
        }
        let plan = load_plan_for_submission(self.credentials.as_ref(), &leg.submission_id)?;
        let source_atoms = plan.bounds.source_usdc_atoms;
        let fee_cap_text = plan
            .bounds
            .maximum_native_fee_atoms
            .as_deref()
            .ok_or_else(|| {
                GoFinError::validation("Persisted funding plan has no finite native fee bound.")
            })?;
        let fee_cap = U256::from_str_radix(fee_cap_text, 10)
            .map_err(|_| GoFinError::validation("Persisted native fee bound is malformed."))?;
        if receipt.fee > fee_cap
            || !has_exact_usdc_transfer(
                &receipt.receipt,
                &self.owner,
                cctp::hyperevm::CORE_DEPOSIT_WALLET,
                source_atoms,
            )?
        {
            return Err(GoFinError::validation(
                "HyperEVM receipt lacks the exact USDC source debit or exceeds its approved gas cap.",
            ));
        }
        Ok(FundingLegState::Settled {
            evidence: json!({
                "transaction_hash":format!("{:#x}",receipt.hash),
                "block_hash":format!("{:#x}",receipt.block_hash),
                "block_number":receipt.block_number,
                "transaction_index":receipt_transaction_index(&receipt.receipt),
                "confirmations":receipt.confirmations,
                "success":receipt.success,
                "fee_atoms":receipt.fee.to_string(),
                "receipt":receipt.receipt,
            }),
        })
    }

    async fn prove_destination(
        &self,
        plan: &BoundFunding,
        settled_legs: &[FundingLegState],
    ) -> Result<Option<FundingReceipt>, GoFinError> {
        if !matches!(
            (&plan.intent.route, &plan.intent.direction),
            (FundingRoute::Hyperevm, FundingDirection::Deposit)
                | (FundingRoute::Core, FundingDirection::Deposit)
        ) {
            return Self::unsupported();
        }
        let source_evidence = settled_legs
            .iter()
            .find_map(|state| match state {
                FundingLegState::Settled { evidence } => Some(evidence.clone()),
                _ => None,
            })
            .ok_or_else(|| GoFinError::validation("HyperEVM source debit is not proven."))?;
        let (source_block, source_index) = if plan.intent.route == FundingRoute::Core {
            if source_evidence.get("kind").and_then(Value::as_str)
                != Some("hyperliquid_core_usdc_debit")
                || plan.proxy_core_usdc_before_atoms != Some(0)
            {
                return Err(GoFinError::validation(
                    "Core source debit or empty proxy baseline is not proven.",
                ));
            }
            (
                plan.core_sweep_from_block.ok_or_else(|| {
                    GoFinError::validation("Core sweep scan baseline is missing.")
                })?,
                0,
            )
        } else {
            (
                source_evidence
                    .get("block_number")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        GoFinError::validation("HyperEVM source debit block is missing.")
                    })?,
                source_evidence
                    .get("transaction_index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| {
                        GoFinError::validation("HyperEVM source transaction index is missing.")
                    })?,
            )
        };
        let from_block = source_block;
        let owner_topic = format!("0x{}{}", "00".repeat(12), &plan.intent.owner[2..]);
        let latest = self
            .runtime
            .rpc
            .request("eth_blockNumber", json!([]))
            .await?;
        let latest = latest
            .as_str()
            .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok())
            .ok_or_else(|| GoFinError::validation("HyperEVM returned an invalid latest block."))?;
        if from_block > latest {
            return Ok(None);
        }
        let source_leg = plan
            .legs
            .first()
            .ok_or_else(|| GoFinError::validation("Papertrade deposit plan has no source leg."))?;
        let mut rows = Vec::<Value>::new();
        let mut page_start = from_block;
        let mut page_budget = 0usize;
        while page_start <= latest {
            let page_end = page_start.saturating_add(199);
            let query_end = latest.min(page_end);
            let page_key = Self::submission_key(
                &source_leg.submission_id,
                &format!("sweep_page_{page_start}"),
            );
            if let Some(saved) = self.credentials.papertrade_runtime_record(&page_key)? {
                if saved.get("from").and_then(Value::as_u64) != Some(page_start)
                    || saved.get("to").and_then(Value::as_u64) != Some(page_end)
                {
                    return Err(GoFinError::validation(
                        "Persisted sweep scan page is inconsistent.",
                    ));
                }
                if let Some(found) = saved.get("matches").and_then(Value::as_array) {
                    rows.extend(found.iter().cloned());
                }
            } else {
                let logs = self.runtime.rpc.request("eth_getLogs", json!([{
                    "address": gofin_papertrade::EXCHANGE,
                    "fromBlock": format!("0x{page_start:x}"),
                    "toBlock": format!("0x{query_end:x}"),
                    "topics": ["0xc36b5179cb9c303b200074996eab2b3473eac370fdd7eba3bec636fe35109696", owner_topic],
                }])).await?;
                let page = logs.as_array().ok_or_else(|| {
                    GoFinError::validation("HyperEVM returned malformed Papertrade Swept logs.")
                })?;
                let matches = page
                    .iter()
                    .filter(|row| {
                        row.get("data")
                            .and_then(Value::as_str)
                            .and_then(|data| crate::papertrade_funding::event_u128(data, 0))
                            .is_some_and(|wad| {
                                wad % 1_000_000_000_000 == 0
                                    && wad / 1_000_000_000_000
                                        >= u128::from(plan.bounds.destination_min_usdc_atoms)
                                    && wad / 1_000_000_000_000
                                        <= u128::from(plan.bounds.source_usdc_atoms)
                            })
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if query_end == page_end {
                    let saved =
                        json!({"version":1,"from":page_start,"to":page_end,"matches":matches});
                    self.persist_immutable(&page_key, &saved)?;
                    rows.extend(saved["matches"].as_array().into_iter().flatten().cloned());
                } else {
                    rows.extend(matches);
                }
            }
            if rows.len() > 1 {
                break;
            }
            page_start = query_end.saturating_add(1);
            page_budget += 1;
            if page_budget >= 100 && page_start <= latest {
                return Ok(None);
            }
        }
        rows.retain(|row| {
            event_position(row).is_some_and(|(block, tx_index)| {
                if plan.intent.route == FundingRoute::Core {
                    block >= source_block
                } else {
                    block > source_block || (block == source_block && tx_index > source_index)
                }
            })
        });
        if rows.is_empty() {
            return Ok(None);
        }
        if rows.len() != 1 {
            return Err(GoFinError::new(
                "PAPERTRADE_SWEEP_AMBIGUOUS",
                "reconciliation",
                "Multiple Swept events match this Papertrade deposit.",
                "Keep the original funding operation unresolved and inspect canonical chain evidence.",
            ));
        }
        let tx_hash = rows[0]
            .get("transactionHash")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                GoFinError::validation("Papertrade Swept log omitted its transaction hash.")
            })?;
        let swept_transaction = self
            .runtime
            .rpc
            .request("eth_getTransactionByHash", json!([tx_hash]))
            .await?;
        if !sweep_finalize_matches(&swept_transaction, &plan.intent.owner) {
            return Err(GoFinError::validation(
                "Papertrade Swept event is not from the exact owner finalizeSweep call.",
            ));
        }
        if plan.intent.route == FundingRoute::Core {
            let proxy_sweep_time_ms = source_evidence
                .get("proxy_sweep_transfer")
                .and_then(|row| row.get("time"))
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    GoFinError::validation("Core proxy sweep transfer omitted its event time.")
                })?;
            let block_number = rows[0]
                .get("blockNumber")
                .and_then(Value::as_str)
                .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok())
                .ok_or_else(|| GoFinError::validation("Papertrade sweep block is malformed."))?;
            let block = self
                .runtime
                .rpc
                .request(
                    "eth_getBlockByNumber",
                    json!([format!("0x{block_number:x}"), false]),
                )
                .await?;
            let sweep_time_ms = block
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(|value| u64::from_str_radix(value.strip_prefix("0x")?, 16).ok())
                .and_then(|value| value.checked_mul(1_000))
                .ok_or_else(|| {
                    GoFinError::validation("Papertrade sweep block timestamp is malformed.")
                })?;
            if sweep_time_ms / 1_000 < proxy_sweep_time_ms / 1_000 {
                return Ok(None);
            }
        }
        let Some(receipt) = self
            .runtime
            .observe_receipt(tx_hash, self.confirmations, self.require_finalized)
            .await?
        else {
            return Ok(None);
        };
        let data = rows[0].get("data").and_then(Value::as_str).ok_or_else(|| {
            GoFinError::validation("Papertrade Swept event omitted its credited amount.")
        })?;
        let wad = crate::papertrade_funding::event_u128(data, 0)
            .ok_or_else(|| GoFinError::validation("Papertrade Swept amount is malformed."))?;
        if wad % 1_000_000_000_000 != 0 {
            return Err(GoFinError::validation(
                "Papertrade Swept amount has non-USDC precision.",
            ));
        }
        let atoms = u64::try_from(wad / 1_000_000_000_000).map_err(|_| {
            GoFinError::validation("Papertrade Swept amount exceeds the supported range.")
        })?;
        let proof = crate::papertrade_funding::verify_swept_event(
            &receipt,
            gofin_papertrade::EXCHANGE,
            &plan.intent.owner,
            atoms,
        )?;
        if atoms < plan.bounds.destination_min_usdc_atoms || atoms > plan.bounds.source_usdc_atoms {
            return Err(GoFinError::validation(
                "Papertrade Swept amount falls outside the approved net-credit bounds.",
            ));
        }
        if plan.intent.route == FundingRoute::Core {
            let expected_credit = source_evidence
                .get("destination_credit_atoms")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    GoFinError::validation(
                        "Core source ledger did not bind an exact destination credit amount.",
                    )
                })?;
            if atoms != expected_credit {
                return Err(GoFinError::validation(
                    "Papertrade Swept credit differs from the exact Core recipient ledger amount.",
                ));
            }
        }
        let activation_fee_usdc_atoms = if plan.intent.route == FundingRoute::Hyperevm {
            observed_activation_fee_atoms(
                plan.bounds.source_usdc_atoms,
                atoms,
                plan.bounds.maximum_activation_fee_atoms,
            )?
        } else {
            source_evidence
                .get("activation_fee_atoms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let source_debit_usdc_atoms = if plan.intent.route == FundingRoute::Core {
            source_evidence
                .get("source_debit_atoms")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    GoFinError::validation("Core source debit proof omitted the exact total debit.")
                })?
        } else {
            plan.bounds.source_usdc_atoms
        };
        Ok(Some(FundingReceipt {
            stage: gofin_papertrade::FundingReceiptStage::FinalDestination,
            status: "settled".into(),
            amount_usdc_atoms: atoms,
            source_debit_usdc_atoms,
            destination_credit_usdc_atoms: atoms,
            retained_source_credit_usdc_atoms: source_evidence
                .get("retained_source_credit_atoms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            actual_source_fee_usdc_atoms: source_evidence
                .get("source_fee_atoms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            actual_cctp_fee_usdc_atoms: source_evidence
                .get("cctp_fee_atoms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            activation_fee_usdc_atoms,
            actual_native_fee_atoms: source_evidence
                .get("fee_atoms")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            actual_native_fee_asset: source_evidence
                .get("fee_atoms")
                .and_then(Value::as_str)
                .and_then(|_| plan.bounds.native_fee_asset.clone()),
            retained_source_native_atoms: None,
            retained_source_native_asset: None,
            source_identity: plan.source_identity.clone(),
            destination_identity: plan.destination_identity.clone(),
            evidence_id: format!("{}:{}", proof.transaction_hash, proof.block_hash),
        }))
    }
}

/// Concrete implementation of the planner host boundary. It deliberately
/// contains no signing keys itself; those remain inside the broker credential
/// provider and typed runtime transports owned by the injected services.
pub(crate) struct PapertradeFundingHostAdapter {
    services: Arc<dyn PapertradeFundingServices>,
}

impl PapertradeFundingHostAdapter {
    pub(crate) fn new(services: Arc<dyn PapertradeFundingServices>) -> Self {
        Self { services }
    }

    pub(crate) fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        self.services.attach_execution_state(state)
    }

    pub(crate) fn attach_runtime(
        &self,
        runtime: std::sync::Weak<crate::papertrade_runtime::BrokerPapertradeRuntime>,
    ) -> Result<(), GoFinError> {
        self.services.attach_runtime(runtime)
    }

    #[allow(clippy::too_many_arguments)] // The Noop child is bound to parent, protected owner, exact nonce and journal.
    pub(crate) async fn invalidate_core_deposit_nonce(
        &self,
        parent_operation_id: &str,
        parent_record_id: &str,
        expected_owner: &str,
        expected_nonce: u64,
        child_operation_id: &str,
        child_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value, GoFinError> {
        self.services
            .invalidate_core_deposit_nonce(
                parent_operation_id,
                parent_record_id,
                expected_owner,
                expected_nonce,
                child_operation_id,
                child_submission,
                validated,
                journal,
            )
            .await
    }
}

#[async_trait::async_trait]
impl PapertradeFundingHost for PapertradeFundingHostAdapter {
    async fn preflight(&self, intent: &FundingIntent) -> Result<FundingPreflight, GoFinError> {
        self.services.preflight(intent).await
    }

    async fn ensure_proxy_registered(
        &self,
        operation_id: &str,
        owner: &str,
        factory_proxy: &str,
    ) -> Result<ProxyRegistrationProof, GoFinError> {
        self.services
            .check_proxy_registration(operation_id, owner, factory_proxy)
            .await
    }

    async fn cctp_quote(&self, request: &CctpQuoteRequest) -> Result<FundingCctpQuote, GoFinError> {
        self.services.cctp_quote(request).await
    }

    async fn prepare_evm_call(
        &self,
        submission_id: &str,
        intent: &gofin_papertrade::FundingIntent,
        call: &EvmCall,
        maximum_total_fee_atoms: &str,
    ) -> Result<gofin_evm::FrozenTransaction, GoFinError> {
        self.services
            .prepare_evm_call(submission_id, intent, call, maximum_total_fee_atoms)
            .await
    }

    async fn solana_burn_request(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
        metadata: &cctp::CctpQuoteMetadata,
        proxy: &str,
    ) -> Result<cctp::solana::hypercore_source::BurnRequest, GoFinError> {
        self.services
            .solana_burn_request(operation_id, intent, metadata, proxy)
            .await
    }

    async fn prepare_solana_burn_transaction(
        &self,
        submission_id: &str,
        instruction: &gofin_solana::NormalizedInstruction,
        maximum_network_fee_lamports: u64,
    ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError> {
        self.services
            .prepare_solana_burn_transaction(
                submission_id,
                instruction,
                maximum_network_fee_lamports,
            )
            .await
    }

    async fn reserve_hyperliquid_nonce(&self, submission_id: &str) -> Result<u64, GoFinError> {
        self.services.reserve_hyperliquid_nonce(submission_id).await
    }

    async fn papertrade_withdraw_to_core_intent(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
    ) -> Result<gofin_papertrade::BoundIntent, GoFinError> {
        self.services
            .papertrade_withdraw_to_core_intent(operation_id, intent)
            .await
    }

    async fn persist_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        self.services.persist_plan(plan).await
    }

    async fn assert_persisted_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError> {
        self.services.assert_persisted_plan(plan).await
    }

    async fn dispatch_leg(
        &self,
        leg: &FundingLeg,
        outer_submission: &gofin_domain::Submission,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<FundingLegState, GoFinError> {
        validate_closed_action(&leg.action)?;
        self.services
            .dispatch_persisted_leg(leg, outer_submission, journal, validated)
            .await
    }

    async fn reconcile_leg(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
    ) -> Result<FundingLegState, GoFinError> {
        validate_closed_action(&leg.action)?;
        self.services.reconcile_persisted_leg(plan, leg).await
    }

    async fn prove_destination(
        &self,
        plan: &BoundFunding,
        settled_legs: &[FundingLegState],
    ) -> Result<Option<FundingReceipt>, GoFinError> {
        self.services.prove_destination(plan, settled_legs).await
    }
}

/// Defense in depth for deserialized journals: route actions must stay inside
/// the supported mainnet topology even if a corrupted or older record is
/// presented to the service implementation.
fn validate_closed_action(action: &FundingAction) -> Result<(), GoFinError> {
    let valid = match action {
        FundingAction::CoreDeposit { action } => match action {
            gofin_hyperliquid::funding::CoreToPapertradeAction::SpotSend(action) => {
                action.action_type == "spotSend"
                    && action.hyperliquid_chain == "Mainnet"
                    && action.token == "USDC"
                    && action.destination.starts_with("0x")
            }
            gofin_hyperliquid::funding::CoreToPapertradeAction::SendAsset(action) => {
                action.action_type == "sendAsset"
                    && action.hyperliquid_chain == "Mainnet"
                    && action.signature_chain_id == "0x3e7"
                    && action.token == "USDC"
                    && action.destination.starts_with("0x")
            }
        },
        FundingAction::EvmCall { call, transaction } => {
            matches!(call.chain_id, 1 | 42161 | 999)
                && transaction.chain_id == call.chain_id
                && transaction.to.eq_ignore_ascii_case(&call.to)
                && transaction.calldata == call.data
                && transaction.value == call.value
        }
        FundingAction::SolanaCctpBurn {
            request,
            instruction,
            transaction,
        } => {
            gofin_solana::parse_address(&request.owner).is_ok()
                && !request.message_sent_event_data.is_empty()
                && request.amount_atoms > 0
                && request.max_fee_atoms < request.amount_atoms
                && !request.hook_data.is_empty()
                && transaction.message_sha256.len() == 64
                && transaction.maximum_network_fee_lamports > 0
                && instruction.program_id == gofin_bridge::cctp::solana::TOKEN_MESSENGER_MINTER
        }
        FundingAction::HyperCoreCctpWithdraw { action } => {
            action.signature_chain_id.eq_ignore_ascii_case("0x3e7")
                && action.hyperliquid_chain == "Mainnet"
                && action.token == "USDC"
                && action.gas_limit > 0
        }
        FundingAction::HyperCoreSendAsset { action } => {
            action.signature_chain_id.eq_ignore_ascii_case("0x3e7")
                && action.hyperliquid_chain == "Mainnet"
                && action.token == "USDC"
                && action.destination == gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS
        }
        FundingAction::PapertradeWithdrawToCore { bound } => {
            let valid_id = bound.intent_id.strip_prefix("0x").is_some_and(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
            match &bound.typed {
                gofin_papertrade::PapertradeTypedIntent::WithdrawToCore {
                    user,
                    amount,
                    nonce,
                    deadline,
                } => {
                    valid_id
                        && !bound
                            .intent_id
                            .eq_ignore_ascii_case(&format!("0x{}", "0".repeat(64)))
                        && user.eq_ignore_ascii_case(&bound.signer)
                        && amount.parse::<u64>().is_ok_and(|value| value > 0)
                        && nonce.parse::<u64>().is_ok_and(|value| value > 0)
                        && *deadline > 0
                        && bound.action.as_deref() == Some("withdrawToCore")
                        && bound.registration_id.is_none()
                        && bound.cancel_target_action.is_none()
                }
                _ => false,
            }
        }
    };
    if valid {
        Ok(())
    } else {
        Err(GoFinError::validation(
            "Persisted Papertrade funding leg is outside the reviewed mainnet route.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BrokerCredentialProvider;
    use async_trait::async_trait;
    use gofin_domain::financial_plans::{
        FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
        FinancialPlanStepSpec, FinancialStepCompletion,
    };
    use gofin_domain::{
        Amount, ApprovalAudit, AssetId, AuthorizationChallenge, Authorizer, CostBound,
        CostTreatment, Quantity, ResourceAmount, ResourceId,
    };
    use gofin_protocol::BrokerRequest;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    struct FundingTestCredentials(Mutex<BTreeMap<String, Value>>);

    impl gofin_exchange::ccxt::CredentialProvider for FundingTestCredentials {
        fn credentials(
            &self,
            _: &str,
        ) -> Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError> {
            Err(gofin_exchange::ccxt::CcxtError {
                kind: gofin_exchange::ccxt::CcxtErrorKind::Unsupported,
                http_status: None,
                exchange_code: None,
                funding_error_code: None,
            })
        }
    }

    impl crate::BrokerCredentialProvider for FundingTestCredentials {
        fn envelope_signer(
            &self,
            _: &gofin_credentials::CredentialRef,
        ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
            Err(GoFinError::validation("test provider has no signer"))
        }

        fn papertrade_wallet_signer(
            &self,
            _: &gofin_credentials::CredentialRef,
            owner: &str,
        ) -> gofin_domain::Result<Arc<gofin_evm::LocalEnvelopeSigner>> {
            let signer = Arc::new(gofin_evm::LocalEnvelopeSigner::from_private_key(&format!(
                "0x{:064x}",
                1
            ))?);
            if !format!("{:#x}", signer.address()).eq_ignore_ascii_case(owner) {
                return Err(GoFinError::validation("fixture owner does not match key"));
            }
            Ok(signer)
        }

        fn papertrade_runtime_record(&self, key: &str) -> gofin_domain::Result<Option<Value>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        fn put_papertrade_runtime_record(
            &self,
            key: &str,
            value: &Value,
        ) -> gofin_domain::Result<()> {
            let mut records = self.0.lock().unwrap();
            if records.get(key).is_some_and(|existing| existing != value) {
                return Err(GoFinError::validation("test record identity conflict"));
            }
            records.insert(key.to_owned(), value.clone());
            Ok(())
        }
    }

    struct CoreEvidenceTransport {
        proxy: String,
        hash: String,
        nonce: std::sync::atomic::AtomicU64,
        sweep_timestamp: Arc<std::sync::atomic::AtomicU64>,
        amount: String,
        source_fee: String,
        dispatch_succeeds: Arc<std::sync::atomic::AtomicBool>,
        core_dispatch_succeeds: Arc<std::sync::atomic::AtomicBool>,
        history_visible: Arc<std::sync::atomic::AtomicBool>,
        canonical_error: bool,
        empty_ledger: bool,
        history_calls: std::sync::atomic::AtomicUsize,
        dispatch_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl gofin_hyperliquid::HyperliquidTransport for CoreEvidenceTransport {
        async fn info(
            &self,
            request: gofin_exchange::ccxt::HyperliquidInfoRequest,
        ) -> Result<Value, GoFinError> {
            let owner = match request {
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserNonFundingLedgerUpdates {
                    owner,
                    ..
                } => owner,
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserRole { .. } => {
                    return Ok(json!({"role":"user"}));
                }
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserAbstractionState { .. } => {
                    return Ok(json!("unifiedAccount"));
                }
                gofin_exchange::ccxt::HyperliquidInfoRequest::UserDexAbstractionState {
                    ..
                } => {
                    return Ok(Value::Null);
                }
                gofin_exchange::ccxt::HyperliquidInfoRequest::SpotClearinghouseState { owner } => {
                    if owner.eq_ignore_ascii_case(OWNER) {
                        return Ok(json!({
                            "balances":[{"coin":"USDC","token":0,"total":"100.0","hold":"0"}],
                            "tokenToAvailableAfterMaintenance":[[0,"100.0"]]
                        }));
                    }
                    return Ok(json!({"balances":[]}));
                }
                _ => return Err(GoFinError::validation("unexpected test info request")),
            };
            if self.empty_ledger {
                return Ok(json!([]));
            }
            let ledger_amount = if self.amount.contains('.') {
                self.amount.clone()
            } else {
                format!("{}.0", self.amount)
            };
            let source = json!({
                "type":"send", "user":OWNER, "destination":self.proxy,
                "sourceDex":"spot", "destinationDex":"spot", "token":"USDC",
                "amount":ledger_amount, "usdcValue":ledger_amount, "fee":self.source_fee, "feeToken":"USDC",
                "nativeTokenFee":"0.0", "nonce":self.nonce.load(std::sync::atomic::Ordering::SeqCst)
            });
            let nonce = self.nonce.load(std::sync::atomic::Ordering::SeqCst);
            let row = |delta: Value| json!({"time":nonce,"hash":self.hash,"delta":delta});
            if owner.eq_ignore_ascii_case(OWNER) {
                Ok(json!([row(source.clone())]))
            } else if owner.eq_ignore_ascii_case(&self.proxy) {
                Ok(json!([
                    // Hyperliquid exposes the same owner-oriented send row
                    // through the recipient's ledger query; the independent
                    // recipient-side evidence is its later proxy->Exchange
                    // spot transfer, whose decimal formatting may differ
                    // from the frozen action amount.
                    row(source),
                    {"time":nonce+1,"hash":"0xproxy-outflow","delta":{
                        "type":"spotTransfer", "user":self.proxy,
                        "destination":"0x6cd5661646289fb6e65ea5c032310fded797d0a2",
                        "token":"USDC", "amount":ledger_amount
                    }}
                ]))
            } else {
                Err(GoFinError::validation("unexpected test ledger owner"))
            }
        }

        async fn explorer(
            &self,
            request: gofin_exchange::ccxt::HyperliquidExplorerRequest,
        ) -> Result<Value, GoFinError> {
            match request {
                gofin_exchange::ccxt::HyperliquidExplorerRequest::UserDetails { .. } => {
                    self.history_calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if !self
                        .history_visible
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        return Ok(json!({"txs":[]}));
                    }
                    Ok(json!({"txs":[{"user":OWNER,"hash":self.hash,"action":{
                        "type":"sendAsset", "nonce":self.nonce.load(std::sync::atomic::Ordering::SeqCst), "time":self.nonce.load(std::sync::atomic::Ordering::SeqCst),
                        "destination":self.proxy, "amount":self.amount, "token":"USDC",
                        "hyperliquidChain":"Mainnet", "signatureChainId":"0x3e7",
                        "sourceDex":"spot", "destinationDex":"spot", "fromSubAccount":""
                    }}]}))
                }
                gofin_exchange::ccxt::HyperliquidExplorerRequest::TxDetails { hash } => Ok(
                    json!({"tx":{"hash":hash,"user":OWNER,"block":100,"error":if self.canonical_error { json!("rejected") } else { Value::Null },"action":{
                        "type":"sendAsset", "nonce":self.nonce.load(std::sync::atomic::Ordering::SeqCst), "time":self.nonce.load(std::sync::atomic::Ordering::SeqCst),
                        "destination":self.proxy, "amount":self.amount, "token":"USDC",
                        "hyperliquidChain":"Mainnet", "signatureChainId":"0x3e7",
                        "sourceDex":"spot", "destinationDex":"spot", "fromSubAccount":""
                    }}}),
                ),
            }
        }

        async fn dispatch(
            &self,
            _: &gofin_hyperliquid::HyperliquidAccount,
            envelope: &gofin_exchange::ccxt::HyperliquidActionEnvelope,
            _: Arc<dyn gofin_domain::SubmissionJournal>,
        ) -> Result<Value, GoFinError> {
            self.dispatch_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if matches!(
                envelope.action,
                gofin_exchange::ccxt::HyperliquidAction::Noop {}
            ) {
                return if self
                    .dispatch_succeeds
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    Ok(json!({"status":"ok","response":{"type":"default"}}))
                } else {
                    Err(GoFinError::new(
                        "VENUE_TIMEOUT",
                        "submission",
                        "synthetic lost Noop response",
                        "reconcile the exact child",
                    ))
                };
            }
            if let gofin_exchange::ccxt::HyperliquidAction::PapertradeCoreDeposit {
                nonce, ..
            } = &envelope.action
            {
                self.nonce
                    .store(*nonce, std::sync::atomic::Ordering::SeqCst);
                self.sweep_timestamp
                    .store(nonce / 1_000 + 1, std::sync::atomic::Ordering::SeqCst);
            }
            if self
                .core_dispatch_succeeds
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                Ok(json!({"status":"ok"}))
            } else {
                Err(GoFinError::new(
                    "VENUE_TIMEOUT",
                    "submission",
                    "synthetic Core send response was lost after dispatch",
                    "reconcile the exact original submission; do not resend it",
                ))
            }
        }
    }

    struct CoreProofRpc {
        hash: String,
        owner: String,
        factory_proxy: String,
        block_hash: String,
        swept_data: String,
        sweep_timestamp: Arc<std::sync::atomic::AtomicU64>,
    }

    #[derive(Clone)]
    struct CoreWithdrawalFixture {
        intent_id: String,
        transaction_input: String,
        logs: Value,
        transaction_hash: String,
        block_hash: String,
        proxy: String,
        timestamp: u64,
    }

    struct CoreWithdrawalRpc {
        fixture: Arc<Mutex<Option<CoreWithdrawalFixture>>>,
        hyperevm_send_submitted: Arc<std::sync::atomic::AtomicBool>,
        hyperevm_send_time_ms: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for CoreWithdrawalRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value, GoFinError> {
            let fixture = self.fixture.lock().unwrap().clone();
            let owner = format!(
                "{:#x}",
                gofin_evm::address("0x7e5f4552091a69125d5dfcb7b8c2659029395bdf").unwrap()
            );
            let proxy = fixture
                .as_ref()
                .map(|fixture| fixture.proxy.clone())
                .unwrap_or_else(|| "0x18b9c4b188f556242edbfd5496d0840130f3ebb0".into());
            let tx_hash = fixture.as_ref().map_or_else(
                || format!("0x{}", "42".repeat(32)),
                |fixture| fixture.transaction_hash.clone(),
            );
            let block_hash = fixture.as_ref().map_or_else(
                || format!("0x{}", "bb".repeat(32)),
                |fixture| fixture.block_hash.clone(),
            );
            let logs = fixture
                .as_ref()
                .map_or_else(|| json!([]), |f| f.logs.clone());
            match method {
                "eth_chainId" => Ok(json!("0x3e7")),
                "eth_blockNumber" => Ok(json!(if self
                    .hyperevm_send_submitted
                    .load(std::sync::atomic::Ordering::SeqCst)
                { "0x71" } else { "0x70" })),
                "eth_getBlockReceiptsWithSystemTx" => Ok(if self
                    .hyperevm_send_submitted
                    .load(std::sync::atomic::Ordering::SeqCst)
                    && params[0].as_str() == Some("0x71")
                {
                    hyperevm_system_receipts(&owner, 18_900_000)
                } else {
                    json!([])
                }),
                "eth_getSystemTxsByBlockNumber" => Ok(if self
                    .hyperevm_send_submitted
                    .load(std::sync::atomic::Ordering::SeqCst)
                    && params[0].as_str() == Some("0x71")
                {
                    hyperevm_system_transactions(&owner, 18_900_000)
                } else {
                    json!([])
                }),
                "eth_call" => {
                    let data = params[0]["data"].as_str().unwrap_or_default();
                    if data.starts_with("0xcab7e8eb") {
                        return Ok(json!(format!("0x{}", "0".repeat(64))));
                    }
                    let mut word = [0u8; 32];
                    hex::decode_to_slice(&proxy[2..], &mut word[12..])
                        .map_err(|_| GoFinError::validation("bad fixture proxy"))?;
                    Ok(json!(format!("0x{}", hex::encode(word))))
                }
                "eth_getLogs" => {
                    let wanted = params[0]["topics"][3].as_str().unwrap_or_default();
                    Ok(json!(fixture
                        .as_ref()
                        .filter(|f| f.intent_id.eq_ignore_ascii_case(wanted))
                        .map(|f| vec![{
                            let marker = &f.logs[2];
                            json!({
                                "address":gofin_papertrade::BATCH_EXECUTOR,
                                "topics":marker["topics"],
                                "data":marker["data"],
                                "transactionHash":f.transaction_hash,
                                "blockNumber":"0x70",
                                "blockHash":f.block_hash,
                                "transactionIndex":"0x0",
                                "logIndex":"0x2"
                            })
                        }])
                        .unwrap_or_default()))
                }
                "eth_getTransactionByHash" => Ok(fixture.as_ref().map_or(Value::Null, |f| json!({
                    "hash":f.transaction_hash,"from":owner,"to":gofin_papertrade::BATCH_EXECUTOR,
                    "input":f.transaction_input,"blockNumber":"0x70","blockHash":f.block_hash
                }))),
                "eth_getTransactionReceipt" => Ok(json!({
                    "transactionHash":tx_hash,"blockNumber":"0x70","blockHash":block_hash,
                    "status":"0x1","gasUsed":"0x5208","effectiveGasPrice":"0x2","logs":logs
                })),
                "eth_getStorageAt" => {
                    let contract = params[0].as_str().unwrap_or_default();
                    let implementation = if contract.eq_ignore_ascii_case(gofin_papertrade::EXCHANGE) {
                        "0f3febfc94876b0c895f7acecdc8f5c863b549ee"
                    } else if contract.eq_ignore_ascii_case(gofin_papertrade::PAPER_STAKING) {
                        "aad6c7b0cc3014fc80ffedae0ed7ce5967b0f016"
                    } else {
                        "e0bc9cc7b067060aafccbaa3e250700c9cdd7262"
                    };
                    Ok(json!(format!("0x{}{}", "0".repeat(24), implementation)))
                }
                "eth_getCode" => Ok(json!("0x6000")),
                "eth_getBlockByNumber" => {
                    let tag = params[0].as_str().unwrap_or("0x70");
                    let send_submitted = self
                        .hyperevm_send_submitted
                        .load(std::sync::atomic::Ordering::SeqCst);
                    let selected = if tag == "finalized" && send_submitted {
                        "0x71"
                    } else if tag == "finalized" {
                        "0x70"
                    } else {
                        tag
                    };
                    if selected == "0x71" && send_submitted {
                        let send_time = self
                            .hyperevm_send_time_ms
                            .load(std::sync::atomic::Ordering::SeqCst);
                        return Ok(json!({
                            "number":"0x71",
                            "hash":format!("0x{}", "cc".repeat(32)),
                            "timestamp":format!("0x{:x}", send_time / 1_000)
                        }));
                    }
                    Ok(json!({
                        "number":selected,
                        "hash":block_hash,
                        "timestamp":format!("0x{:x}",fixture.as_ref().map_or(1_800_000_000, |f| f.timestamp))
                    }))
                }
                _ => Err(GoFinError::validation("unexpected Core withdrawal RPC method")),
            }
        }
    }

    fn rpc_word(value: u128) -> [u8; 32] {
        let mut word = [0u8; 32];
        word[16..].copy_from_slice(&value.to_be_bytes());
        word
    }

    fn rpc_address_word(value: &str) -> [u8; 32] {
        let mut word = [0u8; 32];
        hex::decode_to_slice(&value[2..], &mut word[12..]).unwrap();
        word
    }

    const HYPEREVM_SEND_HASH: &str =
        "0x5555555555555555555555555555555555555555555555555555555555555555";

    fn hyperevm_system_receipts(owner: &str, amount_atoms: u64) -> Value {
        let wallet = gofin_bridge::cctp::hyperevm::CORE_DEPOSIT_WALLET;
        let token = gofin_bridge::cctp::hyperevm::NATIVE_USDC;
        let topic_address = |address: &str| format!("0x{}", hex::encode(rpc_address_word(address)));
        let transfer_topic = format!(
            "0x{}",
            hex::encode(gofin_evm::alloy::primitives::keccak256(
                b"Transfer(address,address,uint256)"
            ))
        );
        let withdraw_topic = format!(
            "0x{}",
            hex::encode(gofin_evm::alloy::primitives::keccak256(
                b"Withdraw(address,uint256)"
            ))
        );
        json!([{
            "transactionHash":HYPEREVM_SEND_HASH,
            "blockNumber":"0x71",
            "blockHash":format!("0x{}", "cc".repeat(32)),
            "status":"0x1",
            "logs":[
                {"address":token,"topics":[transfer_topic,topic_address(wallet),topic_address(owner)],"data":format!("0x{:064x}",amount_atoms),"logIndex":"0x0"},
                {"address":wallet,"topics":[withdraw_topic,topic_address(owner)],"data":format!("0x{:064x}",amount_atoms),"logIndex":"0x1"}
            ]
        }])
    }

    fn hyperevm_system_transactions(owner: &str, amount_atoms: u64) -> Value {
        let mut input = hex::encode(
            &gofin_evm::alloy::primitives::keccak256(b"transfer(address,uint256)")[..4],
        );
        input.push_str(&hex::encode(rpc_address_word(owner)));
        input.push_str(&format!("{:064x}", amount_atoms));
        json!([{
            "hash":HYPEREVM_SEND_HASH,
            "from":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
            "to":gofin_bridge::cctp::hyperevm::CORE_DEPOSIT_WALLET,
            "value":"0x0",
            "input":format!("0x{input}"),
            "blockNumber":"0x71",
            "blockHash":format!("0x{}", "cc".repeat(32)),
            "transactionIndex":"0x0"
        }])
    }

    fn core_withdrawal_fixture_from_post(
        signed: &Value,
        intent_id: &str,
        proxy: &str,
        timestamp: u64,
    ) -> CoreWithdrawalFixture {
        let user = signed["user"].as_str().unwrap();
        let amount = signed["amount"].as_str().unwrap().parse::<u64>().unwrap();
        let nonce = signed["nonce"].as_str().unwrap().parse::<u128>().unwrap();
        let deadline = signed["deadline"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap();
        let v = signed["v"]
            .as_u64()
            .map(u128::from)
            .or_else(|| signed["v"].as_str().and_then(|value| value.parse().ok()))
            .expect("signed EIP-712 v is an exact integer");
        let r = hex::decode(signed["r"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let s = hex::decode(signed["s"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let mut withdraw = hex::decode("8d09f23f").unwrap();
        for word in [
            rpc_address_word(user),
            rpc_word(amount.into()),
            rpc_word(nonce),
            rpc_word(deadline),
            rpc_word(v),
        ] {
            withdraw.extend_from_slice(&word);
        }
        assert_eq!(r.len(), 32);
        assert_eq!(s.len(), 32);
        withdraw.extend_from_slice(&r);
        withdraw.extend_from_slice(&s);

        // execute((address,bytes,bool,bytes32,bytes32)[]) with one fixed call.
        let mut batch = hex::decode("251fb27d").unwrap();
        batch.extend_from_slice(&rpc_word(32)); // top-level dynamic array offset
        batch.extend_from_slice(&rpc_word(1)); // array length
        batch.extend_from_slice(&rpc_word(32)); // tuple offset relative to element-head area
        batch.extend_from_slice(&rpc_address_word(gofin_papertrade::EXCHANGE));
        batch.extend_from_slice(&rpc_word(160)); // tuple bytes offset
        batch.extend_from_slice(&rpc_word(1)); // allow failure
        batch.extend_from_slice(&[0u8; 32]); // group id
        batch.extend_from_slice(&hex::decode(intent_id.trim_start_matches("0x")).unwrap());
        batch.extend_from_slice(&rpc_word(withdraw.len() as u128));
        batch.extend_from_slice(&withdraw);
        while !(batch.len() - 4).is_multiple_of(32) {
            batch.push(0);
        }

        let mut owner_topic = [0u8; 32];
        owner_topic[12..].copy_from_slice(&hex::decode(&user[2..]).unwrap());
        let mut intent_topic = [0u8; 32];
        intent_topic.copy_from_slice(&hex::decode(intent_id.trim_start_matches("0x")).unwrap());
        let mut batch_topic = [0u8; 32];
        batch_topic[31] = 0x77;
        let mut item_index = [0u8; 32];
        item_index[31] = 0;
        let received_8dp = u128::from(amount) * 100;
        let removed_wad = u128::from(amount) * 1_000_000_000_000;
        let mut withdrawal_data = Vec::new();
        withdrawal_data.extend_from_slice(&rpc_word(removed_wad));
        withdrawal_data.extend_from_slice(&rpc_word(received_8dp));
        let mut writer_payload = vec![1, 0, 0, 6];
        writer_payload.extend_from_slice(&rpc_address_word(user));
        writer_payload.extend_from_slice(&rpc_word(0));
        writer_payload.extend_from_slice(&rpc_word(received_8dp));
        writer_payload.extend_from_slice(&[0; 28]);
        let mut writer_data = Vec::new();
        writer_data.extend_from_slice(&rpc_word(32));
        writer_data.extend_from_slice(&rpc_word(100));
        writer_data.extend_from_slice(&writer_payload);
        let mut call_data = Vec::new();
        call_data.extend_from_slice(&rpc_word(1));
        call_data.extend_from_slice(&rpc_word(64));
        call_data.extend_from_slice(&rpc_word(0));
        let mut logs = json!([
            {"address":"0x3333333333333333333333333333333333333333","topics":["0x8c7f585fb295f7eb1e6aeb8fba61b23a4fe60beda405f0045073b185c74412e3",format!("0x{}",hex::encode(rpc_address_word(gofin_papertrade::EXCHANGE)))],"data":format!("0x{}",hex::encode(writer_data)),"logIndex":"0x0"},
            {"address":gofin_papertrade::EXCHANGE,"topics":["0x95d1a93901f24eeaa294588802f66fa008f9f3741060c9f79e5dab4709de5e75",format!("0x{}",hex::encode(owner_topic))],"data":format!("0x{}",hex::encode(withdrawal_data)),"logIndex":"0x1"},
            {"address":gofin_papertrade::BATCH_EXECUTOR,"topics":["0x8a23ef1120f3e42e722a0c0a599a66d55ab2650ecc39716f57e625684e349cad",format!("0x{}",hex::encode(batch_topic)),format!("0x{}",hex::encode(item_index)),format!("0x{}",hex::encode(intent_topic))],"data":format!("0x{}",hex::encode(call_data)),"logIndex":"0x2"}
        ]);
        for log in logs.as_array_mut().unwrap() {
            log["transactionHash"] = json!(format!("0x{}", "42".repeat(32)));
            log["blockNumber"] = json!("0x70");
            log["blockHash"] = json!(format!("0x{}", "bb".repeat(32)));
            log["transactionIndex"] = json!("0x0");
        }
        CoreWithdrawalFixture {
            intent_id: intent_id.to_owned(),
            transaction_input: format!("0x{}", hex::encode(batch)),
            logs,
            transaction_hash: format!("0x{}", "42".repeat(32)),
            block_hash: format!("0x{}", "bb".repeat(32)),
            proxy: proxy.to_owned(),
            timestamp,
        }
    }

    #[async_trait::async_trait]
    impl gofin_evm::EvmRpc for CoreProofRpc {
        async fn request(&self, method: &str, params: Value) -> gofin_domain::Result<Value> {
            let owner_topic = format!("0x{}{}", "00".repeat(12), &self.owner[2..]);
            let swept_log = json!({
                "address":gofin_papertrade::EXCHANGE,
                "topics":["0xc36b5179cb9c303b200074996eab2b3473eac370fdd7eba3bec636fe35109696",owner_topic],
                "data":self.swept_data,
                "blockNumber":"0x70","transactionIndex":"0x2","transactionHash":self.hash
            });
            let finalize_input = format!("0x6d142da1{}{}", "0".repeat(24), &self.owner[2..]);
            match method {
                "eth_chainId" => Ok(json!("0x3e7")),
                "eth_blockNumber" => Ok(json!("0x70")),
                "eth_call" => {
                    let mut word = [0u8; 32];
                    hex::decode_to_slice(&self.factory_proxy[2..], &mut word[12..])
                        .map_err(|_| GoFinError::validation("bad fixture proxy"))?;
                    Ok(json!(format!("0x{}", hex::encode(word))))
                }
                "eth_getLogs" => Ok(json!([swept_log])),
                "eth_getTransactionByHash" => Ok(json!({
                    "hash":self.hash,"to":gofin_papertrade::EXCHANGE,
                    "input":finalize_input,"from":OWNER,
                    "blockNumber":"0x70","blockHash":self.block_hash
                })),
                "eth_getTransactionReceipt" => Ok(json!({
                    "transactionHash":self.hash,"blockNumber":"0x70",
                    "blockHash":self.block_hash,"status":"0x1","gasUsed":"0x5208",
                    "effectiveGasPrice":"0x2","logs":[swept_log]
                })),
                "eth_getBlockByNumber" => {
                    let block = params[0].as_str().unwrap_or("0x64");
                    Ok(json!({
                        "number":block,"hash":self.block_hash,
                        "timestamp":format!("0x{:x}",self.sweep_timestamp.load(std::sync::atomic::Ordering::SeqCst))
                    }))
                }
                _ => Err(GoFinError::validation("unexpected Core proof RPC method")),
            }
        }
    }

    fn core_send_plan(proxy: &str, nonce: u64) -> BoundFunding {
        use gofin_hyperliquid::funding::{HyperliquidAccountMode, core_spot_deposit_action};
        let action = core_spot_deposit_action(
            HyperliquidAccountMode::UnifiedAccount,
            OWNER,
            proxy,
            "5",
            nonce,
        )
        .unwrap();
        BoundFunding {
            version: 1,
            operation_id: "core-lost-response".into(),
            intent: FundingIntent {
                direction: FundingDirection::Deposit,
                route: FundingRoute::Core,
                owner: OWNER.into(),
                amount_usdc_atoms: 5_000_000,
                request_id: "request-1".into(),
            },
            proxy: proxy.into(),
            source_identity: format!("hyperliquid:mainnet:{OWNER}:spot:primary"),
            destination_identity: format!("papertrade:mainnet:{OWNER}"),
            minimum_received_usdc_atoms: 5_000_000,
            bounds: gofin_papertrade::FundingFinancialBounds {
                source_usdc_atoms: 5_000_000,
                maximum_source_fee_usdc_atoms: 1_000_000,
                destination_min_usdc_atoms: 5_000_000,
                maximum_cctp_fee_usdc_atoms: 0,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
            cctp_quote: None,
            proxy_core_usdc_before_atoms: Some(0),
            core_owner_usdc_before_atoms: Some(40_000_000),
            core_sweep_from_block: Some(99),
            proxy_registration: Some(ProxyRegistrationProof {
                owner: OWNER.into(),
                api_proxy: proxy.into(),
                factory_proxy: proxy.into(),
                request_id: "papertrade-proxy-check:core-lost-response".into(),
            }),
            legs: vec![FundingLeg {
                ordinal: 0,
                submission_id: "papertrade-funding:core-lost-response:0".into(),
                action: FundingAction::CoreDeposit { action },
            }],
        }
    }

    async fn papertrade_http_fixture(proxy: String) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    let mut request = vec![0u8; 8192];
                    let read = stream.read(&mut request).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..read]);
                    let line = request.lines().next().unwrap_or_default();
                    let (status, body, content_type) = if line.starts_with("GET /trade/btc") {
                        (
                            "200 OK",
                            "<script src=\"/assets/build-fixture.js\"></script>".into(),
                            "text/html",
                        )
                    } else if line.starts_with("GET /assets/build-fixture.js") {
                        (
                            "200 OK",
                            "var e=`a1b2c3d4e5f6`;export{e as n};".into(),
                            "application/javascript",
                        )
                    } else if line.starts_with("POST /funding/deposits/check") {
                        (
                            "200 OK",
                            json!({"proxy":proxy}).to_string(),
                            "application/json",
                        )
                    } else if line.starts_with("POST /info") {
                        (
                            "200 OK",
                            json!({"balances":[]}).to_string(),
                            "application/json",
                        )
                    } else {
                        ("404 Not Found", "{}".into(), "application/json")
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{address}"), task)
    }

    async fn core_withdrawal_http_fixture(
        credentials: Arc<FundingTestCredentials>,
        chain: Arc<Mutex<Option<CoreWithdrawalFixture>>>,
        owner: String,
        proxy: String,
        amount_atoms: u64,
        timestamp: u64,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let credentials = credentials.clone();
                let chain = chain.clone();
                let owner = owner.clone();
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 8192];
                    let (header_end, content_length) = loop {
                        let count = stream.read(&mut buf).await.unwrap_or(0);
                        if count == 0 {
                            break (0, 0);
                        }
                        request.extend_from_slice(&buf[..count]);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.split_once(':')
                                        .filter(|(key, _)| {
                                            key.eq_ignore_ascii_case("content-length")
                                        })
                                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while request.len() < header_end.saturating_add(content_length) {
                        let count = stream.read(&mut buf).await.unwrap_or(0);
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&buf[..count]);
                    }
                    let text = String::from_utf8_lossy(&request);
                    let line = text.lines().next().unwrap_or_default();
                    let body = request
                        .get(header_end..header_end.saturating_add(content_length))
                        .unwrap_or_default();
                    let response = if line.starts_with("GET /trade/btc") {
                        (
                            "200 OK",
                            "<script src=\"/assets/build-fixture.js\"></script>".to_owned(),
                            "text/html",
                        )
                    } else if line.starts_with("GET /assets/build-fixture.js") {
                        (
                            "200 OK",
                            "var e=`a1b2c3d4e5f6`;export{e as n};".to_owned(),
                            "application/javascript",
                        )
                    } else if line.starts_with("GET /state/user/live?") {
                        let snapshot = json!({"wallet":owner,"indexedThroughBlock":"112","accountRevision":"4","funds":{"availableBalance":format!("{}{}",amount_atoms,"0".repeat(12))},"openPositions":[],"paper":{"balance":"0","staked":"0","pendingBase":"0","rewardDebt":"0"}});
                        (
                            "200 OK",
                            format!("event: wallet\ndata: {snapshot}\n\n"),
                            "text/event-stream",
                        )
                    } else if line.starts_with("POST /intents ") {
                        let post: Value = serde_json::from_slice(body).unwrap();
                        assert_eq!(post["action"], "withdrawToCore");
                        assert_eq!(post["withdrawal"], json!({"route":"hypercore"}));
                        let signed = &post["signed"];
                        let signed_nonce = signed["nonce"].as_str().unwrap_or_default();
                        let signed_deadline = signed["deadline"].as_str().unwrap_or_default();
                        let intent_id = credentials
                            .0
                            .lock()
                            .unwrap()
                            .iter()
                            .find_map(|(key, value)| {
                                if !key.starts_with("papertrade-funding-withdrawal-intent-") {
                                    return None;
                                }
                                let bound: gofin_papertrade::BoundIntent =
                                    serde_json::from_value(value.clone()).ok()?;
                                let intent = matches!(&bound.typed,
                                    gofin_papertrade::PapertradeTypedIntent::WithdrawToCore {
                                        user,
                                        amount,
                                        nonce,
                                        deadline,
                                        ..
                                    } if user.eq_ignore_ascii_case(&owner)
                                        && amount == &amount_atoms.to_string()
                                        && nonce == signed_nonce
                                        && deadline.to_string() == signed_deadline
                                );
                                intent.then_some(bound.intent_id)
                            })
                            .expect("protected withdrawal intent was persisted before /intents");
                        let fixture = core_withdrawal_fixture_from_post(
                            signed, &intent_id, &proxy, timestamp,
                        );
                        *chain.lock().unwrap() = Some(fixture);
                        ("200 OK", json!({"action":"withdrawToCore","intentId":intent_id,"queueRevision":1,"state":"queued"}).to_string(), "application/json")
                    } else {
                        ("404 Not Found", "{}".into(), "application/json")
                    };
                    let wire = format!(
                        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.0,
                        response.2,
                        response.1.len(),
                        response.1
                    );
                    let _ = stream.write_all(wire.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{address}"), task)
    }

    struct CoreWithdrawalTransport {
        owner: String,
        amount: String,
        hash: String,
        time_ms: Arc<std::sync::atomic::AtomicU64>,
        hyperevm_send_time_ms: Arc<std::sync::atomic::AtomicU64>,
        hyperevm_send_submitted: Arc<std::sync::atomic::AtomicBool>,
        hyperevm_send_nonce: Arc<std::sync::atomic::AtomicU64>,
        hyperevm_send_dispatch_count: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait::async_trait]
    impl gofin_hyperliquid::HyperliquidTransport for CoreWithdrawalTransport {
        async fn info(
            &self,
            request: gofin_exchange::ccxt::HyperliquidInfoRequest,
        ) -> Result<Value, GoFinError> {
            use gofin_exchange::ccxt::HyperliquidInfoRequest as Info;
            match request {
                Info::UserRole { .. } => Ok(json!({"role":"user"})),
                Info::UserAbstractionState { .. } => Ok(json!("unifiedAccount")),
                Info::UserDexAbstractionState { .. } => Ok(Value::Null),
                Info::UserNonFundingLedgerUpdates {
                    owner, end_time_ms, ..
                } if owner.eq_ignore_ascii_case(&self.owner) => {
                    // The initial Papertrade transfer fixture uses the query's
                    // bounded row timestamp. For sendAsset, use a separate
                    // stable canonical event timestamp across repeated reads.
                    let send_submitted = self
                        .hyperevm_send_submitted
                        .load(std::sync::atomic::Ordering::SeqCst);
                    let time_ms = if send_submitted {
                        self.hyperevm_send_time_ms
                            .load(std::sync::atomic::Ordering::SeqCst)
                    } else {
                        end_time_ms.unwrap_or_else(|| {
                            self.time_ms.load(std::sync::atomic::Ordering::SeqCst)
                        })
                    };
                    if send_submitted {
                        let time_ms = self
                            .hyperevm_send_time_ms
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let nonce = self
                            .hyperevm_send_nonce
                            .load(std::sync::atomic::Ordering::SeqCst);
                        return Ok(json!([{
                            "time":time_ms,"hash":HYPEREVM_SEND_HASH,"delta":{
                                "type":"send","user":self.owner,"destination":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
                                "sourceDex":"spot","destinationDex":"spot","token":"USDC","usdcValue":"18.9","amount":"18.9",
                                "fee":"0","nativeTokenFee":"0","feeToken":"USDC","nonce":nonce
                            }
                        }]));
                    }
                    self.time_ms
                        .store(time_ms, std::sync::atomic::Ordering::SeqCst);
                    Ok(json!([{
                        "time":time_ms,"hash":self.hash,"delta":{
                            "type":"spotTransfer","user":gofin_papertrade::EXCHANGE,
                            "destination":self.owner,"token":"USDC","amount":self.amount
                        }
                    }]))
                }
                _ => Err(GoFinError::validation(
                    "unexpected Core withdrawal info query",
                )),
            }
        }
        async fn explorer(
            &self,
            request: gofin_exchange::ccxt::HyperliquidExplorerRequest,
        ) -> Result<Value, GoFinError> {
            use gofin_exchange::ccxt::HyperliquidExplorerRequest as Explorer;
            match request {
                Explorer::UserDetails { user }
                    if user.eq_ignore_ascii_case(&self.owner)
                        && self
                            .hyperevm_send_submitted
                            .load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    let nonce = self
                        .hyperevm_send_nonce
                        .load(std::sync::atomic::Ordering::SeqCst);
                    Ok(
                        json!({"txs":[{"hash":HYPEREVM_SEND_HASH,"user":self.owner,"action":{
                            "type":"sendAsset","nonce":nonce,"hyperliquidChain":"Mainnet","signatureChainId":"0x3e7",
                            "token":"USDC","destination":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
                            "amount":"18.9","sourceDex":"spot","destinationDex":"spot","fromSubAccount":""
                        }}]}),
                    )
                }
                Explorer::TxDetails { hash }
                    if hash == HYPEREVM_SEND_HASH
                        && self
                            .hyperevm_send_submitted
                            .load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    let nonce = self
                        .hyperevm_send_nonce
                        .load(std::sync::atomic::Ordering::SeqCst);
                    let action_time = self
                        .hyperevm_send_time_ms
                        .load(std::sync::atomic::Ordering::SeqCst);
                    Ok(
                        json!({"tx":{"hash":HYPEREVM_SEND_HASH,"time":action_time,"error":null,"block":112,"user":self.owner,"action":{
                            "type":"sendAsset","nonce":nonce,"hyperliquidChain":"Mainnet","signatureChainId":"0x3e7",
                            "token":"USDC","destination":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
                            "amount":"18.9","sourceDex":"spot","destinationDex":"spot","fromSubAccount":""
                        }}}),
                    )
                }
                Explorer::TxDetails { hash } if hash == self.hash => Ok(json!({"tx":{
                    "hash":self.hash,"time":self.time_ms.load(std::sync::atomic::Ordering::SeqCst),"error":null,"block":112,"user":gofin_papertrade::EXCHANGE,
                    "action":{"type":"SystemSpotSendAction","destination":self.owner,"token":0,"wei":1_900_000_000u64}
                }})),
                _ => Err(GoFinError::validation(
                    "unexpected Core withdrawal explorer query",
                )),
            }
        }
        async fn dispatch(
            &self,
            _: &gofin_hyperliquid::HyperliquidAccount,
            envelope: &gofin_exchange::ccxt::HyperliquidActionEnvelope,
            _: Arc<dyn gofin_domain::SubmissionJournal>,
        ) -> Result<Value, GoFinError> {
            use gofin_exchange::ccxt::HyperliquidAction;
            match &envelope.action {
                HyperliquidAction::SendAsset { amount, nonce, .. } if amount == "18.9" => {
                    self.hyperevm_send_nonce
                        .store(*nonce, std::sync::atomic::Ordering::SeqCst);
                    self.hyperevm_send_time_ms.store(
                        chrono::Utc::now().timestamp_millis().max(0) as u64,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    self.hyperevm_send_submitted
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    self.hyperevm_send_dispatch_count
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(json!({"status":"ok"}))
                }
                _ => Err(GoFinError::validation(
                    "Unexpected Hyperliquid action in controlled withdrawal fixture",
                )),
            }
        }
    }

    const OWNER: &str = "0x5502f38ed8ad519c3b9635ba723dddb94e991ecd";
    const WALLET: &str = cctp::hyperevm::CORE_DEPOSIT_WALLET;

    #[test]
    fn unified_core_send_proves_full_credit_and_additional_usdc_fee() {
        let proxy = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
        let source = json!({
            "user":OWNER,"destination":proxy,"sourceDex":"spot","destinationDex":"spot",
            "token":"USDC","amount":"88.425521","usdcValue":"88.425521",
            "fee":"1.0","nativeTokenFee":"0.0","feeToken":"USDC"
        });
        let destination = json!({
            "user":proxy,"destination":OWNER,"sourceDex":"spot","destinationDex":"spot",
            "token":"USDC","amount":"88.425521","usdcValue":"88.425521",
            "fee":"1.0","nativeTokenFee":"0.0","feeToken":"USDC"
        });
        let effects = verify_core_send_ledger(
            &source,
            &destination,
            OWNER,
            proxy,
            88_425_521,
            1_000_000,
            "sendAsset",
        )
        .expect("reviewed unified SEND ledger shape");
        assert_eq!(effects.source_debit_atoms, 89_425_521);
        assert_eq!(effects.source_fee_atoms, 1_000_000);
        assert_eq!(effects.destination_credit_atoms, 88_425_521);

        assert!(
            verify_core_send_ledger(
                &source,
                &destination,
                OWNER,
                proxy,
                88_425_521,
                999_999,
                "sendAsset",
            )
            .is_none()
        );
        let mut wrong_recipient = destination.clone();
        wrong_recipient["user"] = json!(OWNER);
        assert!(
            verify_core_send_ledger(
                &source,
                &wrong_recipient,
                OWNER,
                proxy,
                88_425_521,
                1_000_000,
                "sendAsset",
            )
            .is_none()
        );

        // Live Hyperliquid returns the same owner-oriented ledger row for the
        // sender and recipient queries. The row itself binds owner, proxy,
        // principal, fee and nonce; the later proxy outflow is independently
        // checked by the host before Papertrade settlement.
        let single = json!({
            "type":"send","user":OWNER,"destination":proxy,
            "sourceDex":"spot","destinationDex":"spot","token":"USDC",
            "amount":"19.0","usdcValue":"19.0","fee":"1.0",
            "feeToken":"USDC","nativeTokenFee":"0.0","nonce":1_792_187_351_774u64
        });
        let effects = verify_core_send_single_ledger(
            &single,
            OWNER,
            proxy,
            19_000_000,
            1_000_000,
            "sendAsset",
            1_792_187_351_774,
        )
        .expect("canonical owner-oriented SEND row");
        assert_eq!(effects.source_debit_atoms, 20_000_000);
        assert_eq!(effects.destination_credit_atoms, 19_000_000);

        for (field, wrong) in [
            ("user", json!(proxy)),
            ("destination", json!(OWNER)),
            ("amount", json!("19.000001")),
            ("fee", json!("1.000001")),
            ("nonce", json!(1_792_187_351_773u64)),
        ] {
            let mut altered = single.clone();
            altered[field] = wrong;
            assert!(
                verify_core_send_single_ledger(
                    &altered,
                    OWNER,
                    proxy,
                    19_000_000,
                    1_000_000,
                    "sendAsset",
                    1_792_187_351_774,
                )
                .is_none(),
                "accepted altered {field}"
            );
        }

        let default_mode = json!({
            "type":"spotTransfer","user":OWNER,"destination":proxy,
            "token":"USDC","amount":"30849.91","usdcValue":"30849.91",
            "fee":"1.0","nativeTokenFee":"0.0","feeToken":"USDC",
            "nonce":1_791_833_252_499u64
        });
        assert_eq!(
            verify_core_send_single_ledger(
                &default_mode,
                OWNER,
                proxy,
                30_849_910_000,
                1_000_000,
                "spotSend",
                1_791_833_252_499,
            )
            .unwrap()
            .source_debit_atoms,
            30_850_910_000
        );

        let native_fee = json!({
            "type":"send","user":OWNER,"destination":gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
            "sourceDex":"spot","destinationDex":"spot","token":"USDC",
            "amount":"19.9","usdcValue":"19.9","fee":"0.0",
            "feeToken":"","nativeTokenFee":"0.001","nonce":103u64
        });
        let effects = verify_core_send_single_ledger_with_native_fee(
            &native_fee,
            OWNER,
            gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
            19_900_000,
            100_000,
            100_000,
            "sendAsset",
            103,
        )
        .expect("HYPE-denominated Core send fee is separately bounded");
        assert_eq!(effects.source_debit_atoms, 19_900_000);
        assert_eq!(effects.source_fee_atoms, 0);
        assert_eq!(effects.source_native_fee_atoms, 100_000);
        assert_eq!(effects.destination_credit_atoms, 19_900_000);
        assert!(
            verify_core_send_single_ledger_with_native_fee(
                &native_fee,
                OWNER,
                gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
                19_900_000,
                100_000,
                99_999,
                "sendAsset",
                103,
            )
            .is_none()
        );
        let mut double_fee = native_fee;
        double_fee["fee"] = json!("0.000001");
        double_fee["feeToken"] = json!("USDC");
        assert!(
            verify_core_send_single_ledger_with_native_fee(
                &double_fee,
                OWNER,
                gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS,
                19_900_000,
                100_000,
                100_000,
                "sendAsset",
                103,
            )
            .is_none()
        );
    }

    #[test]
    fn source_debit_requires_exact_usdc_transfer_endpoints_and_amount() {
        let owner_topic = format!("0x{}{}", "00".repeat(12), &OWNER[2..]);
        let wallet_topic = format!("0x{}{}", "00".repeat(12), &WALLET[2..]);
        let receipt = json!({"logs":[{
            "address":cctp::hyperevm::NATIVE_USDC,
            "topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",owner_topic,wallet_topic],
            "data":"0x00000000000000000000000000000000000000000000000000000000000f4240"
        }]});
        assert!(has_exact_usdc_transfer(&receipt, OWNER, WALLET, 1_000_000).unwrap());
        assert!(!has_exact_usdc_transfer(&receipt, OWNER, WALLET, 999_999).unwrap());
        assert!(
            !has_exact_usdc_transfer(
                &receipt,
                OWNER,
                "0x0000000000000000000000000000000000000001",
                1_000_000
            )
            .unwrap()
        );
    }

    #[test]
    fn sweep_must_be_the_owner_specific_exchange_finalize_call() {
        let input = format!("0x6d142da1{}{}", "0".repeat(24), &OWNER[2..]);
        assert!(sweep_finalize_matches(
            &json!({"to":gofin_papertrade::EXCHANGE,"input":input}),
            OWNER
        ));
        assert!(!sweep_finalize_matches(
            &json!({"to":gofin_papertrade::EXCHANGE,"input":format!("0x6d142da1{}{}", "0".repeat(24), "11".repeat(20))}),
            OWNER
        ));
        assert!(!sweep_finalize_matches(
            &json!({"to":WALLET,"input":format!("0x6d142da1{}{}", "0".repeat(24), &OWNER[2..])}),
            OWNER
        ));
    }

    #[test]
    fn sweep_scan_order_excludes_events_before_the_source_write() {
        let source = (100, 4);
        assert!(
            !event_position(&json!({"blockNumber":"0x64","transactionIndex":"0x4"}))
                .is_some_and(|position| position > source)
        );
        assert!(
            event_position(&json!({"blockNumber":"0x64","transactionIndex":"0x5"}))
                .is_some_and(|position| position > source)
        );
        assert!(
            event_position(&json!({"blockNumber":"0x65","transactionIndex":"0x0"}))
                .is_some_and(|position| position > source)
        );
    }

    #[test]
    fn unified_core_usdc_balance_keeps_native_precision_then_floors_to_six_atoms() {
        assert_eq!(
            parse_hyperliquid_usdc_atoms("39.50387297").unwrap(),
            39_503_872
        );
        assert_eq!(parse_hyperliquid_usdc_atoms("0.00000099").unwrap(), 0);
        assert!(parse_hyperliquid_usdc_atoms("39.503872979").is_err());
    }

    #[test]
    fn core_preflight_reserves_the_full_one_usdc_source_fee_bound() {
        assert!(!core_available_covers_source_bound(
            19_500_000, 19_000_000, 1_000_000
        ));
        assert!(core_available_covers_source_bound(
            20_000_000, 19_000_000, 1_000_000
        ));
        assert!(!core_available_covers_source_bound(u64::MAX, u64::MAX, 1));
    }

    #[tokio::test]
    async fn multiroute_core_withdraw_preflight_uses_factory_identity_without_evm_fee() {
        let proxy = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
        let credentials = Arc::new(FundingTestCredentials(Mutex::new(BTreeMap::new())));
        let runtime = Arc::new(gofin_evm::EvmRuntime::new(
            Arc::new(CoreProofRpc {
                hash: format!("0x{}", "43".repeat(32)),
                owner: OWNER.into(),
                factory_proxy: proxy.into(),
                block_hash: format!("0x{}", "bb".repeat(32)),
                swept_data: format!("0x{:064x}", 10_000_000u128 * 1_000_000_000_000),
                sweep_timestamp: Arc::new(std::sync::atomic::AtomicU64::new(1_800_000_000)),
            }),
            999,
            None,
        ));
        let native = NativeHyperEvmFundingServices::new(
            credentials.clone(),
            runtime,
            OWNER,
            gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount,
            "10000000000000000",
            Arc::new(crate::papertrade_runtime::PapertradeHttp::new().unwrap()),
            1,
            false,
        )
        .unwrap();
        let transport = Arc::new(CoreEvidenceTransport {
            proxy: proxy.into(),
            hash: format!("0x{}", "43".repeat(32)),
            nonce: std::sync::atomic::AtomicU64::new(1_800_000_000_000),
            sweep_timestamp: Arc::new(std::sync::atomic::AtomicU64::new(1_800_000_000)),
            amount: "10".into(),
            source_fee: "0".into(),
            dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            core_dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            history_visible: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            canonical_error: false,
            empty_ledger: true,
            history_calls: std::sync::atomic::AtomicUsize::new(0),
            dispatch_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let services = MultiRouteFundingServices::new(
            native,
            credentials,
            transport,
            gofin_hyperliquid::HyperliquidAccount {
                name: "test-core-owner".into(),
                owner: OWNER.into(),
                credential_ref: Some("test-ref".into()),
                signing_enabled: true,
                execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            },
        )
        .unwrap();

        let preflight = services
            .preflight(&FundingIntent {
                direction: FundingDirection::Withdraw,
                route: FundingRoute::Core,
                owner: OWNER.into(),
                amount_usdc_atoms: 10_000_000,
                request_id: "core-withdraw-preflight".into(),
            })
            .await
            .unwrap();

        assert_eq!(preflight.factory_proxy, proxy);
        assert!(preflight.native_fee_asset.is_none());
        assert!(preflight.maximum_native_fee_atoms.is_none());
        assert!(preflight.proxy_core_usdc_before_atoms.is_none());
    }

    #[test]
    fn hyperevm_activation_fee_is_the_exact_gross_to_swept_difference() {
        assert_eq!(
            observed_activation_fee_atoms(20_000_000, 19_000_000, 1_000_000).unwrap(),
            1_000_000
        );
        assert!(observed_activation_fee_atoms(20_000_000, 18_999_999, 1_000_000).is_err());
        assert!(observed_activation_fee_atoms(19_000_000, 20_000_000, 1_000_000).is_err());
    }

    #[test]
    fn unified_core_deposit_action_is_exact_proxy_and_amount() {
        use gofin_hyperliquid::funding::{HyperliquidAccountMode, core_spot_deposit_action};
        let action = core_spot_deposit_action(
            HyperliquidAccountMode::UnifiedAccount,
            OWNER,
            "0xb21d281dedb17ae5b501f6aa8256fe38c4e45757",
            "5.25",
            1_800_000_000_000,
        )
        .unwrap();
        let plan = BoundFunding {
            version: 1,
            operation_id: "core-deposit-test".into(),
            intent: FundingIntent {
                direction: FundingDirection::Deposit,
                route: FundingRoute::Core,
                owner: OWNER.into(),
                amount_usdc_atoms: 5_250_000,
                request_id: "request-1".into(),
            },
            proxy: "0xb21d281dedb17ae5b501f6aa8256fe38c4e45757".into(),
            source_identity: format!(
                "hyperliquid:mainnet:{}:spot:primary",
                OWNER.to_ascii_lowercase()
            ),
            destination_identity: format!("papertrade:mainnet:{}", OWNER.to_ascii_lowercase()),
            minimum_received_usdc_atoms: 4_250_000,
            bounds: gofin_papertrade::FundingFinancialBounds {
                source_usdc_atoms: 5_250_000,
                maximum_source_fee_usdc_atoms: 1_000_000,
                destination_min_usdc_atoms: 5_250_000,
                maximum_cctp_fee_usdc_atoms: 0,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
            cctp_quote: None,
            proxy_core_usdc_before_atoms: Some(0),
            core_owner_usdc_before_atoms: Some(10_000_000),
            core_sweep_from_block: Some(1),
            proxy_registration: None,
            legs: vec![FundingLeg {
                ordinal: 0,
                submission_id: "papertrade-funding:core-deposit-test:0".into(),
                action: FundingAction::CoreDeposit { action },
            }],
        };
        let fields = MultiRouteFundingServices::core_action_fields(&plan.legs[0], &plan).unwrap();
        assert_eq!(fields.0, "unified_account");
        assert_eq!(fields.1, plan.proxy);
        assert_eq!(fields.2, "5.25");
        assert_eq!(fields.3, 1_800_000_000_000);
    }

    #[tokio::test]
    async fn core_lost_response_reconciles_original_nonce_and_persists_hash_without_resend() {
        let proxy = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 10_000;
        let hash = "0x076094aa956aa19c08da04469d044302050f0090306dc06eab293ffd546e7b86";
        let credentials = Arc::new(FundingTestCredentials(Mutex::new(BTreeMap::new())));
        let sweep_timestamp = Arc::new(std::sync::atomic::AtomicU64::new((nonce + 10_000) / 1_000));
        let transport = Arc::new(CoreEvidenceTransport {
            proxy: proxy.into(),
            hash: hash.into(),
            nonce: std::sync::atomic::AtomicU64::new(nonce),
            sweep_timestamp: sweep_timestamp.clone(),
            amount: "5".into(),
            source_fee: "1".into(),
            dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            core_dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            history_visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            canonical_error: false,
            empty_ledger: false,
            history_calls: std::sync::atomic::AtomicUsize::new(0),
            dispatch_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let block_hash = format!("0x{}", "bb".repeat(32));
        let runtime = Arc::new(gofin_evm::EvmRuntime::new(
            Arc::new(CoreProofRpc {
                hash: hash.into(),
                owner: OWNER.into(),
                factory_proxy: proxy.into(),
                block_hash,
                swept_data: format!("0x{:064x}", 5_000_000u128 * 1_000_000_000_000),
                sweep_timestamp,
            }),
            999,
            None,
        ));
        let native = NativeHyperEvmFundingServices::new(
            credentials.clone(),
            runtime,
            OWNER,
            gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount,
            "10000000000000000",
            Arc::new(crate::papertrade_runtime::PapertradeHttp::new().unwrap()),
            1,
            false,
        )
        .unwrap();
        let services = Arc::new(
            MultiRouteFundingServices::new(
                native,
                credentials.clone(),
                transport.clone(),
                gofin_hyperliquid::HyperliquidAccount {
                    name: "test-core-owner".into(),
                    owner: OWNER.into(),
                    credential_ref: Some("test-ref".into()),
                    signing_enabled: true,
                    execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
                },
            )
            .unwrap(),
        );
        let adapter = PapertradeFundingHostAdapter::new(services.clone());
        let plan = core_send_plan(proxy, nonce);
        credentials
            .put_papertrade_runtime_record(
                &format!("papertrade-funding-plan:{}", plan.operation_id),
                &serde_json::to_value(&plan).unwrap(),
            )
            .unwrap();
        credentials
            .put_papertrade_runtime_record(
                &MultiRouteFundingServices::record_key(
                    &plan.legs[0].submission_id,
                    "dispatch_attempted",
                ),
                &json!({"nonce":nonce}),
            )
            .unwrap();

        // The first reconcile models an accepted Core write whose HTTP result
        // was lost. It locates the exact action by persisted nonce, then binds
        // the discovered native hash before reading TxDetails and both ledgers.
        let first = crate::papertrade_funding::reconcile_funding(&adapter, &plan)
            .await
            .unwrap();
        assert_eq!(
            first.status,
            gofin_papertrade::FundingProgressStatus::Settled
        );
        let receipt = first
            .receipt
            .expect("the actual host adapter proves credit");
        assert_eq!(receipt.source_debit_usdc_atoms, 6_000_000);
        assert_eq!(receipt.actual_source_fee_usdc_atoms, 1_000_000);
        assert_eq!(receipt.destination_credit_usdc_atoms, 5_000_000);
        assert!(receipt.evidence_id.contains(hash));
        let hash_key = MultiRouteFundingServices::record_key(
            &plan.legs[0].submission_id,
            "discovered_action_hash",
        );
        assert_eq!(
            credentials
                .papertrade_runtime_record(&hash_key)
                .unwrap()
                .unwrap()["hash"],
            hash
        );

        // Restart recovery reuses the durable native hash and never resends.
        let second = crate::papertrade_funding::reconcile_funding(&adapter, &plan)
            .await
            .unwrap();
        assert_eq!(
            second.status,
            gofin_papertrade::FundingProgressStatus::Settled
        );
        assert_eq!(
            transport
                .history_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            transport
                .dispatch_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn core_rejection_requires_exact_canonical_error_and_no_ledger_effects() {
        let proxy = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
        let hash = format!("0x{}", "43".repeat(32));
        let nonce = 1_792_185_442_312u64;
        let credentials = Arc::new(FundingTestCredentials(Mutex::new(BTreeMap::new())));
        let sweep_timestamp = Arc::new(std::sync::atomic::AtomicU64::new(nonce / 1_000));
        let transport = Arc::new(CoreEvidenceTransport {
            proxy: proxy.into(),
            hash: hash.clone(),
            nonce: std::sync::atomic::AtomicU64::new(nonce),
            sweep_timestamp: sweep_timestamp.clone(),
            amount: "5".into(),
            source_fee: "1".into(),
            dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            core_dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            history_visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            canonical_error: true,
            empty_ledger: true,
            history_calls: std::sync::atomic::AtomicUsize::new(0),
            dispatch_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let runtime = Arc::new(gofin_evm::EvmRuntime::new(
            Arc::new(CoreProofRpc {
                hash: hash.clone(),
                owner: OWNER.into(),
                factory_proxy: proxy.into(),
                block_hash: format!("0x{}", "bb".repeat(32)),
                swept_data: format!("0x{:064x}", 5_000_000u128 * 1_000_000_000_000),
                sweep_timestamp,
            }),
            999,
            None,
        ));
        let native = NativeHyperEvmFundingServices::new(
            credentials.clone(),
            runtime,
            OWNER,
            gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount,
            "10000000000000000",
            Arc::new(crate::papertrade_runtime::PapertradeHttp::new().unwrap()),
            1,
            false,
        )
        .unwrap();
        let services = MultiRouteFundingServices::new(
            native,
            credentials.clone(),
            transport,
            gofin_hyperliquid::HyperliquidAccount {
                name: "test-core-owner".into(),
                owner: OWNER.into(),
                credential_ref: Some("test-ref".into()),
                signing_enabled: true,
                execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
            },
        )
        .unwrap();
        let plan = core_send_plan(proxy, nonce);
        let proof = services.core_action_evidence(&plan, nonce).await.unwrap();
        assert_eq!(
            proof.as_ref().unwrap()["kind"],
            "canonical_hyperliquid_action_rejection"
        );
        assert_eq!(proof.as_ref().unwrap()["source_ledger_effects"], 0);

        // An error-bearing canonical transaction with any matching account
        // ledger movement is ambiguous, never a zero-effect rejection.
        let transport_with_effects = Arc::new(CoreEvidenceTransport {
            proxy: proxy.into(),
            hash,
            nonce: std::sync::atomic::AtomicU64::new(nonce),
            sweep_timestamp: Arc::new(std::sync::atomic::AtomicU64::new(nonce / 1_000)),
            amount: "5".into(),
            source_fee: "1".into(),
            dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            core_dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            history_visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            canonical_error: true,
            empty_ledger: false,
            history_calls: std::sync::atomic::AtomicUsize::new(0),
            dispatch_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let native = NativeHyperEvmFundingServices::new(
            credentials.clone(),
            Arc::new(gofin_evm::EvmRuntime::new(
                Arc::new(CoreProofRpc {
                    hash: format!("0x{}", "44".repeat(32)),
                    owner: OWNER.into(),
                    factory_proxy: proxy.into(),
                    block_hash: format!("0x{}", "bc".repeat(32)),
                    swept_data: format!("0x{:064x}", 5_000_000u128 * 1_000_000_000_000),
                    sweep_timestamp: Arc::new(std::sync::atomic::AtomicU64::new(nonce / 1_000)),
                }),
                999,
                None,
            )),
            OWNER,
            gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount,
            "10000000000000000",
            Arc::new(crate::papertrade_runtime::PapertradeHttp::new().unwrap()),
            1,
            false,
        )
        .unwrap();
        let with_effects = MultiRouteFundingServices::new(
            native,
            services.credentials.clone(),
            transport_with_effects,
            services.hyperliquid_account.clone(),
        )
        .unwrap();
        assert!(
            with_effects
                .core_action_evidence(&plan, nonce)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn core_dispatch_error_persistence_is_bounded_and_redacted() {
        let error = GoFinError::new(
            "VENUE_TIMEOUT",
            "submission",
            "private signed payload 0xsecret must not persist",
            "reconcile",
        )
        .with_details(json!({"http_status":422,"exchange_code":"intake_action_paused","body":"signed payload"}));
        let outcome = MultiRouteFundingServices::sanitized_core_dispatch_outcome(&Err(error));
        assert_eq!(outcome["kind"], "transport_error");
        assert_eq!(outcome["code"], "VENUE_TIMEOUT");
        assert_eq!(outcome["http_status"], 422);
        assert_eq!(outcome["exchange_code"], "intake_action_paused");
        let encoded = outcome.to_string();
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("signed"));
        assert!(!encoded.contains("body"));

        let venue = MultiRouteFundingServices::sanitized_core_dispatch_outcome(&Ok(json!({
            "status":"err",
            "response":"raw venue message should not persist"
        })));
        assert_eq!(venue["status"], "err");
        assert!(venue["response_present"].as_bool().unwrap());
        assert!(!venue.to_string().contains("raw venue"));
    }

    #[tokio::test]
    async fn core_prepare_dispatch_and_sweep_settle_through_the_real_funding_host() {
        if std::env::var_os("GOFIN_PAPERTRADE_CLI").is_none() {
            eprintln!(
                "Skipping public CLI subprocess assertion; run make test-papertrade-rehearsal."
            );
            return;
        }
        let proxy = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
        let hash = format!("0x{}", "42".repeat(32));
        let (base, http_task) = papertrade_http_fixture(proxy.into()).await;
        let credentials = Arc::new(FundingTestCredentials(Mutex::new(BTreeMap::new())));
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let amount = 19_000_000u64;
        let source_fee = 1_000_000u64;
        let sweep_timestamp = Arc::new(std::sync::atomic::AtomicU64::new(nonce / 1_000));
        let recovery_mode =
            std::env::var("GOFIN_PAPERTRADE_RECOVERY_REHEARSAL").unwrap_or_default();
        let recovery_rehearsal = !recovery_mode.is_empty();
        let history_visible = Arc::new(std::sync::atomic::AtomicBool::new(!recovery_rehearsal));
        let core_dispatch_succeeds =
            Arc::new(std::sync::atomic::AtomicBool::new(!recovery_rehearsal));
        let transport = Arc::new(CoreEvidenceTransport {
            proxy: proxy.into(),
            hash: hash.clone(),
            nonce: std::sync::atomic::AtomicU64::new(nonce),
            sweep_timestamp: sweep_timestamp.clone(),
            amount: "19".into(),
            source_fee: "1".into(),
            dispatch_succeeds: Arc::new(std::sync::atomic::AtomicBool::new(
                recovery_mode != "lost_noop",
            )),
            core_dispatch_succeeds: core_dispatch_succeeds.clone(),
            history_visible: history_visible.clone(),
            canonical_error: false,
            empty_ledger: false,
            history_calls: std::sync::atomic::AtomicUsize::new(0),
            dispatch_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut swept_word = [0u8; 32];
        swept_word[16..].copy_from_slice(&(u128::from(amount) * 1_000_000_000_000).to_be_bytes());
        let block_hash = format!("0x{}", "bb".repeat(32));
        let rpc = Arc::new(CoreProofRpc {
            hash: hash.clone(),
            owner: OWNER.into(),
            factory_proxy: proxy.into(),
            block_hash,
            swept_data: format!("0x{}", hex::encode(swept_word)),
            sweep_timestamp,
        });
        let evm = Arc::new(gofin_evm::EvmRuntime::new(rpc, 999, None));
        let papertrade_http =
            Arc::new(crate::papertrade_runtime::PapertradeHttp::test_with_base(&base).unwrap());
        let native = NativeHyperEvmFundingServices::new(
            credentials.clone(),
            evm.clone(),
            OWNER,
            gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount,
            "10000000000000000",
            papertrade_http.clone(),
            1,
            false,
        )
        .unwrap()
        .test_with_hyperliquid_info_url(&format!("{base}/info"));
        let services = Arc::new(
            MultiRouteFundingServices::new(
                native,
                credentials.clone(),
                transport.clone(),
                gofin_hyperliquid::HyperliquidAccount {
                    name: "test-core-owner".into(),
                    owner: OWNER.into(),
                    credential_ref: Some("test-ref".into()),
                    signing_enabled: true,
                    execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
                },
            )
            .unwrap(),
        );
        let host = Arc::new(PapertradeFundingHostAdapter::new(services));
        let account = gofin_papertrade::PapertradeAccount {
            name: "synthetic-papertrade-core".into(),
            owner: OWNER.into(),
            signing_enabled: true,
        };
        let credentials = credentials as Arc<dyn crate::BrokerCredentialProvider>;
        let broker_runtime = Arc::new(
            crate::papertrade_runtime::BrokerPapertradeRuntime::new_with_http(
                account.clone(),
                gofin_credentials::CredentialRef::new("9b91b1f3-4a86-41e3-a825-8dc1c8f4ed32")
                    .unwrap(),
                credentials,
                evm,
                1,
                false,
                Arc::new(CoreTestValuator),
                host.clone(),
                (*papertrade_http).clone(),
            )
            .unwrap(),
        );
        broker_runtime.attach_funding_route_runtime().unwrap();
        let adapter =
            Arc::new(gofin_papertrade::PapertradeAdapter::new(broker_runtime, account).unwrap());
        let mut policy = crate::BrokerPolicy::beta();
        policy.max_movement_usd_micros = "20000000".into();
        policy.max_run_allocation_usd_micros = "50000000".into();
        let store_dir = tempfile::tempdir().unwrap();
        let runtime = crate::compose_runtime_with_clock(
            crate::BrokerConfigV2 {
                version: 2,
                networks: crate::NetworkConfig::reviewed_evm_networks(),
                deployments: vec![],
                connections: vec![],
                policy,
                approver_uid: unsafe { libc::geteuid() },
                extensions: BTreeMap::new(),
            },
            crate::RuntimeDependencies {
                store: gofin_store::Store::open(store_dir.path().join("papertrade-cli.sqlite"))
                    .unwrap(),
                adapters: vec![adapter],
                authorizer: Arc::new(CoreTestApproval),
                valuator: Arc::new(CoreTestValuator),
                bridge: None,
            },
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap();
        runtime.engine.recover_interrupted().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let client = CoreCliClient::new(runtime, socket_dir.path()).await;
        let source_resource = ResourceId(format!("hyperliquid:mainnet:{OWNER}:spot:primary"));
        let source_asset = AssetId {
            network: "hyperliquid:mainnet".into(),
            reference: "USDC".into(),
            decimals: 36,
        };
        let principal_atoms = Quantity::new(format!("{}{}", amount, "0".repeat(30))).unwrap();
        let fee_atoms = Quantity::new(format!("{}{}", source_fee, "0".repeat(30))).unwrap();
        let max_debits = vec![ResourceAmount {
            resource: source_resource.clone(),
            amount: Amount {
                asset: source_asset.clone(),
                atoms: principal_atoms,
            },
        }];
        let max_costs = vec![CostBound {
            location: gofin_domain::AccountingLocation::Cash {
                resource: source_resource.clone(),
            },
            amount: Amount {
                asset: source_asset.clone(),
                atoms: fee_atoms,
            },
            treatment: CostTreatment::AdditionalDebit,
        }];
        let plan = FinancialPlanSpec {
            version: 1,
            name: "Deposit 19 USDC from Core into Papertrade".into(),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "deposit".into(),
                operation: "papertrade.deposit".into(),
                input: json!({"route":"core","amount":"19"}),
                permitted_variations: json!({
                    "mode":"exact",
                    "maximum_debits":max_debits,
                    "maximum_costs":max_costs,
                    "possible_credits":[{
                        "resource":gofin_papertrade::PapertradeAccount {
                            name:"synthetic-papertrade-core".into(),owner:OWNER.into(),signing_enabled:true
                        }.trading_cash_resource(),
                        "asset":gofin_papertrade::trading_usdc_asset()
                    }]
                }),
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: None,
                },
                funding_dependencies: vec![],
                action_deadline_seconds: None,
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: max_debits,
                maximum_costs: max_costs,
            },
        };
        let mut authorization = FinancialPlanAuthorizationRequest {
            request_id: "papertrade-core-cli-plan".into(),
            plan,
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let preview = client
            .call(BrokerRequest::PreviewPlan {
                request: authorization.clone(),
            })
            .await;
        assert!(preview.ok, "named-plan preview: {:?}", preview.error);
        authorization.expected_review_hash = Some(
            preview.result.as_ref().unwrap()["review_hash"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        let approved = client
            .call(BrokerRequest::AuthorizePlan {
                request: authorization,
            })
            .await;
        assert!(
            approved.ok,
            "named-plan authorization: {:?}",
            approved.error
        );
        let plan_id = approved.result.unwrap()["plan_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let preview_step = client
            .call(BrokerRequest::PreviewPlanStep {
                plan_id: plan_id.clone(),
                step_id: "deposit".into(),
                operation: "papertrade.deposit".into(),
                input: json!({"route":"core","amount":"19"}),
            })
            .await;
        assert!(
            preview_step.ok,
            "CLI step preview: {:?}",
            preview_step.error
        );
        assert!(
            preview_step.result.unwrap()["summary"]
                .as_str()
                .is_some_and(|s| s.contains("19 USDC"))
        );
        let request = gofin_domain::OperationRequest {
            request_id: "papertrade-core-cli-deposit".into(),
            operation: "papertrade.deposit".into(),
            input: json!({"route":"core","amount":"19"}),
            run_id: None,
        };
        let mut settled = client
            .call(BrokerRequest::ExecutePlanStep {
                plan_id: plan_id.clone(),
                step_id: "deposit".into(),
                request,
            })
            .await;
        assert!(settled.ok, "CLI Core deposit: {:?}", settled.error);
        if recovery_rehearsal {
            assert!(
                settled.result.as_ref().is_some_and(|result| matches!(
                    result["status"].as_str(),
                    Some("pending" | "unknown")
                )),
                "lost Core response must remain unresolved: {:?}",
                settled.result
            );
            let operation_id = settled.result.as_ref().unwrap()["id"]
                .as_str()
                .expect("unresolved execution has durable operation id")
                .to_owned();
            let still_unknown = client
                .call(BrokerRequest::Status {
                    id: operation_id.clone(),
                })
                .await;
            assert!(
                still_unknown.ok,
                "status recovery: {:?}",
                still_unknown.error
            );
            assert!(matches!(
                still_unknown.result.as_ref().unwrap()["status"].as_str(),
                Some("pending" | "unknown")
            ));
            assert_eq!(
                transport
                    .dispatch_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                1,
                "status must not replay the original Core send"
            );

            let child_request = gofin_domain::OperationRequest {
                request_id: "8b891c8d-3d55-4dd8-969d-476b2411e928".into(),
                operation: "papertrade.deposit.invalidate_nonce".into(),
                input: json!({"parent_operation_id":operation_id}),
                run_id: None,
            };
            let child = client
                .call(BrokerRequest::Execute {
                    request: child_request,
                })
                .await;
            assert!(child.ok, "CLI Noop recovery child: {:?}", child.error);
            assert_eq!(
                transport
                    .dispatch_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                2,
                "exactly one Noop follows the original Core dispatch"
            );
            let child_id = child.result.as_ref().unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(
                child.result.as_ref().unwrap()["input"]
                    .as_object()
                    .unwrap()
                    .len(),
                1,
                "public recovery input must contain only the parent operation ID"
            );
            if recovery_mode == "lost_noop" {
                assert!(
                    matches!(
                        child.result.as_ref().unwrap()["status"].as_str(),
                        Some("pending" | "unknown")
                    ),
                    "lost Noop response remains unresolved: {:?}",
                    child.result
                );
                let child_status = client.call(BrokerRequest::Status { id: child_id }).await;
                assert!(child_status.ok);
                assert!(matches!(
                    child_status.result.as_ref().unwrap()["status"].as_str(),
                    Some("pending" | "unknown")
                ));
                assert_eq!(
                    transport
                        .dispatch_calls
                        .load(std::sync::atomic::Ordering::SeqCst),
                    2,
                    "lost Noop status must not resend"
                );
                let second_child = client
                    .call(BrokerRequest::Execute {
                        request: gofin_domain::OperationRequest {
                            request_id: "b8309d17-3d21-4ca9-b9b9-487deff1f5e4".into(),
                            operation: "papertrade.deposit.invalidate_nonce".into(),
                            input: json!({"parent_operation_id":operation_id}),
                            run_id: None,
                        },
                    })
                    .await;
                assert!(
                    !second_child.ok
                        || second_child
                            .result
                            .as_ref()
                            .is_some_and(|result| result["status"] == "failed"),
                    "second child must be blocked after the first Noop attempt: {:?}",
                    second_child.result
                );
                assert_eq!(
                    transport
                        .dispatch_calls
                        .load(std::sync::atomic::Ordering::SeqCst),
                    2,
                    "a second child must not submit another Noop"
                );
                http_task.abort();
                return;
            }
            assert_eq!(
                child.result.as_ref().unwrap()["status"],
                "completed",
                "exact Noop acknowledgement should complete the linked child: {:?}",
                child.result
            );
            let child_replay = client.call(BrokerRequest::Status { id: child_id }).await;
            assert!(child_replay.ok);
            assert_eq!(child_replay.result.as_ref().unwrap()["status"], "completed");
            assert_eq!(
                transport
                    .dispatch_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                2,
                "child recovery status must not submit a second Noop"
            );

            let failed_parent = client
                .call(BrokerRequest::Status { id: operation_id })
                .await;
            assert!(
                failed_parent.ok,
                "parent terminalization: {:?}",
                failed_parent.error
            );
            assert_eq!(
                failed_parent.result.as_ref().unwrap()["status"],
                "failed",
                "only exact linked Noop proof releases the original parent: {:?}",
                failed_parent.result
            );
            assert_eq!(
                failed_parent.result.as_ref().unwrap()["effects"]["effects"],
                json!([])
            );

            core_dispatch_succeeds.store(true, std::sync::atomic::Ordering::SeqCst);
            history_visible.store(true, std::sync::atomic::Ordering::SeqCst);
            let retry = client
                .call(BrokerRequest::ExecutePlanStep {
                    plan_id: plan_id.clone(),
                    step_id: "deposit".into(),
                    request: gofin_domain::OperationRequest {
                        request_id: "papertrade-core-cli-deposit-retry".into(),
                        operation: "papertrade.deposit".into(),
                        input: json!({"route":"core","amount":"19"}),
                        run_id: None,
                    },
                })
                .await;
            assert!(
                retry.ok,
                "fresh same-plan attempt after proven release: {:?}",
                retry.error
            );
            assert_eq!(
                retry.result.as_ref().unwrap()["status"],
                "completed",
                "fresh attempt should use its own nonce and settle: {:?}",
                retry.result
            );
            assert_eq!(
                transport
                    .dispatch_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                3,
                "only the fresh Core attempt may dispatch after recovery"
            );
            http_task.abort();
            return;
        }
        if settled
            .result
            .as_ref()
            .is_some_and(|result| result["status"] == "pending")
        {
            let operation_id = settled.result.as_ref().unwrap()["id"]
                .as_str()
                .expect("pending execution has durable operation id")
                .to_owned();
            // The controlled transport accepts the Core send before the canonical
            // Exchange sweep becomes observable. Reconcile via Status; never resend.
            settled = client
                .call(BrokerRequest::Status { id: operation_id })
                .await;
        }
        assert_eq!(
            settled.result.as_ref().unwrap()["status"],
            "completed",
            "dispatches={}, history_calls={}, result: {:?}",
            transport
                .dispatch_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            transport
                .history_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            settled.result
        );
        let effects = settled.result.as_ref().unwrap()["effects"]["effects"]
            .as_array()
            .unwrap();
        assert!(effects.iter().any(|effect| {
            effect["kind"] == "cash_debit"
                && effect["resource"] == source_resource.0
                && effect["amount"]["atoms"] == format!("{}{}", amount, "0".repeat(30))
        }));
        assert!(
            effects.iter().any(|effect| {
                effect["kind"] == "cost"
                    && effect["amount"]["atoms"] == format!("{}{}", source_fee, "0".repeat(30))
                    && effect["amount"]["asset"]["network"] == "hyperliquid:mainnet"
            }),
            "exact additional 1 USDC Core fee missing: {effects:?}"
        );
        assert!(
            effects.iter().any(|effect| effect["kind"] == "settlement"
                && effect["delta"] == "19000000000000000000")
        );
        assert_eq!(
            transport
                .dispatch_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let replay = client
            .call(BrokerRequest::ExecutePlanStep {
                plan_id,
                step_id: "deposit".into(),
                request: gofin_domain::OperationRequest {
                    request_id: "papertrade-core-cli-deposit".into(),
                    operation: "papertrade.deposit".into(),
                    input: json!({"route":"core","amount":"19"}),
                    run_id: None,
                },
            })
            .await;
        assert!(replay.ok, "CLI recovery: {:?}", replay.error);
        assert_eq!(
            replay.result.as_ref().unwrap()["status"],
            "completed",
            "idempotent same-request replay: {:?}",
            replay.result
        );
        assert_eq!(
            transport
                .dispatch_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "recovery must not resubmit"
        );
        http_task.abort();
    }

    #[tokio::test]
    #[ignore = "requires a built dev CLI; run make test-papertrade-rehearsal"]
    async fn core_withdrawal_named_plan_uses_real_runtime_and_canonical_credit_proof() {
        run_withdrawal_named_plan("core", "core").await;
    }

    #[tokio::test]
    #[ignore = "requires a built dev CLI; run make test-papertrade-rehearsal"]
    async fn hyperevm_withdrawal_named_plan_settles_core_stage_without_dispatching_onward_leg() {
        run_withdrawal_named_plan("hyperevm", "hyperevm").await;
    }

    async fn run_withdrawal_named_plan(route: &'static str, route_id: &'static str) {
        if std::env::var_os("GOFIN_PAPERTRADE_CLI").is_none() {
            panic!("required Papertrade withdrawal CLI rehearsal needs GOFIN_PAPERTRADE_CLI");
        }
        use gofin_domain::execution_state::ExecutionState;
        use gofin_domain::financial_plans::{
            FinancialPlanAuthorizationRequest, FinancialPlanBounds, FinancialPlanSpec,
            FinancialPlanStepSpec, FinancialStepCompletion,
        };
        use gofin_domain::{Amount, AssetId, Quantity, ResourceAmount, ResourceId};
        use gofin_papertrade::PapertradeRuntime;
        use gofin_protocol::BrokerRequest;
        use std::sync::atomic::{AtomicU64, Ordering};

        const OWNER: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
        const PROXY: &str = "0x18b9c4b188f556242edbfd5496d0840130f3ebb0";
        let amount = 19_000_000u64;
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let ledger_time_ms = now.saturating_mul(1_000);
        let chain = Arc::new(Mutex::new(None));
        let credentials = Arc::new(FundingTestCredentials(Mutex::new(BTreeMap::new())));
        let (base, http_task) = core_withdrawal_http_fixture(
            credentials.clone(),
            chain.clone(),
            OWNER.into(),
            PROXY.into(),
            amount,
            now.saturating_sub(1),
        )
        .await;
        let hyperevm_send_submitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hyperevm_send_nonce = Arc::new(AtomicU64::new(0));
        let hyperevm_send_dispatch_count = Arc::new(AtomicU64::new(0));
        let hyperevm_send_time_ms = Arc::new(AtomicU64::new(0));
        let rpc = Arc::new(CoreWithdrawalRpc {
            fixture: chain.clone(),
            hyperevm_send_submitted: hyperevm_send_submitted.clone(),
            hyperevm_send_time_ms: hyperevm_send_time_ms.clone(),
        });
        let evm = Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), 999, None));
        let http =
            Arc::new(crate::papertrade_runtime::PapertradeHttp::test_with_base(&base).unwrap());
        let hyperliquid_account = gofin_hyperliquid::HyperliquidAccount {
            name: "core-withdrawal-fixture".into(),
            owner: OWNER.into(),
            credential_ref: Some("fixture-owner".into()),
            signing_enabled: true,
            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
        };
        let transport = Arc::new(CoreWithdrawalTransport {
            owner: OWNER.into(),
            amount: "19.0".into(),
            hash: format!("0x{}", "99".repeat(32)),
            time_ms: Arc::new(AtomicU64::new(ledger_time_ms)),
            hyperevm_send_time_ms: hyperevm_send_time_ms.clone(),
            hyperevm_send_submitted: hyperevm_send_submitted.clone(),
            hyperevm_send_nonce: hyperevm_send_nonce.clone(),
            hyperevm_send_dispatch_count: hyperevm_send_dispatch_count.clone(),
        });
        let native = NativeHyperEvmFundingServices::new(
            credentials.clone(),
            evm.clone(),
            OWNER,
            gofin_hyperliquid::funding::HyperliquidAccountMode::UnifiedAccount,
            "10000000000000000",
            http.clone(),
            1,
            false,
        )
        .unwrap()
        .test_with_hyperliquid_info_url(&format!("{base}/info"));
        let cctp_provider = Arc::new(
            gofin_bridge::cctp::CctpProvider::new(
                BTreeMap::new(),
                gofin_bridge::cctp::CctpConfig::reviewed(),
            )
            .unwrap(),
        );
        let cctp_verifier = Arc::new(
            crate::papertrade_cctp_settlement::PapertradeCctpSettlementVerifier::new(cctp_provider),
        );
        let inbound = Arc::new(
            crate::papertrade_funding_inbound::PapertradeInboundCctpSource::new(
                BTreeMap::new(),
                BTreeMap::new(),
                None,
                None,
                BTreeMap::new(),
            )
            .unwrap(),
        );
        let routes = Arc::new(
            crate::papertrade_funding_routes::PapertradeFundingRoutes::new(
                credentials.clone(),
                gofin_credentials::CredentialRef::new("00000000-0000-4000-8000-000000000001")
                    .unwrap(),
                OWNER,
                BTreeMap::from([(999, evm.clone())]),
                transport.clone(),
                hyperliquid_account.clone(),
                cctp_verifier,
                None,
                inbound,
                None,
                http.clone(),
                BTreeMap::from([(999, 1)]),
            )
            .unwrap(),
        );
        let services = Arc::new(
            MultiRouteFundingServices::new(
                native,
                credentials.clone(),
                transport,
                hyperliquid_account,
            )
            .unwrap()
            .with_other_routes(routes),
        );
        let host = Arc::new(PapertradeFundingHostAdapter::new(services));
        let account = gofin_papertrade::PapertradeAccount {
            name: "papertrade-core-withdrawal-fixture".into(),
            owner: OWNER.into(),
            signing_enabled: true,
        };
        let credentials_dyn: Arc<dyn crate::BrokerCredentialProvider> = credentials.clone();
        let broker_runtime = Arc::new(
            crate::papertrade_runtime::BrokerPapertradeRuntime::new_with_http(
                account.clone(),
                gofin_credentials::CredentialRef::new("00000000-0000-4000-8000-000000000001")
                    .unwrap(),
                credentials_dyn,
                evm,
                1,
                false,
                Arc::new(CoreTestValuator),
                host.clone(),
                (*http).clone(),
            )
            .unwrap(),
        );
        broker_runtime.attach_funding_route_runtime().unwrap();
        struct NonceState(AtomicU64);
        impl ExecutionState for NonceState {
            fn allocate_nonce(&self, _: &str, _: u64) -> gofin_domain::Result<u64> {
                Ok(self.0.fetch_add(1, Ordering::SeqCst) + 1)
            }
            fn operation(
                &self,
                _: &str,
            ) -> gofin_domain::Result<Option<gofin_domain::OperationRecord>> {
                Ok(None)
            }
        }
        broker_runtime
            .attach_execution_state(Arc::new(NonceState(AtomicU64::new(0))))
            .unwrap();
        let adapter = Arc::new(
            gofin_papertrade::PapertradeAdapter::new(broker_runtime.clone(), account.clone())
                .unwrap(),
        );
        let paper_cash = account.trading_cash_resource();
        let core_resource = ResourceId(format!("hyperliquid:mainnet:{OWNER}:spot:primary"));
        let cash_asset = gofin_papertrade::trading_usdc_asset();
        let core_asset = AssetId {
            network: "hyperliquid:mainnet".into(),
            reference: "USDC".into(),
            decimals: 36,
        };
        let paper_wad = Quantity::new(format!("{}{}", amount, "0".repeat(12))).unwrap();
        let core_wad = Quantity::new(format!("{}{}", amount, "0".repeat(30))).unwrap();
        let debits = vec![ResourceAmount {
            resource: paper_cash.clone(),
            amount: Amount {
                asset: cash_asset.clone(),
                atoms: paper_wad.clone(),
            },
        }];
        let plan = FinancialPlanSpec {
            version: 1,
            name: format!("Withdraw 19 USDC from Papertrade to {route_id}"),
            summary: None,
            steps: vec![FinancialPlanStepSpec {
                step_id: "withdraw".into(),
                operation: "papertrade.withdraw".into(),
                input: json!({"route":route,"amount":"19"}),
                permitted_variations: json!({"mode":"exact","maximum_debits":debits,"maximum_costs":[],"possible_credits":[{"resource":core_resource,"asset":core_asset}]}),
                completion: FinancialStepCompletion {
                    operation_succeeded: true,
                    principal_spend: None,
                    minimum_net_receipt: None,
                },
                funding_dependencies: vec![],
                action_deadline_seconds: None,
            }],
            aggregate_bounds: FinancialPlanBounds {
                maximum_debits: debits,
                maximum_costs: vec![],
            },
        };
        let mut authorization = FinancialPlanAuthorizationRequest {
            request_id: format!("papertrade-{route_id}-withdrawal-plan"),
            plan,
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let store_dir = tempfile::tempdir().unwrap();
        let runtime = crate::compose_runtime_with_clock(
            crate::BrokerConfigV2 {
                version: 2,
                networks: crate::NetworkConfig::reviewed_evm_networks(),
                deployments: vec![],
                connections: vec![],
                policy: {
                    let mut policy = crate::BrokerPolicy::beta();
                    policy.max_movement_usd_micros = "20000000".into();
                    policy.max_run_allocation_usd_micros = "50000000".into();
                    policy
                },
                approver_uid: unsafe { libc::geteuid() },
                extensions: BTreeMap::new(),
            },
            crate::RuntimeDependencies {
                store: gofin_store::Store::open(
                    store_dir
                        .path()
                        .join(format!("papertrade-{route_id}-withdrawal.sqlite")),
                )
                .unwrap(),
                adapters: vec![adapter],
                authorizer: Arc::new(CoreTestApproval),
                valuator: Arc::new(CoreTestValuator),
                bridge: None,
            },
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap();
        runtime.engine.recover_interrupted().unwrap();
        let socket_dir = tempfile::tempdir().unwrap();
        let client = CoreCliClient::new(runtime, socket_dir.path()).await;
        let preview = client
            .call(BrokerRequest::PreviewPlan {
                request: authorization.clone(),
            })
            .await;
        assert!(preview.ok, "withdrawal plan preview: {:?}", preview.error);
        authorization.expected_review_hash = Some(
            preview.result.as_ref().unwrap()["review_hash"]
                .as_str()
                .unwrap()
                .into(),
        );
        let approved = client
            .call(BrokerRequest::AuthorizePlan {
                request: authorization,
            })
            .await;
        assert!(
            approved.ok,
            "withdrawal plan authorization: {:?}",
            approved.error
        );
        let plan_id = approved.result.unwrap()["plan_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let preview_step = client
            .call(BrokerRequest::PreviewPlanStep {
                plan_id: plan_id.clone(),
                step_id: "withdraw".into(),
                operation: "papertrade.withdraw".into(),
                input: json!({"route":route,"amount":"19"}),
            })
            .await;
        assert!(
            preview_step.ok,
            "withdrawal step preview: {:?}",
            preview_step.error
        );
        assert!(
            preview_step.result.as_ref().unwrap()["summary"]
                .as_str()
                .is_some_and(|value| value.contains("19 USDC"))
        );
        let result = client
            .call(BrokerRequest::ExecutePlanStep {
                plan_id,
                step_id: "withdraw".into(),
                request: gofin_domain::OperationRequest {
                    request_id: format!("papertrade-{route_id}-withdrawal-run"),
                    operation: "papertrade.withdraw".into(),
                    input: json!({"route":route,"amount":"19"}),
                    run_id: None,
                },
            })
            .await;
        assert!(
            result.ok,
            "withdrawal CLI execution: {:?} / {:?}",
            result.error, result.result
        );
        assert_eq!(
            result.result.as_ref().unwrap()["status"],
            "completed",
            "settlement must prove the exact stage-one Core credit: {:?}",
            result.result
        );
        let effects = result.result.as_ref().unwrap()["effects"]["effects"]
            .as_array()
            .unwrap();
        assert!(effects.iter().any(|effect| effect["kind"] == "cash_debit"
            && effect["resource"] == paper_cash.0
            && effect["amount"]["atoms"] == paper_wad.to_string()));
        assert!(
            effects
                .iter()
                .any(|effect| effect["kind"] == "usable_credit"
                    && effect["to"] == core_resource.0
                    && effect["amount"]["atoms"] == core_wad.to_string()),
            "Core credit missing: {effects:?}"
        );
        let replay = client
            .call(BrokerRequest::Status {
                id: result.result.as_ref().unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .into(),
            })
            .await;
        assert!(replay.ok);
        assert_eq!(replay.result.as_ref().unwrap()["status"], "completed");
        if route == "hyperevm" {
            use gofin_domain::{AccountingLocation, CostBound, CostTreatment};
            let protected_parent = credentials
                .0
                .lock()
                .unwrap()
                .values()
                .find_map(|value| serde_json::from_value::<BoundFunding>(value.clone()).ok())
                .filter(|plan| {
                    plan.intent.route == FundingRoute::Hyperevm
                        && plan.intent.direction == FundingDirection::Withdraw
                })
                .expect("stage-one route plan is protected before continuation");
            let route_id = protected_parent.operation_id.clone();
            let source_resource = ResourceId(format!("hyperliquid:mainnet:{OWNER}:spot:primary"));
            let source_asset = AssetId {
                network: "hyperliquid:mainnet".into(),
                reference: "USDC".into(),
                decimals: 36,
            };
            let source_principal = protected_parent
                .intent
                .amount_usdc_atoms
                .checked_sub(crate::papertrade_funding::CORE_WITHDRAWAL_RESERVE_ATOMS)
                .expect("HyperEVM withdrawal retains the fixed Core reserve");
            let source_debit =
                Quantity::new(format!("{}{}", source_principal, "0".repeat(30))).unwrap();
            let source_fee = Quantity::new(format!(
                "{}{}",
                protected_parent.bounds.maximum_source_fee_usdc_atoms,
                "0".repeat(30)
            ))
            .unwrap();
            let native_fee_atoms = protected_parent
                .bounds
                .maximum_native_fee_atoms
                .as_deref()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            let native_fee =
                Quantity::new(format!("{}{}", native_fee_atoms, "0".repeat(28))).unwrap();
            let native_asset = AssetId {
                network: "hyperliquid:mainnet".into(),
                reference: "HYPE".into(),
                decimals: 36,
            };
            let destination = ResourceId(protected_parent.destination_identity.clone());
            let destination_asset = AssetId {
                network: "ethereum:999".into(),
                reference: gofin_bridge::cctp::hyperevm::NATIVE_USDC.to_ascii_lowercase(),
                decimals: 6,
            };
            let continuation_plan = FinancialPlanSpec {
                version: 1,
                name: "Continue Papertrade withdrawal to HyperEVM".into(),
                summary: None,
                steps: vec![FinancialPlanStepSpec {
                    step_id: "continue".into(),
                    operation: "papertrade.funding_continue".into(),
                    input: json!({"route_id":route_id}),
                    permitted_variations: json!({
                        "mode":"exact",
                        "maximum_debits":[{"resource":source_resource,"amount":{"asset":source_asset,"atoms":source_debit}}],
                        "maximum_costs":[
                            {"location":{"kind":"cash","resource":source_resource},"amount":{"asset":source_asset,"atoms":source_fee},"treatment":"additional_debit"},
                            {"location":{"kind":"cash","resource":source_resource},"amount":{"asset":native_asset,"atoms":native_fee},"treatment":"additional_debit"}
                        ],
                        "possible_credits":[{"resource":destination,"asset":destination_asset}]
                    }),
                    completion: FinancialStepCompletion {
                        operation_succeeded: true,
                        principal_spend: None,
                        minimum_net_receipt: None,
                    },
                    funding_dependencies: vec![],
                    action_deadline_seconds: None,
                }],
                aggregate_bounds: FinancialPlanBounds {
                    maximum_debits: vec![ResourceAmount {
                        resource: source_resource.clone(),
                        amount: Amount {
                            asset: source_asset.clone(),
                            atoms: source_debit.clone(),
                        },
                    }],
                    maximum_costs: vec![
                        CostBound {
                            location: AccountingLocation::Cash {
                                resource: source_resource.clone(),
                            },
                            amount: Amount {
                                asset: source_asset,
                                atoms: source_fee,
                            },
                            treatment: CostTreatment::AdditionalDebit,
                        },
                        CostBound {
                            location: AccountingLocation::Cash {
                                resource: source_resource.clone(),
                            },
                            amount: Amount {
                                asset: native_asset,
                                atoms: native_fee,
                            },
                            treatment: CostTreatment::AdditionalDebit,
                        },
                    ],
                },
            };
            let mut continuation_authorization = FinancialPlanAuthorizationRequest {
                request_id: "papertrade-hyperevm-withdrawal-continuation-plan".into(),
                plan: continuation_plan,
                valid_for_seconds: Some(600),
                replaces_revision_id: None,
                expected_review_hash: None,
            };
            let preview = client
                .call(BrokerRequest::PreviewPlan {
                    request: continuation_authorization.clone(),
                })
                .await;
            assert!(
                preview.ok,
                "continuation plan preview: {:?} / {:?}",
                preview.error, preview.result
            );
            continuation_authorization.expected_review_hash = Some(
                preview.result.as_ref().unwrap()["review_hash"]
                    .as_str()
                    .unwrap()
                    .into(),
            );
            let authorized = client
                .call(BrokerRequest::AuthorizePlan {
                    request: continuation_authorization,
                })
                .await;
            assert!(
                authorized.ok,
                "continuation plan authorization: {:?}",
                authorized.error
            );
            let continuation_plan_id = authorized.result.unwrap()["plan_id"]
                .as_str()
                .unwrap()
                .to_owned();
            let step_preview = client
                .call(BrokerRequest::PreviewPlanStep {
                    plan_id: continuation_plan_id.clone(),
                    step_id: "continue".into(),
                    operation: "papertrade.funding_continue".into(),
                    input: json!({"route_id":route_id}),
                })
                .await;
            assert!(
                step_preview.ok,
                "continuation step preview: {:?} / {:?}",
                step_preview.error, step_preview.result
            );
            let continuation = client
                .call(BrokerRequest::ExecutePlanStep {
                    plan_id: continuation_plan_id,
                    step_id: "continue".into(),
                    request: gofin_domain::OperationRequest {
                        request_id: "papertrade-hyperevm-withdrawal-continuation-run".into(),
                        operation: "papertrade.funding_continue".into(),
                        input: json!({"route_id":route_id}),
                        run_id: None,
                    },
                })
                .await;
            assert!(
                continuation.ok,
                "continuation CLI execution: {:?} / {:?}",
                continuation.error, continuation.result
            );
            if continuation.result.as_ref().unwrap()["status"] != "completed" {
                let records = credentials.0.lock().unwrap();
                eprintln!(
                    "funding fixture proof records: {:#?}",
                    records
                        .iter()
                        .filter(|(key, _)| key.contains("hyperevm_")
                            || key.contains("dispatch_attempted")
                            || key.contains("dispatch_response"))
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(
                continuation.result.as_ref().unwrap()["status"],
                "completed",
                "continuation must prove final HyperEVM receipt: {:?}",
                continuation.result
            );
            assert!(hyperevm_send_submitted.load(std::sync::atomic::Ordering::SeqCst));
            let continuation_effects = continuation.result.as_ref().unwrap()["effects"]["effects"]
                .as_array()
                .unwrap();
            assert!(
                continuation_effects.iter().any(|effect| {
                    effect["kind"] == "cash_debit"
                        && effect["resource"] == source_resource.0
                        && effect["amount"]["atoms"] == source_debit.to_string()
                }),
                "unexpected continuation effects: {continuation_effects:#?}"
            );
            assert!(continuation_effects.iter().any(|effect| {
                effect["kind"] == "usable_credit"
                    && effect["to"] == destination.0
                    && effect["amount"]["atoms"] == "18900000"
            }));
            let replay = client
                .call(BrokerRequest::Status {
                    id: continuation.result.as_ref().unwrap()["id"]
                        .as_str()
                        .unwrap()
                        .into(),
                })
                .await;
            assert!(replay.ok);
            assert_eq!(replay.result.as_ref().unwrap()["status"], "completed");
            assert_eq!(
                hyperevm_send_dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "continuation status/replay must not submit the CoreWriter action again"
            );
        }
        http_task.abort();
    }

    struct CoreTestApproval;

    #[async_trait]
    impl Authorizer for CoreTestApproval {
        async fn authorize(
            &self,
            challenge: &AuthorizationChallenge,
        ) -> gofin_domain::Result<ApprovalAudit> {
            Ok(ApprovalAudit {
                source: None,
                yolo_provenance: None,
                mechanism: "synthetic Papertrade Core CLI approval".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: chrono::Utc::now().to_rfc3339(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    struct CoreTestValuator;

    #[async_trait]
    impl crate::AssetValuator for CoreTestValuator {
        async fn usd_micros(
            &self,
            amount: &Amount,
        ) -> gofin_domain::Result<(String, String, String)> {
            use num_bigint::BigUint;
            let atoms = BigUint::parse_bytes(amount.atoms.to_string().as_bytes(), 10)
                .ok_or_else(|| GoFinError::validation("test valuation amount is malformed"))?;
            let scale = BigUint::from(10u8).pow(amount.asset.decimals.into());
            let micros = atoms * BigUint::from(1_000_000u64) / scale;
            Ok((
                micros.to_str_radix(10),
                chrono::Utc::now().to_rfc3339(),
                "synthetic exact unit-price fixture".into(),
            ))
        }
    }

    struct CoreCliClient {
        path: std::path::PathBuf,
        server: tokio::task::JoinHandle<()>,
    }

    impl CoreCliClient {
        async fn new(runtime: Arc<crate::BrokerRuntime>, dir: &std::path::Path) -> Self {
            let path = dir.join("papertrade-core-cli.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let server = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let runtime = runtime.clone();
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(stream);
                        let mut line = String::new();
                        reader.read_line(&mut line).await.unwrap();
                        let hello: Value = serde_json::from_str(&line).unwrap();
                        let identity = "synthetic-papertrade-workflow";
                        let build_id = "synthetic-papertrade-workflow-build";
                        assert_eq!(hello["identity"], identity);
                        assert_eq!(hello["build_id"], build_id);
                        let reply = json!({"kind":"hello","identity":identity,"build_id":build_id,"mode":"synthetic"});
                        reader
                            .get_mut()
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .unwrap();
                        line.clear();
                        reader.read_line(&mut line).await.unwrap();
                        let envelope: Value = serde_json::from_str(&line).unwrap();
                        let request: BrokerRequest =
                            serde_json::from_value(envelope["request"].clone()).unwrap();
                        let response = gofin_protocol::progress::observe(
                            reader.get_mut(),
                            envelope["progress"].as_bool().unwrap_or(false),
                            runtime.handle(request),
                        )
                        .await;
                        let reply = json!({"identity":identity,"build_id":build_id,"mode":"synthetic","response":response});
                        reader
                            .get_mut()
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .unwrap();
                    });
                }
            });
            Self { path, server }
        }

        async fn call(&self, request: BrokerRequest) -> gofin_protocol::BrokerResponse {
            let binary = std::path::PathBuf::from(std::env::var("GOFIN_PAPERTRADE_CLI").unwrap())
                .canonicalize()
                .expect("GOFIN_PAPERTRADE_CLI must name the built development CLI");
            let encode = |value: &Value| serde_json::to_string(value).unwrap();
            let mut args = vec!["--json".to_owned()];
            match request {
                BrokerRequest::PreviewPlan { request } => {
                    args.extend([
                        "plans".into(),
                        "preview".into(),
                        "--input".into(),
                        encode(&json!(request.plan)),
                    ]);
                    if let Some(seconds) = request.valid_for_seconds {
                        args.extend(["--valid-for".into(), format!("{seconds}s")]);
                    }
                }
                BrokerRequest::AuthorizePlan { request } => {
                    args.extend([
                        "plans".into(),
                        "authorize".into(),
                        "--input".into(),
                        encode(&json!(request.plan)),
                        "--id".into(),
                        request.request_id,
                    ]);
                    if let Some(seconds) = request.valid_for_seconds {
                        args.extend(["--valid-for".into(), format!("{seconds}s")]);
                    }
                    if let Some(hash) = request.expected_review_hash {
                        args.extend(["--review-hash".into(), hash]);
                    }
                }
                BrokerRequest::PreviewPlanStep {
                    plan_id,
                    step_id,
                    operation,
                    input,
                } => {
                    args.extend([
                        "execute".into(),
                        operation,
                        "--input".into(),
                        encode(&input),
                        "--plan".into(),
                        plan_id,
                        "--step".into(),
                        step_id,
                        "--dry-run".into(),
                    ]);
                }
                BrokerRequest::ExecutePlanStep {
                    plan_id,
                    step_id,
                    request,
                } => {
                    args.extend([
                        "execute".into(),
                        request.operation,
                        "--input".into(),
                        encode(&request.input),
                        "--id".into(),
                        request.request_id,
                        "--plan".into(),
                        plan_id,
                        "--step".into(),
                        step_id,
                    ]);
                }
                BrokerRequest::PlanStatus { plan_id } => {
                    args.extend(["plans".into(), "status".into(), plan_id])
                }
                BrokerRequest::Status { id } => args.extend(["status".into(), id]),
                BrokerRequest::Execute { request } => {
                    args.extend([
                        "execute".into(),
                        request.operation,
                        "--input".into(),
                        encode(&request.input),
                        "--id".into(),
                        request.request_id,
                    ]);
                }
                other => panic!("CLI workflow mapping is not defined for {other:?}"),
            }
            let socket = self.path.clone();
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(binary)
                    .args([
                        "--identity",
                        "synthetic-papertrade-workflow",
                        "--build-id",
                        "synthetic-papertrade-workflow-build",
                    ])
                    .args(args)
                    .env("GOFIN_DEV_IDENTITY", "synthetic-papertrade-workflow")
                    .env("GOFIN_DEV_BUILD_ID", "synthetic-papertrade-workflow-build")
                    .env("GOFIN_DEV_SOCKET", socket)
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(
                output.stderr.is_empty(),
                "machine CLI must keep stderr quiet: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            if output.status.success() {
                gofin_protocol::BrokerResponse::success(value)
            } else {
                gofin_protocol::BrokerResponse::failure(
                    serde_json::from_value(value["error"].clone()).unwrap_or_else(|_| {
                        GoFinError::validation("Papertrade CLI returned an unstructured error.")
                    }),
                )
            }
        }
    }

    impl Drop for CoreCliClient {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
