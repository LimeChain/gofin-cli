//! Broker-owned Papertrade funding route composition.
//!
//! This module fixes route topology and submission sequencing. The host owns
//! credentials, RPCs, journal storage and venue-specific evidence retrieval.
//! Every write is a closed action in `FundingAction`; no caller-provided
//! endpoint, calldata, recipient or transfer hook is accepted.

use gofin_bridge::{EvmCall, cctp};
use gofin_domain::GoFinError;
use gofin_hyperliquid::funding::{
    CctpWithdrawalTarget, CoreToPapertradeAction, HyperliquidAccountMode, SendToEvmWithDataAction,
    cctp_withdrawal_action, core_spot_deposit_action, same_owner_hyperevm_send_asset_action,
};
use gofin_papertrade::{
    FundingDirection, FundingIntent, FundingProgress, FundingProgressStatus, FundingReceipt,
    FundingReceiptStage, FundingRoute, validate_funding,
};
use gofin_solana::transaction::NormalizedInstruction;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
const CORE_WITHDRAWAL_TOPIC: &str =
    "0x95d1a93901f24eeaa294588802f66fa008f9f3741060c9f79e5dab4709de5e75";
const SWEPT_TOPIC: &str = "0xc36b5179cb9c303b200074996eab2b3473eac370fdd7eba3bec636fe35109696";
/// Papertrade's proxy sweep minimum, expressed in six-decimal USDC atoms.
const MIN_PAPERTRADE_FUNDING_USDC_ATOMS: u64 = 10_000_000;
/// HyperCore keeps this portion of a Papertrade withdrawal available to pay
/// the CoreWriter send fee. It is a bound on the fee reserve, not a second
/// destination credit.
pub(crate) const CORE_WITHDRAWAL_RESERVE_ATOMS: u64 = 100_000;
/// HYPE uses eight decimals; bound a CoreWriter `sendAsset` fee at 0.001 HYPE.
pub(crate) const MAX_CORE_ONWARD_NATIVE_FEE_ATOMS: u64 = 100_000;
/// Protected credential runtime records enforce this same byte bound. Check
/// every deterministic funding key before preflight can create/sign anything.
const MAX_FUNDING_RUNTIME_KEY_BYTES: usize = 512;
pub const PAPERTRADE_PROXY_FACTORY: &str = "0x1629b7d46bef6e13ff754174fc9f32dea309b38f";

fn net_funding_credit_atoms(
    gross_atoms: u64,
    cctp_fee_atoms: u64,
    activation_fee_atoms: u64,
    minimum_net_atoms: u64,
) -> Result<u64, GoFinError> {
    let net = gross_atoms
        .checked_sub(cctp_fee_atoms)
        .and_then(|value| value.checked_sub(activation_fee_atoms))
        .filter(|value| *value >= minimum_net_atoms)
        .ok_or_else(|| {
            GoFinError::validation("Papertrade funding fees exceed its minimum net USDC bound.")
        })?;
    Ok(net)
}

/// Build the exact read-only factory call used to independently verify the
/// official Papertrade funding-check API's proxy result.
pub fn compute_proxy_address_call(owner: &str) -> Result<EvmCall, GoFinError> {
    let owner = gofin_evm::address(owner)?;
    let selector = gofin_evm::alloy::primitives::keccak256(b"computeProxyAddress(address)");
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&selector[..4]);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(owner.as_slice());
    Ok(EvmCall {
        chain_id: 999,
        to: PAPERTRADE_PROXY_FACTORY.into(),
        value: gofin_domain::Quantity::from_u64(0),
        data: format!("0x{}", hex::encode(data)),
    })
}

/// Decode the exact ABI return value of `computeProxyAddress(address)`.
pub fn decode_proxy_address_result(result: &str) -> Result<String, GoFinError> {
    let raw = hex::decode(
        result
            .strip_prefix("0x")
            .ok_or_else(|| GoFinError::validation("Factory proxy result is not hex encoded."))?,
    )
    .map_err(|_| GoFinError::validation("Factory proxy result is malformed."))?;
    if raw.len() != 32 || raw[..12].iter().any(|b| *b != 0) || raw[12..].iter().all(|b| *b == 0) {
        return Err(GoFinError::validation(
            "Factory proxy result is not one nonzero ABI address word.",
        ));
    }
    Ok(format!("0x{}", hex::encode(&raw[12..])))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PapertradeEventProof {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub confirmations: u64,
    pub owner: String,
    pub usdc_atoms: u64,
    pub internal_usdc_wad: String,
}

pub(crate) fn event_u128(data: &str, index: usize) -> Option<u128> {
    let raw = hex::decode(data.strip_prefix("0x")?).ok()?;
    let start = index.checked_mul(32)?;
    let word = raw.get(start..start + 32)?;
    if word[..16].iter().any(|b| *b != 0) {
        return None;
    }
    Some(u128::from_be_bytes(word[16..].try_into().ok()?))
}

fn event_address_topic(owner: &str) -> Option<String> {
    if owner.len() != 42 || !owner.starts_with("0x") {
        return None;
    }
    let address_bytes = hex::decode(&owner[2..]).ok()?;
    Some(format!(
        "0x{}{}",
        "00".repeat(12),
        hex::encode(address_bytes)
    ))
}

fn matching_event<'a>(
    receipt: &'a Value,
    emitter: &str,
    topic: &str,
    owner: &str,
) -> Result<&'a Value, GoFinError> {
    let owner_topic = event_address_topic(owner).ok_or_else(|| {
        GoFinError::validation("Papertrade event proof has an invalid owner address.")
    })?;
    let logs = receipt
        .get("logs")
        .and_then(Value::as_array)
        .ok_or_else(|| GoFinError::validation("Canonical Papertrade receipt has no logs."))?;
    let rows: Vec<_> = logs
        .iter()
        .filter(|log| {
            log.get("address")
                .and_then(Value::as_str)
                .is_some_and(|v| v.eq_ignore_ascii_case(emitter))
                && log
                    .get("topics")
                    .and_then(Value::as_array)
                    .is_some_and(|topics| {
                        topics.len() == 2
                            && topics[0]
                                .as_str()
                                .is_some_and(|v| v.eq_ignore_ascii_case(topic))
                            && topics[1]
                                .as_str()
                                .is_some_and(|v| v.eq_ignore_ascii_case(&owner_topic))
                    })
        })
        .collect();
    if rows.len() != 1 {
        return Err(funding_error(
            "PAPERTRADE_EVENT_AMBIGUOUS",
            "reconciliation",
            "Canonical receipt does not contain exactly one owner-bound Papertrade funding event.",
        ));
    }
    Ok(rows[0])
}

/// Verify Papertrade's internal USDC withdrawal into the owner's Core spot
/// balance. The event's second field is the exact eight-decimal Core USDC
/// amount; its first field is the 18-decimal internal cash debit.
#[cfg(test)]
pub fn verify_core_withdrawal_event(
    receipt: &gofin_evm::CanonicalReceipt,
    exchange: &str,
    owner: &str,
    expected_atoms: u64,
) -> Result<PapertradeEventProof, GoFinError> {
    verify_canonical_receipt(receipt)?;
    let event = matching_event(&receipt.receipt, exchange, CORE_WITHDRAWAL_TOPIC, owner)?;
    let data = event
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| GoFinError::validation("CoreWithdrawal event data is missing."))?;
    let internal_wad = event_u128(data, 0).ok_or_else(|| {
        GoFinError::validation("CoreWithdrawal internal USDC debit is malformed.")
    })?;
    let received_atoms = event_u128(data, 1).ok_or_else(|| {
        GoFinError::validation("CoreWithdrawal received USDC amount is malformed.")
    })?;
    let expected_wad = u128::from(expected_atoms) * 1_000_000_000_000;
    let expected_core_atoms = u128::from(expected_atoms) * 100;
    if received_atoms != expected_core_atoms || internal_wad != expected_wad {
        return Err(funding_error(
            "PAPERTRADE_WITHDRAWAL_AMOUNT_MISMATCH",
            "reconciliation",
            "CoreWithdrawal does not match the exact approved internal debit and USDC amount.",
        ));
    }
    Ok(PapertradeEventProof {
        transaction_hash: format!("{:#x}", receipt.hash),
        block_hash: format!("{:#x}", receipt.block_hash),
        block_number: receipt.block_number,
        confirmations: receipt.confirmations,
        owner: owner.to_ascii_lowercase(),
        usdc_atoms: expected_atoms,
        internal_usdc_wad: internal_wad.to_string(),
    })
}

/// Verify the final Exchange cash credit attributed to the exact enrolled
/// proxy after registration and sweep. The event is emitted by Exchange and
/// indexes the account owner; the event stores USDC in 18-decimal wad.
pub fn verify_swept_event(
    receipt: &gofin_evm::CanonicalReceipt,
    exchange: &str,
    owner: &str,
    expected_atoms: u64,
) -> Result<PapertradeEventProof, GoFinError> {
    verify_canonical_receipt(receipt)?;
    let event = matching_event(&receipt.receipt, exchange, SWEPT_TOPIC, owner)?;
    let data = event
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| GoFinError::validation("Swept event data is missing."))?;
    let credited_wad = event_u128(data, 0)
        .ok_or_else(|| GoFinError::validation("Swept internal USDC credit is malformed."))?;
    let expected_wad = u128::from(expected_atoms) * 1_000_000_000_000;
    if credited_wad != expected_wad {
        return Err(funding_error(
            "PAPERTRADE_DEPOSIT_AMOUNT_MISMATCH",
            "reconciliation",
            "Swept event does not match the exact approved USDC amount.",
        ));
    }
    Ok(PapertradeEventProof {
        transaction_hash: format!("{:#x}", receipt.hash),
        block_hash: format!("{:#x}", receipt.block_hash),
        block_number: receipt.block_number,
        confirmations: receipt.confirmations,
        owner: owner.to_ascii_lowercase(),
        usdc_atoms: expected_atoms,
        internal_usdc_wad: credited_wad.to_string(),
    })
}

fn verify_canonical_receipt(receipt: &gofin_evm::CanonicalReceipt) -> Result<(), GoFinError> {
    if !receipt.success
        || receipt.block_number == 0
        || receipt.confirmations == 0
        || receipt.hash == gofin_evm::alloy::primitives::B256::ZERO
        || receipt.block_hash == gofin_evm::alloy::primitives::B256::ZERO
    {
        return Err(funding_error(
            "PAPERTRADE_RECEIPT_NOT_FINAL",
            "reconciliation",
            "Papertrade funding event is not in a successful canonical receipt.",
        ));
    }
    Ok(())
}

fn funding_error(code: &str, stage: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        stage,
        message,
        "Inspect the persisted funding operation and reconcile its original submissions before retrying.",
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingPreflight {
    pub owner: String,
    /// Result of the configured HyperEVM factory's `computeProxyAddress`.
    pub factory_proxy: String,
    pub account_mode: HyperliquidAccountMode,
    /// HyperEVM ERC-20 allowance observed from the enrolled owner to the
    /// reviewed CoreDepositWallet. Funding requires a sufficient, finite
    /// pre-existing allowance; it never creates a second financial envelope.
    #[serde(default)]
    pub hyperevm_usdc_allowance_atoms: Option<u64>,
    /// Route-policy gas bound selected by the broker, not caller input.
    #[serde(default)]
    pub native_fee_asset: Option<String>,
    #[serde(default)]
    pub maximum_native_fee_atoms: Option<String>,
    /// Proxy's existing HyperCore spot USDC balance, in atoms. Native deposits
    /// require a zero baseline so a later Exchange sweep cannot absorb funds
    /// from an earlier deposit.
    #[serde(default)]
    pub proxy_core_usdc_before_atoms: Option<u64>,
    /// The enrolled HyperCore owner's available USDC baseline used to prove
    /// an exact Core spot debit for a direct Core deposit.
    #[serde(default)]
    pub core_owner_usdc_before_atoms: Option<u64>,
    /// EVM scan lower bound captured before a Core action; used to correlate
    /// only the resulting Exchange sweep for this deposit.
    #[serde(default)]
    pub core_sweep_from_block: Option<u64>,
    /// Protected enrolled Solana owner, required only for Solana source or
    /// destination routes. Base58 is case-sensitive and is preserved exactly.
    #[serde(default)]
    pub solana_owner: Option<String>,
}

/// Acknowledgment from the official funding check endpoint that the exact
/// factory-derived proxy is registered for the owner. The endpoint returns no
/// server-side request ID, so the broker's durable request identity is bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyRegistrationProof {
    pub owner: String,
    pub api_proxy: String,
    pub factory_proxy: String,
    pub request_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CctpQuoteRequest {
    pub source_domain: u32,
    pub destination_domain: u32,
    pub amount_atoms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingCctpQuote {
    pub metadata: cctp::CctpQuoteMetadata,
    /// HyperCore's signed `gasLimit`, returned by the live route cost query.
    /// This is never a hard-coded fee estimate.
    pub hypercore_gas_limit: u64,
    /// Finite queried gas cost cap in the source network's native atom unit.
    pub native_fee_asset: Option<String>,
    pub maximum_native_fee_atoms: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FundingAction {
    CoreDeposit {
        action: CoreToPapertradeAction,
    },
    EvmCall {
        call: EvmCall,
        transaction: gofin_evm::FrozenTransaction,
    },
    SolanaCctpBurn {
        request: cctp::solana::hypercore_source::BurnRequest,
        instruction: NormalizedInstruction,
        transaction: gofin_papertrade::SolanaFundingTransactionBinding,
    },
    HyperCoreCctpWithdraw {
        action: SendToEvmWithDataAction,
    },
    HyperCoreSendAsset {
        action: gofin_hyperliquid::funding::SendAssetAction,
    },
    PapertradeWithdrawToCore {
        bound: gofin_papertrade::BoundIntent,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingLeg {
    pub ordinal: u8,
    /// Stable within an operation and persisted before dispatch.
    pub submission_id: String,
    pub action: FundingAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundFunding {
    pub version: u32,
    pub operation_id: String,
    pub intent: FundingIntent,
    pub proxy: String,
    pub source_identity: String,
    pub destination_identity: String,
    pub minimum_received_usdc_atoms: u64,
    pub bounds: gofin_papertrade::FundingFinancialBounds,
    /// Exact Circle cost/route quote used to construct any CCTP leg. Kept in
    /// the private broker plan so reconciliation never re-quotes a dispatched
    /// burn or reconstructs mutable protocol metadata.
    #[serde(default)]
    pub cctp_quote: Option<FundingCctpQuote>,
    /// Private host journal baseline; not caller-selected or exposed by CLI.
    #[serde(default)]
    pub proxy_core_usdc_before_atoms: Option<u64>,
    #[serde(default)]
    pub core_owner_usdc_before_atoms: Option<u64>,
    #[serde(default)]
    pub core_sweep_from_block: Option<u64>,
    #[serde(default)]
    pub proxy_registration: Option<ProxyRegistrationProof>,
    pub legs: Vec<FundingLeg>,
}

/// Public approval binding for the separately authorized second stage of a
/// Papertrade withdrawal. The original `BoundFunding` remains immutable and
/// protected; the continuation identifies it by both its plan fingerprint
/// and the canonical Core-credit proof observed after stage one. Leg actions
/// remain private and are loaded from the original plan at dispatch time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundFundingContinuation {
    pub version: u32,
    /// Identity of this separately authorized stage-two operation.
    pub operation_id: String,
    /// Identity of the original `papertrade.withdraw` plan.
    pub route_id: String,
    /// Exact original outer submission, retained to prevent a continuation
    /// from being attached to a different parent withdrawal.
    pub parent_submission: gofin_domain::Submission,
    pub plan_fingerprint: String,
    pub stage1_proof_fingerprint: String,
    pub intent: FundingIntent,
    pub source_identity: String,
    pub destination_identity: String,
    /// Stage-two-only bounds. The internal Papertrade debit and Core credit
    /// were settled by stage one and must not be charged or credited again.
    pub bounds: gofin_papertrade::FundingFinancialBounds,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum FundingLegState {
    NotSubmitted,
    Submitted { evidence: Value },
    Settled { evidence: Value },
    Rejected { evidence: Value },
    Unknown { evidence: Value },
}

/// Implemented by the protected broker runtime. `persist_plan` must durably
/// save every leg identity before `dispatch_leg` can submit a write.
#[async_trait::async_trait]
pub trait PapertradeFundingHost: Send + Sync {
    async fn preflight(&self, intent: &FundingIntent) -> Result<FundingPreflight, GoFinError>;
    /// Ensure relayer registration for the exact owner/proxy pair and return
    /// only after that durable registration is independently confirmed.
    async fn ensure_proxy_registered(
        &self,
        operation_id: &str,
        owner: &str,
        factory_proxy: &str,
    ) -> Result<ProxyRegistrationProof, GoFinError>;
    async fn cctp_quote(&self, request: &CctpQuoteRequest) -> Result<FundingCctpQuote, GoFinError>;
    /// Freeze one exact transaction envelope before approval. The resulting
    /// unsigned transaction is stored in the funding binding and is the only
    /// transaction nested dispatch may submit.
    async fn prepare_evm_call(
        &self,
        submission_id: &str,
        intent: &FundingIntent,
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

    /// Freeze the exact source burn instruction against current finalized
    /// Solana blockhash/fee policy and retain its private runtime transaction
    /// under `submission_id`. The returned summary is the public immutable
    /// execution binding; it contains no signed payload.
    async fn prepare_solana_burn_transaction(
        &self,
        submission_id: &str,
        instruction: &NormalizedInstruction,
        maximum_network_fee_lamports: u64,
    ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError>;
    /// Reserve and durably bind a unique Hyperliquid nonce for this leg before
    /// it is added to the immutable plan.
    async fn reserve_hyperliquid_nonce(&self, submission_id: &str) -> Result<u64, GoFinError>;
    async fn papertrade_withdraw_to_core_intent(
        &self,
        operation_id: &str,
        intent: &FundingIntent,
    ) -> Result<gofin_papertrade::BoundIntent, GoFinError>;
    async fn persist_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError>;
    /// Compare the supplied binding byte-for-byte with the durable plan for
    /// its operation. This prevents a modified plan from changing recipients,
    /// amounts or write identities between prepare and dispatch.
    async fn assert_persisted_plan(&self, plan: &BoundFunding) -> Result<(), GoFinError>;
    /// Dispatch is allowed only for an unsubmitted persisted identity. The
    /// implementation rejects a second call after any ambiguous submission.
    async fn dispatch_leg(
        &self,
        leg: &FundingLeg,
        outer_submission: &gofin_domain::Submission,
        journal: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
        validated: &gofin_domain::RevalidatedOperation,
    ) -> Result<FundingLegState, GoFinError>;
    /// Read/reconcile only. This method must never issue the source write.
    async fn reconcile_leg(
        &self,
        plan: &BoundFunding,
        leg: &FundingLeg,
    ) -> Result<FundingLegState, GoFinError>;
    /// Return a receipt only after exact destination-side USDC credit/debit is
    /// proven, including the proxy registration/sweep for deposits.
    async fn prove_destination(
        &self,
        plan: &BoundFunding,
        settled_legs: &[FundingLegState],
    ) -> Result<Option<FundingReceipt>, GoFinError>;
}

fn evm_source_domain(route: &FundingRoute) -> Result<u32, GoFinError> {
    match route {
        FundingRoute::Ethereum => Ok(0),
        FundingRoute::Arbitrum => Ok(3),
        FundingRoute::Solana => Ok(5),
        FundingRoute::Core | FundingRoute::Hyperevm => Ok(cctp::hypercore::DOMAIN),
    }
}

fn route_domain(route: &FundingRoute) -> Result<u32, GoFinError> {
    match route {
        FundingRoute::Core | FundingRoute::Hyperevm => Ok(cctp::hypercore::DOMAIN),
        FundingRoute::Ethereum => Ok(0),
        FundingRoute::Arbitrum => Ok(3),
        FundingRoute::Solana => Ok(5),
    }
}

fn amount_decimal(atoms: u64) -> String {
    let whole = atoms / 1_000_000;
    let fractional = atoms % 1_000_000;
    if fractional == 0 {
        return whole.to_string();
    }
    let mut fraction = format!("{fractional:06}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{whole}.{fraction}")
}

fn leg(operation_id: &str, ordinal: u8, action: FundingAction) -> FundingLeg {
    FundingLeg {
        ordinal,
        submission_id: format!("papertrade-funding:{operation_id}:{ordinal}"),
        action,
    }
}

async fn prepared_evm_leg<H: PapertradeFundingHost>(
    host: &H,
    operation_id: &str,
    ordinal: u8,
    intent: &FundingIntent,
    call: EvmCall,
    maximum_total_fee_atoms: &str,
) -> Result<FundingLeg, GoFinError> {
    let submission_id = format!("papertrade-funding:{operation_id}:{ordinal}");
    let transaction = host
        .prepare_evm_call(&submission_id, intent, &call, maximum_total_fee_atoms)
        .await?;
    if transaction.chain_id != call.chain_id
        || transaction.to != call.to
        || transaction.calldata != call.data
        || transaction.value != call.value
    {
        return Err(funding_error(
            "PAPERTRADE_FROZEN_ENVELOPE_MISMATCH",
            "preview",
            "Frozen EVM transaction differs from the reviewed funding call.",
        ));
    }
    Ok(leg(
        operation_id,
        ordinal,
        FundingAction::EvmCall { call, transaction },
    ))
}

async fn build_plan<H: PapertradeFundingHost>(
    host: &H,
    operation_id: &str,
    intent: FundingIntent,
) -> Result<BoundFunding, GoFinError> {
    validate_funding(&intent)?;
    if operation_id.trim().is_empty() {
        return Err(GoFinError::validation(
            "Papertrade funding requires a stable operation identity.",
        ));
    }
    validate_funding_storage_keys(operation_id)?;
    let preflight = host.preflight(&intent).await?;
    if !preflight.owner.eq_ignore_ascii_case(&intent.owner)
        || !preflight.factory_proxy.starts_with("0x")
        || preflight.factory_proxy.len() != 42
        || !preflight.factory_proxy[2..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(funding_error(
            "PAPERTRADE_FUNDING_IDENTITY_MISMATCH",
            "preview",
            "Funding owner or computed Papertrade proxy differs from the enrolled identity.",
        ));
    }
    if intent.route == FundingRoute::Hyperevm
        && intent.direction == FundingDirection::Deposit
        && preflight.proxy_core_usdc_before_atoms != Some(0)
    {
        return Err(funding_error(
            "PAPERTRADE_PROXY_BALANCE_AMBIGUOUS",
            "preview",
            "The Papertrade proxy Core USDC baseline is missing or nonzero.",
        ));
    }
    if intent.route == FundingRoute::Core
        && intent.direction == FundingDirection::Deposit
        && preflight
            .core_owner_usdc_before_atoms
            .is_none_or(|balance| balance < intent.amount_usdc_atoms)
    {
        return Err(funding_error(
            "HYPERCORE_USDC_INSUFFICIENT",
            "preview",
            "The enrolled HyperCore spot wallet does not have enough available USDC for this deposit.",
        ));
    }
    let solana_owner = if intent.route == FundingRoute::Solana {
        let address = preflight.solana_owner.as_deref().ok_or_else(|| {
            GoFinError::validation(
                "No protected Solana wallet is enrolled for this Papertrade route.",
            )
        })?;
        Some(gofin_solana::parse_address(address)?)
    } else {
        None
    };
    let proxy_registration = if intent.direction == FundingDirection::Deposit {
        let proof = host
            .ensure_proxy_registered(operation_id, &intent.owner, &preflight.factory_proxy)
            .await?;
        if !proof.owner.eq_ignore_ascii_case(&intent.owner)
            || !proof
                .api_proxy
                .eq_ignore_ascii_case(&preflight.factory_proxy)
            || !proof
                .factory_proxy
                .eq_ignore_ascii_case(&preflight.factory_proxy)
            || proof.request_id != format!("papertrade-proxy-check:{operation_id}")
        {
            return Err(funding_error(
                "PAPERTRADE_PROXY_REGISTRATION_MISMATCH",
                "preview",
                "Papertrade proxy registration does not match the exact owner and computed proxy.",
            ));
        }
        Some(proof)
    } else {
        None
    };
    let proxy = proxy_registration.as_ref().map_or_else(
        || preflight.factory_proxy.clone(),
        |proof| proof.api_proxy.clone(),
    );
    let amount = intent.amount_usdc_atoms;
    let mut minimum_received = amount;
    let mut maximum_cctp_fee = 0u64;
    let mut maximum_activation_fee = 0u64;
    let mut maximum_source_fee = 0u64;
    let mut native_fee_asset = preflight.native_fee_asset.clone();
    let mut maximum_native_fee_atoms = preflight.maximum_native_fee_atoms.clone();
    let mut maximum_refundable_native_funding_atoms = None;
    let mut maximum_native_source_debit_atoms = None;
    let mut cctp_quote = None;
    let amount_decimal_text = amount_decimal(amount);
    let mut legs = Vec::new();

    match intent.direction {
        FundingDirection::Deposit => match intent.route {
            FundingRoute::Core => {
                // Direct Core sends charge the sender's USDC fee separately;
                // the requested action amount is still the exact proxy credit.
                maximum_source_fee = 1_000_000;
                let submission_id = format!("papertrade-funding:{operation_id}:0");
                let nonce = host.reserve_hyperliquid_nonce(&submission_id).await?;
                let action = core_spot_deposit_action(
                    preflight.account_mode,
                    &intent.owner,
                    &proxy,
                    &amount_decimal_text,
                    nonce,
                )
                .map_err(|_| GoFinError::validation("Invalid Core funding action."))?;
                legs.push(leg(operation_id, 0, FundingAction::CoreDeposit { action }));
            }
            FundingRoute::Hyperevm => {
                // A first-time HyperCore proxy may pay the reviewed 1 USDC
                // activation charge from the gross native bridge amount.
                maximum_activation_fee = 1_000_000;
                minimum_received = net_funding_credit_atoms(
                    amount,
                    maximum_cctp_fee,
                    maximum_activation_fee,
                    MIN_PAPERTRADE_FUNDING_USDC_ATOMS,
                )?;
                let token = cctp::usdc_asset(999)?
                    .token
                    .ok_or_else(|| GoFinError::validation("HyperEVM native USDC is missing."))?;
                let allowance = preflight.hyperevm_usdc_allowance_atoms.ok_or_else(|| {
                    GoFinError::validation(
                        "A verified finite HyperEVM USDC allowance is required before funding.",
                    )
                })?;
                if allowance < amount || allowance == u64::MAX {
                    return Err(GoFinError::validation(
                        "HyperEVM USDC allowance is insufficient or unbounded; approve an exact finite allowance first.",
                    ));
                }
                if maximum_native_fee_atoms
                    .as_deref()
                    .and_then(|value| value.parse::<u128>().ok())
                    .is_none_or(|value| value == 0)
                    || native_fee_asset.as_deref() != Some("ethereum:999:native")
                {
                    return Err(GoFinError::validation(
                        "HyperEVM funding requires a finite broker-reviewed HYPE gas-cost cap.",
                    ));
                }
                let deposit = cctp::hyperevm::deposit_call(
                    proxy
                        .parse()
                        .map_err(|_| GoFinError::validation("Invalid Papertrade proxy."))?,
                    &gofin_domain::Quantity::from_u64(amount),
                )?;
                if token != cctp::usdc_asset(999)?.token.unwrap_or_default() {
                    return Err(GoFinError::validation(
                        "HyperEVM USDC token configuration drifted.",
                    ));
                }
                legs.push(
                    prepared_evm_leg(
                        host,
                        operation_id,
                        0,
                        &intent,
                        deposit,
                        maximum_native_fee_atoms
                            .as_deref()
                            .expect("validated fee cap"),
                    )
                    .await?,
                );
            }
            FundingRoute::Ethereum | FundingRoute::Arbitrum => {
                maximum_activation_fee = 1_000_000;
                let chain_id = if intent.route == FundingRoute::Ethereum {
                    1
                } else {
                    42161
                };
                let quote = host
                    .cctp_quote(&CctpQuoteRequest {
                        source_domain: evm_source_domain(&intent.route)?,
                        destination_domain: cctp::hypercore::DOMAIN,
                        amount_atoms: amount,
                    })
                    .await?;
                cctp_quote = Some(quote.clone());
                let metadata = quote.metadata;
                let fee = metadata.max_fee.to_string().parse::<u64>().map_err(|_| {
                    GoFinError::validation("CCTP quote fee exceeds the supported USDC range.")
                })?;
                maximum_cctp_fee = fee;
                native_fee_asset = Some(quote.native_fee_asset.ok_or_else(|| {
                    GoFinError::validation("EVM CCTP source quote omitted its native fee asset.")
                })?);
                maximum_native_fee_atoms =
                    Some(quote.maximum_native_fee_atoms.ok_or_else(|| {
                        GoFinError::validation("EVM CCTP source quote omitted its native fee cap.")
                    })?);
                minimum_received = net_funding_credit_atoms(
                    amount,
                    fee,
                    maximum_activation_fee,
                    MIN_PAPERTRADE_FUNDING_USDC_ATOMS,
                )?;
                let call = cctp::hypercore::burn_call(
                    chain_id,
                    &gofin_domain::Quantity::from_u64(amount),
                    &metadata,
                    proxy
                        .parse()
                        .map_err(|_| GoFinError::validation("Invalid Papertrade proxy."))?,
                )?;
                legs.push(
                    prepared_evm_leg(
                        host,
                        operation_id,
                        0,
                        &intent,
                        call,
                        maximum_native_fee_atoms.as_deref().expect("CCTP fee cap"),
                    )
                    .await?,
                );
            }
            FundingRoute::Solana => {
                maximum_activation_fee = 1_000_000;
                let quote = host
                    .cctp_quote(&CctpQuoteRequest {
                        source_domain: 5,
                        destination_domain: cctp::hypercore::DOMAIN,
                        amount_atoms: amount,
                    })
                    .await?;
                cctp_quote = Some(quote.clone());
                let metadata = quote.metadata;
                let fee = metadata.max_fee.to_string().parse::<u64>().map_err(|_| {
                    GoFinError::validation("CCTP quote fee exceeds the supported USDC range.")
                })?;
                maximum_cctp_fee = fee;
                native_fee_asset = Some(quote.native_fee_asset.ok_or_else(|| {
                    GoFinError::validation("Solana CCTP source quote omitted its native fee asset.")
                })?);
                maximum_native_fee_atoms =
                    Some(quote.maximum_native_fee_atoms.ok_or_else(|| {
                        GoFinError::validation(
                            "Solana CCTP source quote omitted its native fee cap.",
                        )
                    })?);
                minimum_received = net_funding_credit_atoms(
                    amount,
                    fee,
                    maximum_activation_fee,
                    MIN_PAPERTRADE_FUNDING_USDC_ATOMS,
                )?;
                let request = host
                    .solana_burn_request(operation_id, &intent, &metadata, &proxy)
                    .await?;
                let rent_lamports = request.message_sent_event_rent_lamports;
                let fee_cap = maximum_native_fee_atoms
                    .as_deref()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        GoFinError::validation("Solana CCTP source has no finite native fee cap.")
                    })?;
                const MAX_SOLANA_SOURCE_NATIVE_DEBIT_LAMPORTS: u64 = 5_000_000;
                if rent_lamports == 0
                    || rent_lamports
                        .checked_add(fee_cap)
                        .is_none_or(|total| total > MAX_SOLANA_SOURCE_NATIVE_DEBIT_LAMPORTS)
                {
                    return Err(GoFinError::validation(
                        "Solana CCTP rent plus network fee exceeds the approved 0.005 SOL total source bound.",
                    ));
                }
                maximum_refundable_native_funding_atoms = Some(rent_lamports.to_string());
                maximum_native_source_debit_atoms =
                    Some(MAX_SOLANA_SOURCE_NATIVE_DEBIT_LAMPORTS.to_string());
                let proxy_address = proxy
                    .parse()
                    .map_err(|_| GoFinError::validation("Invalid Papertrade proxy."))?;
                let instruction = cctp::solana::hypercore_source::burn_instruction(
                    &request,
                    &metadata,
                    proxy_address,
                )?;
                let submission_id = format!("papertrade-funding:{operation_id}:0");
                let transaction = host
                    .prepare_solana_burn_transaction(
                        &submission_id,
                        &instruction,
                        maximum_native_fee_atoms
                            .as_deref()
                            .and_then(|value| value.parse::<u64>().ok())
                            .ok_or_else(|| {
                                GoFinError::validation(
                                    "Solana CCTP source has no finite native fee cap.",
                                )
                            })?,
                    )
                    .await?;
                legs.push(leg(
                    operation_id,
                    0,
                    FundingAction::SolanaCctpBurn {
                        request,
                        instruction,
                        transaction,
                    },
                ));
            }
        },
        FundingDirection::Withdraw => {
            let bound = host
                .papertrade_withdraw_to_core_intent(operation_id, &intent)
                .await?;
            let matches_intent = match &bound.typed {
                gofin_papertrade::PapertradeTypedIntent::WithdrawToCore {
                    user,
                    amount: typed_amount,
                    ..
                } => {
                    user.eq_ignore_ascii_case(&intent.owner)
                        && bound.signer.eq_ignore_ascii_case(&intent.owner)
                        && typed_amount == &amount.to_string()
                        && bound.action.as_deref() == Some("withdrawToCore")
                        && bound.registration_id.is_none()
                        && bound.cancel_target_action.is_none()
                }
                _ => false,
            };
            if !matches_intent {
                return Err(funding_error(
                    "PAPERTRADE_WITHDRAW_BINDING_MISMATCH",
                    "preview",
                    "The protected Papertrade withdrawal intent does not match the exact owner and USDC amount.",
                ));
            }
            legs.push(leg(
                operation_id,
                0,
                FundingAction::PapertradeWithdrawToCore { bound },
            ));
            match intent.route {
                FundingRoute::Core => {}
                FundingRoute::Hyperevm => {
                    let submission_id = format!("papertrade-funding:{operation_id}:1");
                    let nonce = host.reserve_hyperliquid_nonce(&submission_id).await?;
                    maximum_source_fee = CORE_WITHDRAWAL_RESERVE_ATOMS;
                    if native_fee_asset.as_deref() != Some("hyperliquid:mainnet:HYPE")
                        || maximum_native_fee_atoms
                            .as_deref()
                            .and_then(|value| value.parse::<u64>().ok())
                            .is_none_or(|cap| cap == 0 || cap > MAX_CORE_ONWARD_NATIVE_FEE_ATOMS)
                    {
                        return Err(GoFinError::validation(
                            "HyperEVM withdrawal requires a finite reviewed HYPE fee bound.",
                        ));
                    }
                    let bridge_amount_atoms = amount
                        .checked_sub(CORE_WITHDRAWAL_RESERVE_ATOMS)
                        .filter(|value| *value > 0)
                        .ok_or_else(|| {
                            GoFinError::validation(
                                "HyperEVM withdrawal amount must exceed the retained 0.1 USDC reserve.",
                            )
                        })?;
                    // HyperCore charges its native send fee separately from
                    // the token amount. The EVM destination receives the
                    // exact stage-two principal; any fee is paid from this
                    // explicitly retained reserve.
                    minimum_received = bridge_amount_atoms;
                    let action = same_owner_hyperevm_send_asset_action(
                        gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
                        "Mainnet",
                        &intent.owner,
                        &intent.owner,
                        &amount_decimal(bridge_amount_atoms),
                        nonce,
                    )
                    .map_err(|_| {
                        GoFinError::validation("Invalid same-owner HyperEVM withdrawal.")
                    })?;
                    legs.push(leg(
                        operation_id,
                        1,
                        FundingAction::HyperCoreSendAsset { action },
                    ));
                }
                FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana => {
                    // The Core onward transfer fee is charged separately from
                    // the bridge amount and the retained 0.1 USDC reserve.
                    // Bound it independently; actual observed ledger fee is
                    // emitted as a separate financial effect.
                    // Core retains a fixed 0.1 USDC reserve for the outbound
                    // CCTP send fee; the burn amount is separately reduced by
                    // that reserve. Do not authorize more source fee than it.
                    maximum_source_fee = 100_000;
                    let submission_id = format!("papertrade-funding:{operation_id}:1");
                    let nonce = host.reserve_hyperliquid_nonce(&submission_id).await?;
                    let domain = route_domain(&intent.route)?;
                    // The reviewed HyperCore CCTP path retains 0.1 USDC in
                    // Core before the outbound send. Circle fees are then
                    // deducted from this exact bridge amount.
                    const CORE_WITHDRAWAL_RESERVE_ATOMS: u64 = 100_000;
                    let bridge_amount_atoms = amount
                        .checked_sub(CORE_WITHDRAWAL_RESERVE_ATOMS)
                        .filter(|value| *value > 0)
                        .ok_or_else(|| {
                            GoFinError::validation(
                                "Core CCTP withdrawal amount must exceed the retained 0.1 USDC reserve.",
                            )
                        })?;
                    let quote = host
                        .cctp_quote(&CctpQuoteRequest {
                            source_domain: cctp::hypercore::DOMAIN,
                            destination_domain: domain,
                            amount_atoms: bridge_amount_atoms,
                        })
                        .await?;
                    cctp_quote = Some(quote.clone());
                    let metadata = quote.metadata;
                    if metadata.protocol_version != 2
                        || metadata.source_domain != cctp::hypercore::DOMAIN
                        || metadata.destination_domain != domain
                        || metadata.max_fee >= gofin_domain::Quantity::from_u64(bridge_amount_atoms)
                    {
                        return Err(funding_error(
                            "CCTP_QUOTE_MISMATCH",
                            "preview",
                            "CCTP quote differs from the exact HyperCore withdrawal route.",
                        ));
                    }
                    let fee = metadata.max_fee.to_string().parse::<u64>().map_err(|_| {
                        GoFinError::validation("CCTP quote fee exceeds the supported USDC range.")
                    })?;
                    maximum_cctp_fee = fee;
                    match (quote.native_fee_asset, quote.maximum_native_fee_atoms) {
                        (None, None) => {
                            native_fee_asset = None;
                            maximum_native_fee_atoms = None;
                        }
                        (Some(asset), Some(cap)) => {
                            native_fee_asset = Some(asset);
                            maximum_native_fee_atoms = Some(cap);
                        }
                        _ => {
                            return Err(GoFinError::validation(
                                "CCTP route quote supplied an incomplete native fee bound.",
                            ));
                        }
                    }
                    if metadata.source_domain == cctp::hypercore::DOMAIN
                        && (native_fee_asset.as_deref() != Some("hyperliquid:mainnet:HYPE")
                            || maximum_native_fee_atoms
                                .as_deref()
                                .and_then(|value| value.parse::<u64>().ok())
                                .is_none_or(|cap| {
                                    cap == 0 || cap > MAX_CORE_ONWARD_NATIVE_FEE_ATOMS
                                }))
                    {
                        return Err(GoFinError::validation(
                            "HyperCore CCTP withdrawal requires the reviewed finite HYPE fee bound.",
                        ));
                    }
                    minimum_received = net_funding_credit_atoms(
                        bridge_amount_atoms,
                        fee,
                        maximum_activation_fee,
                        1,
                    )?;
                    let target = match intent.route {
                        FundingRoute::Ethereum => CctpWithdrawalTarget::Ethereum {
                            recipient: &intent.owner,
                        },
                        FundingRoute::Arbitrum => CctpWithdrawalTarget::Arbitrum {
                            recipient: &intent.owner,
                        },
                        FundingRoute::Solana => CctpWithdrawalTarget::Solana {
                            recipient_owner: solana_owner.as_deref().ok_or_else(|| {
                                GoFinError::validation(
                                    "No protected Solana destination is enrolled.",
                                )
                            })?,
                        },
                        _ => unreachable!(),
                    };
                    let action = cctp_withdrawal_action(
                        gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
                        &amount_decimal(bridge_amount_atoms),
                        quote.hypercore_gas_limit,
                        nonce,
                        target,
                    )
                    .map_err(|_| GoFinError::validation("Invalid HyperCore CCTP withdrawal."))?;
                    legs.push(leg(
                        operation_id,
                        1,
                        FundingAction::HyperCoreCctpWithdraw { action },
                    ));
                }
            }
        }
    }

    let route_id = match intent.route {
        FundingRoute::Solana => solana_owner.as_deref().expect("validated Solana owner"),
        _ => &intent.owner,
    };
    let route_identity = route_identity(&intent.route, route_id);
    let owner_identity = format!("papertrade:mainnet:{}", intent.owner.to_ascii_lowercase());
    let (source_identity, destination_identity) = match intent.direction {
        FundingDirection::Deposit => (route_identity, owner_identity),
        FundingDirection::Withdraw => (owner_identity, route_identity),
    };
    if maximum_native_fee_atoms.as_deref().is_some_and(|value| {
        value.parse::<u128>().is_err() || value.parse::<u128>().is_ok_and(|amount| amount == 0)
    }) || (maximum_native_fee_atoms.is_some() != native_fee_asset.is_some())
    {
        return Err(GoFinError::validation(
            "Funding route native fee bound is malformed or unpaired with its asset.",
        ));
    }
    if native_fee_asset.is_some() != maximum_native_fee_atoms.is_some() {
        return Err(GoFinError::validation(
            "Funding route supplied an incomplete native fee bound.",
        ));
    }
    if intent.direction == FundingDirection::Deposit
        && matches!(
            intent.route,
            FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana
        )
        && maximum_native_fee_atoms.is_none()
    {
        return Err(GoFinError::validation(
            "Funding route requires a queried finite native transaction fee cap.",
        ));
    }
    let bounds = gofin_papertrade::FundingFinancialBounds {
        source_usdc_atoms: amount,
        maximum_source_fee_usdc_atoms: maximum_source_fee,
        destination_min_usdc_atoms: minimum_received,
        maximum_cctp_fee_usdc_atoms: maximum_cctp_fee,
        maximum_activation_fee_atoms: maximum_activation_fee,
        native_fee_asset,
        maximum_native_fee_atoms,
        maximum_refundable_native_funding_atoms,
        maximum_native_source_debit_atoms,
    };
    if intent.direction == FundingDirection::Deposit
        && bounds.destination_min_usdc_atoms < MIN_PAPERTRADE_FUNDING_USDC_ATOMS
    {
        return Err(GoFinError::validation(
            "Papertrade funding must guarantee at least ten USDC after route fees.",
        ));
    }
    Ok(BoundFunding {
        version: 1,
        operation_id: operation_id.to_owned(),
        intent,
        proxy,
        source_identity,
        destination_identity,
        minimum_received_usdc_atoms: minimum_received,
        bounds,
        cctp_quote,
        proxy_core_usdc_before_atoms: preflight.proxy_core_usdc_before_atoms,
        core_owner_usdc_before_atoms: preflight.core_owner_usdc_before_atoms,
        core_sweep_from_block: preflight.core_sweep_from_block,
        proxy_registration,
        legs,
    })
}

/// Reject operation IDs whose plan, leg, or leg-derived journal keys would
/// exceed the bounded protected runtime-record API. Do this before route
/// preflight because preflight may persist registration, signer, or nonce
/// identities even though it does not dispatch a financial write.
fn validate_funding_storage_keys(operation_id: &str) -> Result<(), GoFinError> {
    let mut keys = vec![format!("papertrade-funding-plan:{operation_id}")];
    // The current funding topology uses ordinals zero and one. Keep the
    // suffix list exhaustive with papertrade_funding_routes.rs and
    // papertrade_funding_inbound.rs: every derived key is an immutable
    // operation/recovery identity.
    const SUFFIXES: &[&str] = &[
        "solana_message_sent_event_signer",
        "dispatch_attempted",
        "dispatch_response",
        "settled_proof",
        "source_burn_proof",
        "discovered_action_hash",
        "solana_burn_request",
        "start_block",
        "acceptance",
        "core_credit_proof",
        "core_cctp_source_proof",
        "child_submission",
        "submission",
        "sweep_page_99999999999999999999",
        "inbound_hypercore_settled_proof",
        "nonce_invalidation_proof",
        "nonce_invalidation_attempted",
        "nonce_invalidation_response",
    ];
    for ordinal in 0..=1 {
        let submission_id = format!("papertrade-funding:{operation_id}:{ordinal}");
        for suffix in SUFFIXES {
            // The EVM funding host uses the stable submission id verbatim;
            // route and Solana source records hex-encode that same identity.
            keys.push(format!("papertrade-funding-leg:{submission_id}:{suffix}"));
            keys.push(format!(
                "papertrade-funding-leg:{}:{suffix}",
                hex::encode(submission_id.as_bytes())
            ));
        }
    }
    // The plan key contains the operation id verbatim, while all generated
    // leg keys must satisfy the same closed ASCII alphabet as the credential
    // runtime-record API. Engine step ids are arbitrary strings, so validate
    // before any preflight that could persist an identity or signer.
    if !operation_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-_:".contains(&byte))
        || keys
            .iter()
            .any(|key| key.len() > MAX_FUNDING_RUNTIME_KEY_BYTES)
    {
        return Err(funding_error(
            "PAPERTRADE_FUNDING_ID_TOO_LONG",
            "preview",
            "Funding operation identity is invalid for its durable recovery records.",
        ));
    }
    Ok(())
}

/// Prepare fixed route actions and persist every durable leg identity before
/// returning the approved binding.
pub async fn prepare_funding<H: PapertradeFundingHost>(
    host: &H,
    operation_id: &str,
    intent: FundingIntent,
) -> Result<BoundFunding, GoFinError> {
    let plan = build_plan(host, operation_id, intent).await?;
    host.persist_plan(&plan).await?;
    Ok(plan)
}

fn route_identity(route: &FundingRoute, owner: &str) -> String {
    match route {
        FundingRoute::Core => format!(
            "hyperliquid:mainnet:{}:spot:primary",
            owner.to_ascii_lowercase()
        ),
        FundingRoute::Hyperevm => format!("ethereum:999:{}", owner.to_ascii_lowercase()),
        FundingRoute::Ethereum => format!("ethereum:1:{}", owner.to_ascii_lowercase()),
        FundingRoute::Arbitrum => format!("ethereum:42161:{}", owner.to_ascii_lowercase()),
        FundingRoute::Solana => format!("solana:mainnet-beta:{owner}"),
    }
}

fn status_name(state: &FundingLegState) -> &'static str {
    match state {
        FundingLegState::NotSubmitted => "not_submitted",
        FundingLegState::Submitted { .. } => "submitted",
        FundingLegState::Settled { .. } => "settled",
        FundingLegState::Rejected { .. } => "rejected",
        FundingLegState::Unknown { .. } => "unknown",
    }
}

fn validate_plan_registration(plan: &BoundFunding) -> Result<(), GoFinError> {
    match (&plan.intent.direction, &plan.proxy_registration) {
        (FundingDirection::Deposit, Some(proof))
            if proof.owner.eq_ignore_ascii_case(&plan.intent.owner)
                && proof.api_proxy.eq_ignore_ascii_case(&plan.proxy)
                && proof.factory_proxy.eq_ignore_ascii_case(&plan.proxy)
                && proof.request_id == format!("papertrade-proxy-check:{}", plan.operation_id) =>
        {
            Ok(())
        }
        (FundingDirection::Withdraw, None) => Ok(()),
        _ => Err(funding_error(
            "PAPERTRADE_PROXY_REGISTRATION_MISMATCH",
            "reconciliation",
            "Persisted deposit plan lacks exact relayer registration proof for its owner and proxy.",
        )),
    }
}

async fn finish_progress<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
    states: Vec<FundingLegState>,
) -> Result<FundingProgress, GoFinError> {
    let evidence = serde_json::json!({
        "operation_id": plan.operation_id,
        "legs": plan.legs.iter().zip(&states).map(|(leg, state)| serde_json::json!({
            "ordinal": leg.ordinal,
            "status": status_name(state),
        })).collect::<Vec<_>>(),
    });
    if states.len() != plan.legs.len() {
        return Err(GoFinError::validation(
            "Funding evidence does not cover every persisted leg.",
        ));
    }
    if states
        .iter()
        .any(|state| matches!(state, FundingLegState::Rejected { .. }))
    {
        return Ok(FundingProgress {
            status: FundingProgressStatus::Rejected,
            receipt: None,
            evidence,
        });
    }
    if states
        .iter()
        .any(|state| matches!(state, FundingLegState::Unknown { .. }))
    {
        return Ok(FundingProgress {
            status: FundingProgressStatus::Unknown,
            receipt: None,
            evidence,
        });
    }
    if states
        .iter()
        .any(|state| !matches!(state, FundingLegState::Settled { .. }))
    {
        return Ok(FundingProgress {
            status: FundingProgressStatus::Pending,
            receipt: None,
            evidence,
        });
    }
    let Some(receipt) = host.prove_destination(plan, &states).await? else {
        return Ok(FundingProgress {
            status: FundingProgressStatus::Pending,
            receipt: None,
            evidence,
        });
    };
    if receipt.status != "settled"
        || receipt.amount_usdc_atoms < plan.minimum_received_usdc_atoms
        || receipt.amount_usdc_atoms > plan.intent.amount_usdc_atoms
        || receipt.source_identity != plan.source_identity
        || receipt.destination_identity != plan.destination_identity
        || receipt.evidence_id.trim().is_empty()
    {
        return Err(funding_error(
            "PAPERTRADE_FUNDING_PROOF_MISMATCH",
            "reconciliation",
            "Final Papertrade funding proof does not match the exact route, owner and amount.",
        ));
    }
    Ok(FundingProgress {
        status: FundingProgressStatus::Settled,
        receipt: Some(receipt),
        evidence,
    })
}

fn finish_withdrawal_core_stage(
    plan: &BoundFunding,
    state: FundingLegState,
) -> Result<FundingProgress, GoFinError> {
    let leg_status = status_name(&state);
    let evidence = serde_json::json!({
        "operation_id":plan.operation_id,
        "stage":"core_credit",
        "legs":[
            {"ordinal":0,"status":leg_status},
            {"ordinal":1,"status":"not_submitted"}
        ]
    });
    let status = match &state {
        FundingLegState::Rejected { .. } => FundingProgressStatus::Rejected,
        FundingLegState::Unknown { .. } => FundingProgressStatus::Unknown,
        FundingLegState::NotSubmitted | FundingLegState::Submitted { .. } => {
            FundingProgressStatus::Pending
        }
        FundingLegState::Settled { .. } => {
            let stage1 = validate_stage1_core_credit(plan, &state)?;
            let papertrade_tx = stage1
                .get("transaction_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("Core-credit proof omitted its Papertrade transaction.")
                })?;
            let core_tx = stage1
                .get("core_credit_transaction_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("Core-credit proof omitted its HyperCore transaction.")
                })?;
            return Ok(FundingProgress {
                status: FundingProgressStatus::Settled,
                receipt: Some(FundingReceipt {
                    stage: FundingReceiptStage::CoreCredit,
                    status: "settled".into(),
                    amount_usdc_atoms: plan.intent.amount_usdc_atoms,
                    source_debit_usdc_atoms: plan.intent.amount_usdc_atoms,
                    destination_credit_usdc_atoms: plan.intent.amount_usdc_atoms,
                    actual_source_fee_usdc_atoms: 0,
                    actual_cctp_fee_usdc_atoms: 0,
                    retained_source_credit_usdc_atoms: 0,
                    retained_source_native_atoms: None,
                    retained_source_native_asset: None,
                    activation_fee_usdc_atoms: 0,
                    actual_native_fee_atoms: None,
                    actual_native_fee_asset: None,
                    source_identity: plan.source_identity.clone(),
                    destination_identity: format!(
                        "hyperliquid:mainnet:{}:spot:primary",
                        plan.intent.owner.to_ascii_lowercase()
                    ),
                    evidence_id: format!("papertrade-core-credit:{papertrade_tx}:{core_tx}"),
                }),
                evidence,
            });
        }
    };
    Ok(FundingProgress {
        status,
        receipt: None,
        evidence,
    })
}

fn funding_fingerprint<T: Serialize>(value: &T) -> Result<String, GoFinError> {
    let encoded = serde_json::to_vec(value).map_err(|_| {
        GoFinError::validation("Funding continuation identity could not be encoded.")
    })?;
    use sha2::Digest;
    Ok(format!("0x{}", hex::encode(sha2::Sha256::digest(encoded))))
}

pub(crate) fn validate_stage1_core_credit<'a>(
    plan: &BoundFunding,
    state: &'a FundingLegState,
) -> Result<&'a Value, GoFinError> {
    let FundingLegState::Settled { evidence } = state else {
        return Err(funding_error(
            "PAPERTRADE_WITHDRAWAL_CORE_CREDIT_PENDING",
            "reconciliation",
            "The original Papertrade withdrawal has not yet produced a canonical Core USDC credit.",
        ));
    };
    if evidence.get("kind").and_then(Value::as_str) != Some("canonical_papertrade_core_withdrawal")
        || evidence.get("owner").and_then(Value::as_str)
            != Some(plan.intent.owner.to_ascii_lowercase().as_str())
        || evidence.get("received_usdc_atoms").and_then(Value::as_u64)
            != Some(plan.intent.amount_usdc_atoms)
        || evidence
            .get("transaction_hash")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || evidence
            .get("core_credit_transaction_hash")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(funding_error(
            "PAPERTRADE_WITHDRAWAL_CORE_CREDIT_MISMATCH",
            "reconciliation",
            "The stage-one Core credit proof differs from the original owner, amount or canonical transaction identities.",
        ));
    }
    Ok(evidence)
}

pub(crate) fn build_continuation_binding(
    plan: &BoundFunding,
    operation_id: &str,
    parent_submission: &gofin_domain::Submission,
    stage1_evidence: &Value,
) -> Result<BoundFundingContinuation, GoFinError> {
    validate_funding_storage_keys(operation_id)?;
    if plan.version != 1
        || plan.intent.direction != FundingDirection::Withdraw
        || plan.intent.route == FundingRoute::Core
        || plan.legs.len() != 2
        || plan.legs[0].ordinal != 0
        || plan.legs[1].ordinal != 1
        || operation_id.trim().is_empty()
        || operation_id == plan.operation_id
        || parent_submission.kind != "papertrade_funding"
        || parent_submission.reference != plan.operation_id
        || parent_submission
            .data
            .get("operation_id")
            .and_then(Value::as_str)
            != Some(plan.operation_id.as_str())
        || plan.legs[0].submission_id != format!("papertrade-funding:{}:0", plan.operation_id)
        || plan.legs[1].submission_id != format!("papertrade-funding:{}:1", plan.operation_id)
        || !matches!(
            &plan.legs[0].action,
            FundingAction::PapertradeWithdrawToCore { bound }
                if matches!(
                    &bound.typed,
                    gofin_papertrade::PapertradeTypedIntent::WithdrawToCore { user, amount, .. }
                        if user.eq_ignore_ascii_case(&plan.intent.owner)
                            && bound.signer.eq_ignore_ascii_case(&plan.intent.owner)
                            && amount.parse::<u64>().ok() == Some(plan.intent.amount_usdc_atoms)
                )
        )
    {
        return Err(GoFinError::validation(
            "This route does not have a separately continuable withdrawal stage.",
        ));
    }
    let plan_fingerprint = funding_fingerprint(plan)?;
    let stage1_proof_fingerprint = funding_fingerprint(stage1_evidence)?;
    // Only the onward Core debit and destination settlement are approved in
    // this stage. For CCTP, stage one credited the full internal withdrawal
    // to Core and stage two burns gross minus the documented 0.1 USDC reserve.
    // The reserve left after the separately charged Core fee remains ordinary
    // Core balance; it is not a stage-two credit.
    let onward = plan.legs.get(1).ok_or_else(|| {
        GoFinError::validation("Protected withdrawal plan has no onward route leg.")
    })?;
    let principal_text = match &onward.action {
        FundingAction::HyperCoreCctpWithdraw { action }
            if action.action_type == "sendToEvmWithData"
                && action.hyperliquid_chain == "Mainnet"
                && action.token == gofin_hyperliquid::funding::USDC_TOKEN
                && action.source_dex == "spot"
                && action.destination_chain_id == route_domain(&plan.intent.route)? =>
        {
            action.amount.as_str()
        }
        FundingAction::HyperCoreSendAsset { action }
            if action.action_type == "sendAsset"
                && action.hyperliquid_chain == "Mainnet"
                && action.signature_chain_id.eq_ignore_ascii_case(
                    gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
                )
                && action.token == gofin_hyperliquid::funding::USDC_TOKEN
                && action
                    .destination
                    .eq_ignore_ascii_case(gofin_hyperliquid::funding::USDC_SYSTEM_ADDRESS)
                && action.source_dex == "spot"
                && action.destination_dex == "spot"
                && action.from_sub_account.is_empty() =>
        {
            action.amount.as_str()
        }
        _ => {
            return Err(GoFinError::validation(
                "Protected withdrawal onward leg is not an approved Core USDC route action.",
            ));
        }
    };
    let principal_atoms = gofin_domain::Quantity::from_decimal(principal_text, 6)?
        .as_str()
        .parse::<u64>()
        .map_err(|_| {
            GoFinError::validation("Core withdrawal amount exceeds the supported range.")
        })?;
    let expected_principal = match plan.intent.route {
        FundingRoute::Hyperevm
        | FundingRoute::Ethereum
        | FundingRoute::Arbitrum
        | FundingRoute::Solana => plan
            .intent
            .amount_usdc_atoms
            .checked_sub(100_000)
            .ok_or_else(|| {
                GoFinError::validation("Withdrawal is below its retained Core reserve.")
            })?,
        FundingRoute::Core => unreachable!("Core-only withdrawals have no continuation"),
    };
    if principal_atoms != expected_principal || principal_atoms == 0 {
        return Err(funding_error(
            "PAPERTRADE_FUNDING_CONTINUATION_ACTION_MISMATCH",
            "authorization",
            "The persisted Core withdrawal action does not match the exact stage-two principal.",
        ));
    }
    let mut bounds = plan.bounds.clone();
    bounds.source_usdc_atoms = principal_atoms;
    // The external destination is funded only by the onward principal; the
    // Papertrade withdrawal and its matching Core credit belong to stage one.
    bounds.destination_min_usdc_atoms = plan.minimum_received_usdc_atoms;
    bounds.maximum_activation_fee_atoms = 0;
    bounds.maximum_refundable_native_funding_atoms = None;
    bounds.maximum_native_source_debit_atoms = None;
    if plan.intent.route != FundingRoute::Hyperevm {
        bounds.native_fee_asset = None;
        bounds.maximum_native_fee_atoms = None;
    }
    let source_identity = route_identity(&FundingRoute::Core, &plan.intent.owner);
    Ok(BoundFundingContinuation {
        version: 1,
        operation_id: operation_id.to_owned(),
        route_id: plan.operation_id.clone(),
        parent_submission: parent_submission.clone(),
        plan_fingerprint,
        stage1_proof_fingerprint,
        intent: plan.intent.clone(),
        source_identity,
        destination_identity: plan.destination_identity.clone(),
        bounds,
    })
}

async fn revalidate_continuation<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
    binding: &BoundFundingContinuation,
) -> Result<(FundingLegState, FundingLegState), GoFinError> {
    host.assert_persisted_plan(plan).await?;
    let Some(source_leg) = plan.legs.first() else {
        return Err(GoFinError::validation(
            "Protected funding plan has no stage-one Core-credit leg.",
        ));
    };
    let stage1 = host.reconcile_leg(plan, source_leg).await?;
    let stage1_evidence = validate_stage1_core_credit(plan, &stage1)?;
    let expected = build_continuation_binding(
        plan,
        &binding.operation_id,
        &binding.parent_submission,
        stage1_evidence,
    )?;
    if &expected != binding
        || binding.route_id != plan.operation_id
        || binding.intent != plan.intent
        || binding.source_identity != route_identity(&FundingRoute::Core, &plan.intent.owner)
        || binding.destination_identity != plan.destination_identity
    {
        return Err(funding_error(
            "PAPERTRADE_FUNDING_CONTINUATION_MISMATCH",
            "authorization",
            "The continuation binding no longer matches the immutable withdrawal plan and canonical Core credit.",
        ));
    }
    let stage2 = host.reconcile_leg(plan, &plan.legs[1]).await?;
    Ok((stage1, stage2))
}

fn continuation_progress(
    plan: &BoundFunding,
    binding: &BoundFundingContinuation,
    states: &[FundingLegState],
) -> FundingProgress {
    let evidence = serde_json::json!({
        "operation_id": binding.operation_id,
        "route_id": plan.operation_id,
        "stage": 2,
        "legs": [{"ordinal": 1, "status": status_name(&states[1])}],
    });
    let status = if matches!(states[1], FundingLegState::Rejected { .. }) {
        FundingProgressStatus::Rejected
    } else if matches!(states[1], FundingLegState::Unknown { .. }) {
        FundingProgressStatus::Unknown
    } else if !matches!(states[1], FundingLegState::Settled { .. }) {
        FundingProgressStatus::Pending
    } else {
        // The final receipt is added by the async caller after the destination
        // verifier has checked both legs and their causal relationship.
        FundingProgressStatus::Pending
    };
    FundingProgress {
        status,
        receipt: None,
        evidence,
    }
}

async fn finish_continuation<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
    binding: &BoundFundingContinuation,
    states: [FundingLegState; 2],
) -> Result<FundingProgress, GoFinError> {
    let mut progress = continuation_progress(plan, binding, &states);
    if progress.status != FundingProgressStatus::Pending
        || !matches!(states[1], FundingLegState::Settled { .. })
    {
        return Ok(progress);
    }
    let Some(receipt) = host.prove_destination(plan, &states).await? else {
        return Ok(progress);
    };
    let conserved_principal = receipt
        .destination_credit_usdc_atoms
        .checked_add(receipt.actual_cctp_fee_usdc_atoms)
        .and_then(|amount| amount.checked_add(receipt.activation_fee_usdc_atoms));
    let expected_retained = match plan.intent.route {
        FundingRoute::Hyperevm
        | FundingRoute::Ethereum
        | FundingRoute::Arbitrum
        | FundingRoute::Solana => 100_000u64.checked_sub(receipt.actual_source_fee_usdc_atoms),
        FundingRoute::Core => None,
    };
    let native_fee_within_bound = match (
        receipt.actual_native_fee_atoms.as_deref(),
        receipt.actual_native_fee_asset.as_deref(),
        binding.bounds.maximum_native_fee_atoms.as_deref(),
        binding.bounds.native_fee_asset.as_deref(),
    ) {
        (None, None, _, _) => true,
        (Some(actual), Some(asset), Some(maximum), Some(expected_asset))
            if asset == expected_asset =>
        {
            actual
                .parse::<u128>()
                .ok()
                .zip(maximum.parse::<u128>().ok())
                .is_some_and(|(actual, maximum)| actual <= maximum)
        }
        _ => false,
    };
    if receipt.status != "settled"
        || receipt.destination_credit_usdc_atoms < binding.bounds.destination_min_usdc_atoms
        || receipt.destination_credit_usdc_atoms > binding.bounds.source_usdc_atoms
        || receipt.actual_source_fee_usdc_atoms > binding.bounds.maximum_source_fee_usdc_atoms
        || receipt.actual_cctp_fee_usdc_atoms > binding.bounds.maximum_cctp_fee_usdc_atoms
        || receipt.activation_fee_usdc_atoms > binding.bounds.maximum_activation_fee_atoms
        || !native_fee_within_bound
        || conserved_principal != Some(binding.bounds.source_usdc_atoms)
        || receipt.source_debit_usdc_atoms != plan.intent.amount_usdc_atoms
        || expected_retained != Some(receipt.retained_source_credit_usdc_atoms)
        || receipt.source_identity != plan.source_identity
        || receipt.destination_identity != binding.destination_identity
        || receipt.evidence_id.trim().is_empty()
    {
        return Err(funding_error(
            "PAPERTRADE_FUNDING_CONTINUATION_PROOF_MISMATCH",
            "reconciliation",
            "The onward destination proof exceeds the stage-two bounds or differs from its exact route.",
        ));
    }
    // The route verifier reports full stage-one-to-final-route movement. The
    // continuation receipt is deliberately stage-two-only: principal debited
    // from Core, actual costs, and final external credit. The retained reserve
    // was already included in stage-one Core credit and is never credited a
    // second time.
    let mut receipt = receipt;
    receipt.amount_usdc_atoms = receipt.destination_credit_usdc_atoms;
    receipt.source_debit_usdc_atoms = binding.bounds.source_usdc_atoms;
    receipt.retained_source_credit_usdc_atoms = 0;
    receipt.retained_source_native_atoms = None;
    receipt.retained_source_native_asset = None;
    receipt.source_identity = binding.source_identity.clone();
    receipt.destination_identity = binding.destination_identity.clone();
    progress.status = FundingProgressStatus::Settled;
    progress.receipt = Some(receipt);
    Ok(progress)
}

pub(crate) fn continuation_submission_matches(
    binding: &BoundFundingContinuation,
    submission: &gofin_domain::Submission,
) -> bool {
    submission.kind == "papertrade_funding_continuation"
        && submission.reference == binding.operation_id
        && submission.data
            == serde_json::json!({
                "operation_id":binding.operation_id,
                "route_id":binding.route_id,
                "plan_fingerprint":binding.plan_fingerprint,
                "stage1_proof_fingerprint":binding.stage1_proof_fingerprint
            })
}

/// Prepare a second-stage withdrawal binding only after the original
/// Papertrade withdrawal has a canonical, exact Core credit. This function is
/// read-only; the runtime persists the returned binding under its new outer
/// operation identity before authorizing dispatch.
pub async fn prepare_funding_continuation<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
    parent_submission: &gofin_domain::Submission,
    continuation_operation_id: &str,
) -> Result<BoundFundingContinuation, GoFinError> {
    host.assert_persisted_plan(plan).await?;
    if plan.legs.len() != 2 {
        return Err(GoFinError::validation(
            "Protected withdrawal plan does not have exactly one onward leg.",
        ));
    }
    let stage1 = host.reconcile_leg(plan, &plan.legs[0]).await?;
    let evidence = validate_stage1_core_credit(plan, &stage1)?;
    build_continuation_binding(plan, continuation_operation_id, parent_submission, evidence)
}

/// Dispatch exactly the persisted onward leg. Stage one is only reconciled
/// and re-proven; it is never submitted again or included in stage-two debit
/// accounting by this planner.
pub async fn dispatch_funding_continuation<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
    binding: &BoundFundingContinuation,
    outer_submission: &gofin_domain::Submission,
    journal: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
    validated: &gofin_domain::RevalidatedOperation,
) -> Result<FundingProgress, GoFinError> {
    if !continuation_submission_matches(binding, outer_submission) {
        return Err(funding_error(
            "PAPERTRADE_FUNDING_CONTINUATION_SUBMISSION_MISMATCH",
            "authorization",
            "The outer submission does not match this separately authorized withdrawal continuation.",
        ));
    }
    let (stage1, mut stage2) = revalidate_continuation(host, plan, binding).await?;
    if !matches!(stage2, FundingLegState::NotSubmitted) {
        // Existing Submitted/Unknown outcomes are read-only. In particular,
        // stage two never retries a CoreWriter/EVM/Solana write on uncertainty.
        if matches!(
            stage2,
            FundingLegState::Submitted { .. } | FundingLegState::Unknown { .. }
        ) {
            stage2 = host.reconcile_leg(plan, &plan.legs[1]).await?;
        }
        return finish_continuation(host, plan, binding, [stage1, stage2]).await;
    }
    stage2 = host
        .dispatch_leg(&plan.legs[1], outer_submission, journal, validated)
        .await?;
    if matches!(
        stage2,
        FundingLegState::Submitted { .. } | FundingLegState::Unknown { .. }
    ) {
        stage2 = host.reconcile_leg(plan, &plan.legs[1]).await?;
    }
    finish_continuation(host, plan, binding, [stage1, stage2]).await
}

/// Reconcile stage two without dispatching any route leg.
pub async fn reconcile_funding_continuation<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
    binding: &BoundFundingContinuation,
) -> Result<FundingProgress, GoFinError> {
    let (stage1, stage2) = revalidate_continuation(host, plan, binding).await?;
    finish_continuation(host, plan, binding, [stage1, stage2]).await
}

/// Submit only a persisted, definitely-not-submitted leg. Every ambiguous
/// result is reconciled under its original identity and is never replayed.
pub async fn dispatch_funding<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
    outer_submission: &gofin_domain::Submission,
    journal: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
    validated: &gofin_domain::RevalidatedOperation,
) -> Result<FundingProgress, GoFinError> {
    if plan.version != 1 || plan.operation_id.trim().is_empty() || plan.legs.is_empty() {
        return Err(GoFinError::validation(
            "Invalid persisted Papertrade funding plan.",
        ));
    }
    host.assert_persisted_plan(plan).await?;
    validate_plan_registration(plan)?;
    if plan.intent.direction == FundingDirection::Withdraw
        && plan.intent.route != FundingRoute::Core
    {
        let first = plan.legs.first().ok_or_else(|| {
            GoFinError::validation("Persisted Papertrade withdrawal omitted its Core-credit leg.")
        })?;
        if first.ordinal != 0
            || !matches!(
                &first.action,
                FundingAction::PapertradeWithdrawToCore { .. }
            )
            || plan.legs.len() != 2
        {
            return Err(GoFinError::validation(
                "Multi-stage Papertrade withdrawal differs from its immutable two-leg plan.",
            ));
        }
        let mut state = host.reconcile_leg(plan, first).await?;
        if matches!(state, FundingLegState::NotSubmitted) {
            state = host
                .dispatch_leg(first, outer_submission, journal, validated)
                .await?;
        }
        if matches!(
            state,
            FundingLegState::Submitted { .. } | FundingLegState::Unknown { .. }
        ) {
            state = host.reconcile_leg(plan, first).await?;
        }
        // An external continuation is a separate normal authorized operation.
        // Stage one proves only Papertrade cash debit and the owner's Core
        // credit; the onward leg remains untouched until that operation.
        return finish_withdrawal_core_stage(plan, state);
    }
    let mut states = Vec::with_capacity(plan.legs.len());
    for leg in &plan.legs {
        let status = host.reconcile_leg(plan, leg).await?;
        let state = match status {
            FundingLegState::NotSubmitted => {
                host.dispatch_leg(leg, outer_submission, journal.clone(), validated)
                    .await?
            }
            other => other,
        };
        let state = match state {
            FundingLegState::Submitted { .. } | FundingLegState::Unknown { .. } => {
                host.reconcile_leg(plan, leg).await?
            }
            other => other,
        };
        states.push(state);
        if !matches!(states.last(), Some(FundingLegState::Settled { .. })) {
            // Sequential funding actions depend on the prior leg having
            // settled (for example, depositFor cannot follow an uncertain
            // approve). Leave all later durable identities untouched so a
            // subsequent explicitly approved continuation can resume them.
            states.extend(
                plan.legs[states.len()..]
                    .iter()
                    .map(|_| FundingLegState::NotSubmitted),
            );
            break;
        }
    }
    finish_progress(host, plan, states).await
}

/// Read-only funding reconciliation. It never dispatches or prepares a new
/// source transaction and only reports settlement after destination proof.
pub async fn reconcile_funding<H: PapertradeFundingHost>(
    host: &H,
    plan: &BoundFunding,
) -> Result<FundingProgress, GoFinError> {
    if plan.version != 1 || plan.operation_id.trim().is_empty() || plan.legs.is_empty() {
        return Err(GoFinError::validation(
            "Invalid persisted Papertrade funding plan.",
        ));
    }
    host.assert_persisted_plan(plan).await?;
    validate_plan_registration(plan)?;
    if plan.intent.direction == FundingDirection::Withdraw
        && plan.intent.route != FundingRoute::Core
    {
        let first = plan.legs.first().ok_or_else(|| {
            GoFinError::validation("Persisted Papertrade withdrawal omitted its Core-credit leg.")
        })?;
        if first.ordinal != 0
            || !matches!(
                &first.action,
                FundingAction::PapertradeWithdrawToCore { .. }
            )
            || plan.legs.len() != 2
        {
            return Err(GoFinError::validation(
                "Multi-stage Papertrade withdrawal differs from its immutable two-leg plan.",
            ));
        }
        return finish_withdrawal_core_stage(plan, host.reconcile_leg(plan, first).await?);
    }
    let mut states = Vec::with_capacity(plan.legs.len());
    for leg in &plan.legs {
        states.push(host.reconcile_leg(plan, leg).await?);
    }
    finish_progress(host, plan, states).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const OWNER: &str = "0x1234567890123456789012345678901234567890";
    const PROXY: &str = "0x2234567890123456789012345678901234567890";
    const EXCHANGE: &str = gofin_papertrade::EXCHANGE;

    #[test]
    fn funding_runtime_keys_are_preflight_bounded_before_host_work() {
        assert!(validate_funding_storage_keys("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_funding_storage_keys(
            "plan-execution:4ef633d3-5e2e-4cf2-9ea3-1ef193f976fb:ceff6b4d-f0cb-4cec-825c-9cc2b40d47a8:deposit:e7667423-8322-41cf-95b6-df758fecb963"
        )
        .is_ok());
        assert!(validate_funding_storage_keys("step/with/slash").is_err());
        let largest = (0..400)
            .filter(|length| validate_funding_storage_keys(&"x".repeat(*length)).is_ok())
            .max()
            .expect("ordinary operation IDs fit");
        assert!(validate_funding_storage_keys(&"x".repeat(largest + 1)).is_err());
    }

    struct Journal;
    impl gofin_domain::SubmissionJournal for Journal {
        fn assert_authorized(&self) -> Result<(), GoFinError> {
            Ok(())
        }
        fn persist_submission(&self, _: &gofin_domain::Submission) -> Result<(), GoFinError> {
            Ok(())
        }
    }

    fn validated() -> gofin_domain::RevalidatedOperation {
        gofin_domain::RevalidatedOperation::after_revalidation(
            gofin_domain::PreparedOperation {
                operation: "papertrade.deposit".into(),
                input: json!({}),
                binding: json!({"funding_binding":{}}),
                summary: "Papertrade funding test".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: gofin_domain::now(),
                expires_at: "2099-01-01T00:00:00Z".into(),
                valuation: None,
                intent: gofin_domain::FinancialIntent::default(),
                operation_version: 1,
                action_deadline: None,
            },
            std::sync::Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap()
    }

    fn bound_core_withdrawal(owner: &str, amount_atoms: u64) -> gofin_papertrade::BoundIntent {
        gofin_papertrade::BoundIntent {
            typed: gofin_papertrade::PapertradeTypedIntent::WithdrawToCore {
                user: owner.to_ascii_lowercase(),
                amount: amount_atoms.to_string(),
                nonce: "1790000000000".into(),
                deadline: 1_790_000_000,
            },
            intent_id: format!("0x{}", "cc".repeat(32)),
            signer: owner.to_ascii_lowercase(),
            action: Some("withdrawToCore".into()),
            registration_id: None,
            cancel_target_action: None,
        }
    }

    fn word(value: u128) -> String {
        format!("{:064x}", value)
    }
    fn receipt(emitter: &str, topic: &str, atoms: u64) -> gofin_evm::CanonicalReceipt {
        let wad = u128::from(atoms) * 1_000_000_000_000;
        let data = if topic == CORE_WITHDRAWAL_TOPIC {
            format!("0x{}{}", word(wad), word(u128::from(atoms) * 100))
        } else {
            format!("0x{}", word(wad))
        };
        gofin_evm::CanonicalReceipt {
            hash: gofin_evm::hash(
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .unwrap(),
            transaction_sender: OWNER.into(),
            block_hash: gofin_evm::hash(
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap(),
            block_number: 10,
            confirmations: 5,
            success: true,
            fee: gofin_evm::alloy::primitives::U256::from(1),
            receipt: json!({"logs":[{
                "address":emitter,
                "topics":[topic,event_address_topic(OWNER).unwrap()],
                "data":data
            }]}),
        }
    }

    #[test]
    fn factory_proxy_call_and_return_are_exact_abi_words() {
        let call = compute_proxy_address_call(OWNER).unwrap();
        assert_eq!(call.chain_id, 999);
        assert_eq!(call.to, PAPERTRADE_PROXY_FACTORY);
        assert_eq!(call.value.to_string(), "0");
        assert_eq!(
            call.data,
            format!("0xd600539a{}{}", "00".repeat(12), &OWNER[2..])
        );

        let result = format!("0x{}{}", "00".repeat(12), &PROXY[2..]);
        assert_eq!(decode_proxy_address_result(&result).unwrap(), PROXY);
        assert!(decode_proxy_address_result("0x").is_err());
        assert!(decode_proxy_address_result(&format!("0x{}", "00".repeat(32))).is_err());
        assert!(
            decode_proxy_address_result(&format!("0x{}{}", "01".repeat(12), &PROXY[2..])).is_err()
        );
    }

    struct ReplayHost {
        dispatches: AtomicUsize,
        reconciliations: AtomicUsize,
        dispatched_ordinal: AtomicUsize,
        reconcile_states: std::sync::Mutex<std::collections::VecDeque<FundingLegState>>,
    }

    #[async_trait::async_trait]
    impl PapertradeFundingHost for ReplayHost {
        async fn preflight(&self, _: &FundingIntent) -> Result<FundingPreflight, GoFinError> {
            unreachable!()
        }
        async fn ensure_proxy_registered(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<ProxyRegistrationProof, GoFinError> {
            unreachable!()
        }
        async fn cctp_quote(&self, _: &CctpQuoteRequest) -> Result<FundingCctpQuote, GoFinError> {
            unreachable!()
        }
        async fn prepare_evm_call(
            &self,
            _: &str,
            _: &FundingIntent,
            _: &EvmCall,
            _: &str,
        ) -> Result<gofin_evm::FrozenTransaction, GoFinError> {
            unreachable!()
        }
        async fn solana_burn_request(
            &self,
            _: &str,
            _: &FundingIntent,
            _: &cctp::CctpQuoteMetadata,
            _: &str,
        ) -> Result<cctp::solana::hypercore_source::BurnRequest, GoFinError> {
            unreachable!()
        }
        async fn prepare_solana_burn_transaction(
            &self,
            _: &str,
            _: &NormalizedInstruction,
            _: u64,
        ) -> Result<gofin_papertrade::SolanaFundingTransactionBinding, GoFinError> {
            unreachable!()
        }
        async fn reserve_hyperliquid_nonce(&self, _: &str) -> Result<u64, GoFinError> {
            unreachable!()
        }
        async fn papertrade_withdraw_to_core_intent(
            &self,
            _: &str,
            _: &FundingIntent,
        ) -> Result<gofin_papertrade::BoundIntent, GoFinError> {
            unreachable!()
        }
        async fn persist_plan(&self, _: &BoundFunding) -> Result<(), GoFinError> {
            unreachable!()
        }
        async fn assert_persisted_plan(&self, _: &BoundFunding) -> Result<(), GoFinError> {
            Ok(())
        }
        async fn dispatch_leg(
            &self,
            leg: &FundingLeg,
            _: &gofin_domain::Submission,
            _: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
            _: &gofin_domain::RevalidatedOperation,
        ) -> Result<FundingLegState, GoFinError> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            self.dispatched_ordinal
                .store(usize::from(leg.ordinal), Ordering::SeqCst);
            Ok(FundingLegState::Submitted {
                evidence: json!({"tx":"original"}),
            })
        }
        async fn reconcile_leg(
            &self,
            _: &BoundFunding,
            _: &FundingLeg,
        ) -> Result<FundingLegState, GoFinError> {
            self.reconciliations.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .reconcile_states
                .lock()
                .expect("test reconcile state lock")
                .pop_front()
                .unwrap_or_else(|| FundingLegState::Unknown {
                    evidence: json!({"reason":"receipt unavailable"}),
                }))
        }
        async fn prove_destination(
            &self,
            _: &BoundFunding,
            _: &[FundingLegState],
        ) -> Result<Option<FundingReceipt>, GoFinError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn uncertain_submission_is_reconciled_without_replaying_the_write() {
        let host = ReplayHost {
            dispatches: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            dispatched_ordinal: AtomicUsize::new(usize::MAX),
            reconcile_states: std::sync::Mutex::new(
                [
                    FundingLegState::NotSubmitted,
                    FundingLegState::Unknown {
                        evidence: json!({"reason":"receipt unavailable"}),
                    },
                ]
                .into(),
            ),
        };
        let plan = BoundFunding {
            version: 1,
            operation_id: "op-1".into(),
            intent: FundingIntent {
                direction: FundingDirection::Deposit,
                route: FundingRoute::Core,
                owner: OWNER.into(),
                amount_usdc_atoms: 1_000_000,
                request_id: "request-1".into(),
            },
            proxy: PROXY.into(),
            source_identity: format!(
                "hyperliquid:mainnet:{}:spot:primary",
                OWNER.to_ascii_lowercase()
            ),
            destination_identity: format!("papertrade:mainnet:{}", OWNER.to_ascii_lowercase()),
            minimum_received_usdc_atoms: 1_000_000,
            bounds: gofin_papertrade::FundingFinancialBounds {
                source_usdc_atoms: 1_000_000,
                maximum_source_fee_usdc_atoms: 0,
                destination_min_usdc_atoms: 1_000_000,
                maximum_cctp_fee_usdc_atoms: 0,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
            cctp_quote: None,
            proxy_core_usdc_before_atoms: Some(0),
            core_owner_usdc_before_atoms: Some(1_000_000),
            core_sweep_from_block: Some(1),
            proxy_registration: Some(ProxyRegistrationProof {
                owner: OWNER.into(),
                api_proxy: PROXY.into(),
                factory_proxy: PROXY.into(),
                request_id: "papertrade-proxy-check:op-1".into(),
            }),
            legs: vec![FundingLeg {
                ordinal: 0,
                submission_id: "papertrade-funding:op-1:0".into(),
                action: FundingAction::EvmCall {
                    call: EvmCall {
                        chain_id: 999,
                        to: PROXY.into(),
                        value: gofin_domain::Quantity::from_u64(0),
                        data: "0x".into(),
                    },
                    transaction: gofin_evm::FrozenTransaction {
                        chain_id: 999,
                        sender: OWNER.into(),
                        to: PROXY.into(),
                        value: gofin_domain::Quantity::from_u64(0),
                        calldata: "0x".into(),
                        nonce: 0,
                        gas: 21_000,
                        max_fee_per_gas: gofin_domain::Quantity::from_u64(1),
                        max_priority_fee_per_gas: gofin_domain::Quantity::from_u64(1),
                        rollup_fee: None,
                    },
                },
            }],
        };
        let progress = dispatch_funding(
            &host,
            &plan,
            &gofin_domain::Submission {
                kind: "papertrade_funding".into(),
                reference: "op-1".into(),
                data: json!({"operation_id":"op-1"}),
            },
            std::sync::Arc::new(Journal),
            &validated(),
        )
        .await
        .unwrap();
        assert_eq!(progress.status, FundingProgressStatus::Unknown);
        assert!(progress.receipt.is_none());
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(host.reconciliations.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn external_withdrawal_dispatch_settles_only_core_credit_stage() {
        let amount = 20_000_000u64;
        let stage1_evidence = json!({
            "kind":"canonical_papertrade_core_withdrawal",
            "owner":OWNER.to_ascii_lowercase(),
            "received_usdc_atoms":amount,
            "transaction_hash":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "core_credit_transaction_hash":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        });
        let host = ReplayHost {
            dispatches: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            dispatched_ordinal: AtomicUsize::new(usize::MAX),
            reconcile_states: std::sync::Mutex::new(
                [
                    FundingLegState::NotSubmitted,
                    FundingLegState::Settled {
                        evidence: stage1_evidence,
                    },
                ]
                .into(),
            ),
        };
        let plan = BoundFunding {
            version: 1,
            operation_id: "route-core-stage".into(),
            intent: FundingIntent {
                direction: FundingDirection::Withdraw,
                route: FundingRoute::Ethereum,
                owner: OWNER.into(),
                amount_usdc_atoms: amount,
                request_id: "route-core-stage".into(),
            },
            proxy: PROXY.into(),
            source_identity: format!("papertrade:mainnet:{}", OWNER.to_ascii_lowercase()),
            destination_identity: format!("ethereum:1:{}", OWNER.to_ascii_lowercase()),
            minimum_received_usdc_atoms: 19_700_000,
            bounds: gofin_papertrade::FundingFinancialBounds {
                source_usdc_atoms: amount,
                maximum_source_fee_usdc_atoms: 100_000,
                destination_min_usdc_atoms: 19_700_000,
                maximum_cctp_fee_usdc_atoms: 200_000,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
            cctp_quote: None,
            proxy_core_usdc_before_atoms: None,
            core_owner_usdc_before_atoms: None,
            core_sweep_from_block: None,
            proxy_registration: None,
            legs: vec![
                FundingLeg {
                    ordinal: 0,
                    submission_id: "papertrade-funding:route-core-stage:0".into(),
                    action: FundingAction::PapertradeWithdrawToCore {
                        bound: bound_core_withdrawal(OWNER, amount),
                    },
                },
                FundingLeg {
                    ordinal: 1,
                    submission_id: "papertrade-funding:route-core-stage:1".into(),
                    action: FundingAction::HyperCoreCctpWithdraw {
                        action: cctp_withdrawal_action(
                            gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
                            "19.9",
                            200_000,
                            102,
                            CctpWithdrawalTarget::Ethereum { recipient: OWNER },
                        )
                        .unwrap(),
                    },
                },
            ],
        };
        let progress = dispatch_funding(
            &host,
            &plan,
            &gofin_domain::Submission {
                kind: "papertrade_funding".into(),
                reference: plan.operation_id.clone(),
                data: json!({"operation_id":plan.operation_id}),
            },
            std::sync::Arc::new(Journal),
            &validated(),
        )
        .await
        .unwrap();

        assert_eq!(progress.status, FundingProgressStatus::Settled);
        let receipt = progress.receipt.expect("stage-one Core credit proof");
        assert_eq!(receipt.stage, FundingReceiptStage::CoreCredit);
        assert_eq!(receipt.source_debit_usdc_atoms, amount);
        assert_eq!(receipt.destination_credit_usdc_atoms, amount);
        assert_eq!(
            receipt.destination_identity,
            format!(
                "hyperliquid:mainnet:{}:spot:primary",
                OWNER.to_ascii_lowercase()
            )
        );
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(host.dispatched_ordinal.load(Ordering::SeqCst), 0);
        assert_eq!(host.reconciliations.load(Ordering::SeqCst), 2);
        assert_eq!(progress.evidence["legs"][1]["status"], "not_submitted");
    }

    #[tokio::test]
    async fn continuation_dispatches_only_onward_leg_after_exact_stage_one_proof() {
        let amount = 20_000_000u64;
        let stage2_action = cctp_withdrawal_action(
            gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
            "19.9",
            200_000,
            102,
            CctpWithdrawalTarget::Ethereum { recipient: OWNER },
        )
        .unwrap();
        let plan = BoundFunding {
            version: 1,
            operation_id: "route-1".into(),
            intent: FundingIntent {
                direction: FundingDirection::Withdraw,
                route: FundingRoute::Ethereum,
                owner: OWNER.into(),
                amount_usdc_atoms: amount,
                request_id: "withdraw-1".into(),
            },
            proxy: PROXY.into(),
            source_identity: format!("papertrade:mainnet:{}", OWNER.to_ascii_lowercase()),
            destination_identity: format!("ethereum:1:{}", OWNER.to_ascii_lowercase()),
            minimum_received_usdc_atoms: 19_700_000,
            bounds: gofin_papertrade::FundingFinancialBounds {
                source_usdc_atoms: amount,
                maximum_source_fee_usdc_atoms: 100_000,
                destination_min_usdc_atoms: 19_700_000,
                maximum_cctp_fee_usdc_atoms: 200_000,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
            cctp_quote: None,
            proxy_core_usdc_before_atoms: None,
            core_owner_usdc_before_atoms: None,
            core_sweep_from_block: None,
            proxy_registration: None,
            legs: vec![
                FundingLeg {
                    ordinal: 0,
                    submission_id: "papertrade-funding:route-1:0".into(),
                    action: FundingAction::PapertradeWithdrawToCore {
                        bound: bound_core_withdrawal(OWNER, amount),
                    },
                },
                FundingLeg {
                    ordinal: 1,
                    submission_id: "papertrade-funding:route-1:1".into(),
                    action: FundingAction::HyperCoreCctpWithdraw {
                        action: stage2_action,
                    },
                },
            ],
        };
        let parent = gofin_domain::Submission {
            kind: "papertrade_funding".into(),
            reference: "route-1".into(),
            data: json!({"operation_id":"route-1"}),
        };
        let stage1_evidence = json!({
            "kind":"canonical_papertrade_core_withdrawal",
            "owner":OWNER.to_ascii_lowercase(),
            "received_usdc_atoms":amount,
            "transaction_hash":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "core_credit_transaction_hash":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        });
        let binding =
            build_continuation_binding(&plan, "continuation-1", &parent, &stage1_evidence).unwrap();
        let host = ReplayHost {
            dispatches: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            dispatched_ordinal: AtomicUsize::new(usize::MAX),
            reconcile_states: std::sync::Mutex::new(
                [
                    FundingLegState::Settled {
                        evidence: stage1_evidence,
                    },
                    FundingLegState::NotSubmitted,
                    FundingLegState::Unknown {
                        evidence: json!({"reason":"destination receipt unavailable"}),
                    },
                ]
                .into(),
            ),
        };
        let progress = dispatch_funding_continuation(
            &host,
            &plan,
            &binding,
            &gofin_domain::Submission {
                kind: "papertrade_funding_continuation".into(),
                reference: "continuation-1".into(),
                data: json!({
                    "operation_id":binding.operation_id,
                    "route_id":binding.route_id,
                    "plan_fingerprint":binding.plan_fingerprint,
                    "stage1_proof_fingerprint":binding.stage1_proof_fingerprint
                }),
            },
            std::sync::Arc::new(Journal),
            &validated(),
        )
        .await
        .unwrap();
        assert_eq!(progress.status, FundingProgressStatus::Unknown);
        assert_eq!(host.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(host.dispatched_ordinal.load(Ordering::SeqCst), 1);
        assert_eq!(host.reconciliations.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn continuation_binding_is_core_principal_only_and_keeps_stage_one_immutable() {
        let amount = 20_000_000u64;
        let stage2_action = cctp_withdrawal_action(
            gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
            "19.9",
            200_000,
            102,
            CctpWithdrawalTarget::Ethereum { recipient: OWNER },
        )
        .unwrap();
        let plan = BoundFunding {
            version: 1,
            operation_id: "route-1".into(),
            intent: FundingIntent {
                direction: FundingDirection::Withdraw,
                route: FundingRoute::Ethereum,
                owner: OWNER.into(),
                amount_usdc_atoms: amount,
                request_id: "withdraw-1".into(),
            },
            proxy: PROXY.into(),
            source_identity: format!("papertrade:mainnet:{}", OWNER.to_ascii_lowercase()),
            destination_identity: format!("ethereum:1:{}", OWNER.to_ascii_lowercase()),
            minimum_received_usdc_atoms: 19_700_000,
            bounds: gofin_papertrade::FundingFinancialBounds {
                source_usdc_atoms: amount,
                maximum_source_fee_usdc_atoms: 100_000,
                destination_min_usdc_atoms: 19_700_000,
                maximum_cctp_fee_usdc_atoms: 200_000,
                maximum_activation_fee_atoms: 0,
                native_fee_asset: None,
                maximum_native_fee_atoms: None,
                maximum_refundable_native_funding_atoms: None,
                maximum_native_source_debit_atoms: None,
            },
            cctp_quote: None,
            proxy_core_usdc_before_atoms: None,
            core_owner_usdc_before_atoms: None,
            core_sweep_from_block: None,
            proxy_registration: None,
            legs: vec![
                FundingLeg {
                    ordinal: 0,
                    submission_id: "papertrade-funding:route-1:0".into(),
                    action: FundingAction::PapertradeWithdrawToCore {
                        bound: bound_core_withdrawal(OWNER, amount),
                    },
                },
                FundingLeg {
                    ordinal: 1,
                    submission_id: "papertrade-funding:route-1:1".into(),
                    action: FundingAction::HyperCoreCctpWithdraw {
                        action: stage2_action,
                    },
                },
            ],
        };
        let parent = gofin_domain::Submission {
            kind: "papertrade_funding".into(),
            reference: "route-1".into(),
            data: json!({"operation_id":"route-1"}),
        };
        let stage1_proof = json!({
            "kind":"canonical_papertrade_core_withdrawal",
            "owner":OWNER.to_ascii_lowercase(),
            "received_usdc_atoms":amount,
            "transaction_hash":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "core_credit_transaction_hash":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        });
        let binding =
            build_continuation_binding(&plan, "continuation-1", &parent, &stage1_proof).unwrap();
        assert_eq!(binding.bounds.source_usdc_atoms, 19_900_000);
        assert_eq!(binding.bounds.destination_min_usdc_atoms, 19_700_000);
        assert_eq!(binding.bounds.maximum_source_fee_usdc_atoms, 100_000);
        assert_eq!(
            binding.source_identity,
            format!(
                "hyperliquid:mainnet:{}:spot:primary",
                OWNER.to_ascii_lowercase()
            )
        );
        assert_eq!(binding.destination_identity, plan.destination_identity);
        assert_eq!(plan.bounds.source_usdc_atoms, amount);
        assert_eq!(
            plan.source_identity,
            format!("papertrade:mainnet:{}", OWNER.to_ascii_lowercase())
        );

        // The same 0.1 USDC reserve applies to the same-owner HyperEVM
        // continuation. Its EVM send principal is gross minus reserve, and
        // any separately charged Core send fee is bounded by that reserve.
        let mut hyperevm_plan = plan.clone();
        hyperevm_plan.intent.route = FundingRoute::Hyperevm;
        hyperevm_plan.destination_identity = format!("ethereum:999:{}", OWNER.to_ascii_lowercase());
        hyperevm_plan.minimum_received_usdc_atoms = 19_900_000;
        hyperevm_plan.bounds.destination_min_usdc_atoms = 19_900_000;
        hyperevm_plan.bounds.maximum_cctp_fee_usdc_atoms = 0;
        hyperevm_plan.bounds.native_fee_asset = Some("hyperliquid:mainnet:HYPE".into());
        hyperevm_plan.bounds.maximum_native_fee_atoms = Some("100000".into());
        hyperevm_plan.legs[1].action = FundingAction::HyperCoreSendAsset {
            action: same_owner_hyperevm_send_asset_action(
                gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
                "Mainnet",
                OWNER,
                OWNER,
                "19.9",
                103,
            )
            .unwrap(),
        };
        let hype_binding = build_continuation_binding(
            &hyperevm_plan,
            "hyperevm-continuation-1",
            &parent,
            &stage1_proof,
        )
        .unwrap();
        assert_eq!(hype_binding.bounds.source_usdc_atoms, 19_900_000);
        assert_eq!(hype_binding.bounds.destination_min_usdc_atoms, 19_900_000);
        assert_eq!(hype_binding.bounds.maximum_source_fee_usdc_atoms, 100_000);
        assert_eq!(
            hype_binding.bounds.native_fee_asset.as_deref(),
            Some("hyperliquid:mainnet:HYPE")
        );
        assert_eq!(
            hype_binding.bounds.maximum_native_fee_atoms.as_deref(),
            Some("100000")
        );
        let public_hype = gofin_papertrade::FundingContinuationBinding {
            version: hype_binding.version,
            operation_id: hype_binding.operation_id.clone(),
            route_id: hype_binding.route_id.clone(),
            parent_submission: hype_binding.parent_submission.clone(),
            plan_fingerprint: hype_binding.plan_fingerprint.clone(),
            stage1_proof_fingerprint: hype_binding.stage1_proof_fingerprint.clone(),
            intent: hype_binding.intent.clone(),
            source_identity: hype_binding.source_identity.clone(),
            destination_identity: hype_binding.destination_identity.clone(),
            bounds: hype_binding.bounds.clone(),
        };
        public_hype.validate().unwrap();
        hyperevm_plan.legs[1].action = FundingAction::HyperCoreSendAsset {
            action: same_owner_hyperevm_send_asset_action(
                gofin_hyperliquid::funding::HYPEREVM_SIGNATURE_CHAIN_ID,
                "Mainnet",
                OWNER,
                OWNER,
                "20",
                103,
            )
            .unwrap(),
        };
        assert!(
            build_continuation_binding(
                &hyperevm_plan,
                "hyperevm-continuation-1",
                &parent,
                &stage1_proof,
            )
            .is_err()
        );
        let public = gofin_papertrade::FundingContinuationBinding {
            version: binding.version,
            operation_id: binding.operation_id.clone(),
            route_id: binding.route_id.clone(),
            parent_submission: binding.parent_submission.clone(),
            plan_fingerprint: binding.plan_fingerprint.clone(),
            stage1_proof_fingerprint: binding.stage1_proof_fingerprint.clone(),
            intent: binding.intent.clone(),
            source_identity: binding.source_identity.clone(),
            destination_identity: binding.destination_identity.clone(),
            bounds: binding.bounds.clone(),
        };
        public.validate().unwrap();
    }

    #[test]
    fn swept_event_proves_only_exact_proxy_owner_and_internal_credit() {
        let r = receipt(EXCHANGE, SWEPT_TOPIC, 1_234_567);
        let proof = verify_swept_event(&r, EXCHANGE, OWNER, 1_234_567).unwrap();
        assert_eq!(proof.usdc_atoms, 1_234_567);
        assert_eq!(proof.internal_usdc_wad, "1234567000000000000");
        assert!(verify_swept_event(&r, PROXY, OWNER, 1_234_567).is_err());
        assert!(verify_swept_event(&r, EXCHANGE, OWNER, 1_234_568).is_err());
        assert!(
            verify_swept_event(
                &r,
                EXCHANGE,
                "0x4234567890123456789012345678901234567890",
                1_234_567
            )
            .is_err()
        );
    }

    #[test]
    fn core_withdrawal_event_proves_internal_debit_and_exact_core_amount() {
        let r = receipt(EXCHANGE, CORE_WITHDRAWAL_TOPIC, 2_000_000);
        let proof = verify_core_withdrawal_event(&r, EXCHANGE, OWNER, 2_000_000).unwrap();
        assert_eq!(proof.usdc_atoms, 2_000_000);
        assert!(verify_core_withdrawal_event(&r, EXCHANGE, OWNER, 2_000_001).is_err());
        let mut reverted = r.clone();
        reverted.success = false;
        assert!(verify_core_withdrawal_event(&reverted, EXCHANGE, OWNER, 2_000_000).is_err());
    }

    #[test]
    fn deposit_net_must_keep_sweep_minimum_but_withdrawal_net_may_be_lower() {
        assert_eq!(
            net_funding_credit_atoms(11_000_000, 1_000_000, 0, MIN_PAPERTRADE_FUNDING_USDC_ATOMS)
                .unwrap(),
            10_000_000
        );
        assert!(
            net_funding_credit_atoms(10_000_000, 1, 0, MIN_PAPERTRADE_FUNDING_USDC_ATOMS).is_err()
        );
        assert_eq!(
            net_funding_credit_atoms(10_000_000, 1, 0, 1).unwrap(),
            9_999_999
        );
    }
}
