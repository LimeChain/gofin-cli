//! Protocol-neutral contract shared by the Meteora DLMM and DAMM v2 modules.
//!
//! Protocol implementations discover and validate canonical program state and
//! produce an instruction candidate. The adapter owns authorization, freezing,
//! simulation, journaling, dispatch, recovery, and finalized accounting.

use async_trait::async_trait;
use gofin_domain::{GoFinError, Result, SignedQuantity};
use gofin_solana::{FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFamily {
    Dlmm,
    DammV2,
}

impl ProtocolFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dlmm => "dlmm",
            Self::DammV2 => "damm_v2",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeteoraAction {
    Swap,
    Open,
    Add,
    Remove,
    Collect,
    Close,
}

/// Exact user request. Quantities are decimal base-unit strings and are never
/// converted through floating point. Protocol-specific bin/distribution data
/// stays typed as JSON here and is strictly validated by its owning module.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MeteoraIntent {
    /// Enrolled connection alias. The adapter resolves it to `wallet_address`
    /// before invoking a protocol module; modules never infer an address from
    /// this display name.
    pub wallet: String,
    pub family: ProtocolFamily,
    pub action: MeteoraAction,
    pub pool: String,
    #[serde(default)]
    pub position: Option<String>,
    #[serde(default)]
    pub input_mint: Option<String>,
    /// Explicit token output identity. `native` requests a temporary WSOL
    /// account that is unwrapped only when the transaction created it.
    #[serde(default)]
    pub output_mint: Option<String>,
    /// Explicit pool-side identities for all liquidity lifecycle actions.
    #[serde(default)]
    pub token_a_mint: Option<String>,
    #[serde(default)]
    pub token_b_mint: Option<String>,
    #[serde(default)]
    pub amount_atoms: Option<String>,
    #[serde(default)]
    pub minimum_output_atoms: Option<String>,
    #[serde(default)]
    pub token_a_atoms: Option<String>,
    #[serde(default)]
    pub token_b_atoms: Option<String>,
    #[serde(default)]
    pub minimum_token_a_atoms: Option<String>,
    #[serde(default)]
    pub minimum_token_b_atoms: Option<String>,
    #[serde(default)]
    pub remove_share_bps: Option<u16>,
    #[serde(default)]
    pub bin_lower: Option<i32>,
    #[serde(default)]
    pub bin_upper: Option<i32>,
    #[serde(default)]
    pub liquidity_distribution: Option<Value>,
    pub maximum_slippage_bps: u16,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtocolBuildContext {
    /// Exact wallet public key selected by the protected broker.
    pub wallet_address: String,
    /// Stable public identity for a broker-owned position mint signer on open.
    /// The private signer is never included in this context or serialized.
    #[serde(default)]
    pub position_signer_address: Option<String>,
    /// Stable operation-bound keypair for a temporary wrapped-SOL account.
    #[serde(default)]
    pub temporary_wsol_signer_address: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenInfo {
    pub mint: String,
    pub decimals: u8,
    pub token_program: String,
    #[serde(default)]
    pub transfer_fee: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PoolInfo {
    pub family: ProtocolFamily,
    pub address: String,
    pub token_a: TokenInfo,
    pub token_b: TokenInfo,
    pub active: bool,
    #[serde(default)]
    pub current_bin: Option<i32>,
    #[serde(default)]
    pub fee_bps: Option<u32>,
    /// Canonical, protocol-owned fields needed to bind a preview to dispatch.
    #[serde(default)]
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PoolQuery {
    #[serde(default)]
    pub search: Option<String>,
    pub limit: u16,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub token_a_mint: Option<String>,
    #[serde(default)]
    pub token_b_mint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PoolPage {
    pub pools: Vec<PoolInfo>,
    pub complete: bool,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionInfo {
    pub family: ProtocolFamily,
    pub address: String,
    pub pool: String,
    pub owner: String,
    /// RPC context slot for this position account observation, when the
    /// provider returned one. This is distinct from the transaction slot.
    #[serde(default)]
    pub observation_slot: Option<u64>,
    /// Accounts whose lamports are refundable to this position owner when
    /// the protocol closes the position. Shared/permanent bin arrays do not
    /// belong here.
    #[serde(default)]
    pub rent_accounts: Vec<PositionRentAccount>,
    pub token_a: TokenInfo,
    pub token_b: TokenInfo,
    #[serde(default)]
    pub receipt_mint: Option<String>,
    #[serde(default)]
    pub receipt_atoms: Option<String>,
    /// Pool liquidity units owned by this position, independent of the
    /// current underlying-token composition and receipt NFT identity.
    #[serde(default)]
    pub liquidity_units_atoms: Option<String>,
    #[serde(default)]
    pub token_a_atoms: Option<String>,
    #[serde(default)]
    pub token_b_atoms: Option<String>,
    #[serde(default)]
    pub fees_a_atoms: Option<String>,
    #[serde(default)]
    pub fees_b_atoms: Option<String>,
    #[serde(default)]
    pub rewards: Vec<PositionReward>,
    #[serde(default)]
    pub bin_lower: Option<i32>,
    #[serde(default)]
    pub bin_upper: Option<i32>,
    #[serde(default)]
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionRentAccount {
    pub account: String,
    pub lamports: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionInventory {
    pub positions: Vec<PositionInfo>,
    /// `true` only when the owning protocol module completed the full wallet
    /// query and all pages for the requested identity.
    pub complete: bool,
    pub observed_at: String,
    pub coverage: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionReward {
    pub mint: String,
    pub decimals: u8,
    pub token_program: String,
    pub amount_atoms: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuoteInfo {
    pub family: ProtocolFamily,
    pub action: MeteoraAction,
    pub pool: String,
    #[serde(default)]
    pub position: Option<String>,
    #[serde(default)]
    pub token_a_debit_atoms: Option<String>,
    #[serde(default)]
    pub token_b_debit_atoms: Option<String>,
    #[serde(default)]
    pub token_a_credit_atoms: Option<String>,
    #[serde(default)]
    pub token_b_credit_atoms: Option<String>,
    #[serde(default)]
    pub receipt_delta_atoms: Option<String>,
    #[serde(default)]
    pub fees_a_atoms: Option<String>,
    #[serde(default)]
    pub fees_b_atoms: Option<String>,
    #[serde(default)]
    pub rewards: Vec<PositionReward>,
    pub expires_at: String,
    #[serde(default)]
    pub price_impact_bps: Option<u32>,
    /// Exact wallet native-SOL change from a native/WSOL swap leg, excluding
    /// network fees and separately classified temporary-account rent.
    #[serde(default)]
    pub native_cash_delta: Option<String>,
    #[serde(default)]
    pub protocol_details: Value,
}

/// Frozen-ready proposal from a protocol module. `quote` and the immutable pool
/// and position identities are copied into prepared state and rechecked before
/// dispatch. `stable_position_address` must never be regenerated on recovery.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtocolCandidate {
    pub quote: QuoteInfo,
    pub instructions: Vec<NormalizedInstruction>,
    #[serde(default)]
    pub lookup_tables: Vec<String>,
    pub blockhash: String,
    pub last_valid_block_height: u64,
    #[serde(default)]
    pub stable_position_address: Option<String>,
    #[serde(default)]
    pub required_secondary_signers: Vec<String>,
    #[serde(default)]
    pub metadata: Value,
}

/// Fail closed before unsigned protocol probes unless every required signer
/// is the primary wallet or one of the exact broker-bound auxiliary identities
/// supplied in the build context. `sigVerify:false` simulations otherwise
/// treat unknown zeroed signature slots as if they were signed.
pub fn validate_candidate_signer_scope(
    candidate: &ProtocolCandidate,
    context: &ProtocolBuildContext,
) -> Result<()> {
    let expected_secondary = context
        .position_signer_address
        .iter()
        .chain(context.temporary_wsol_signer_address.iter())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let declared_secondary = candidate
        .required_secondary_signers
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if declared_secondary.len() != candidate.required_secondary_signers.len()
        || declared_secondary != expected_secondary
    {
        return Err(GoFinError::validation(
            "Meteora candidate signer identities do not match the broker-bound auxiliary signers.",
        ));
    }
    let mut actual = std::collections::BTreeSet::from([context.wallet_address.clone()]);
    for signer in candidate
        .instructions
        .iter()
        .flat_map(|instruction| &instruction.accounts)
        .filter(|account| account.signer)
    {
        actual.insert(signer.address.clone());
    }
    let expected = std::iter::once(context.wallet_address.clone())
        .chain(expected_secondary)
        .collect::<std::collections::BTreeSet<_>>();
    if actual != expected {
        return Err(GoFinError::validation(
            "Meteora instruction graph contains an unknown or unused signer identity.",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedTokenDelta {
    pub mint: String,
    pub decimals: u8,
    pub token_program: String,
    /// Post minus pre wallet-owned atoms, strictly decoded from the finalized
    /// receipt. Positive is a credit; negative is a debit.
    pub atoms: SignedQuantity,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedRentAccountDelta {
    /// Exact rent-bearing protocol/associated account from the finalized tx.
    pub account: String,
    /// Stable shared accounting identity: `account-funding:<account>`.
    pub position: String,
    /// Account lamports post minus pre. Positive is refundable rent funded;
    /// negative is rent returned when the account is closed.
    pub atoms: SignedQuantity,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedProtocolEffects {
    pub token_deltas: Vec<VerifiedTokenDelta>,
    /// Wallet lamport change from native SOL swap amount only, excluding
    /// transaction fee and the separately classified rent accounts.
    #[serde(default)]
    pub native_cash_delta: Option<SignedQuantity>,
    /// Positive wallet credits separately classified by exact protocol event.
    #[serde(default)]
    pub collected_fees: Vec<VerifiedTokenDelta>,
    #[serde(default)]
    pub collected_rewards: Vec<VerifiedTokenDelta>,
    /// Transaction-scoped position backing changes, equal to the inverse of
    /// the finalized wallet cashflow for the pool mints: a wallet debit is a
    /// positive contribution and a wallet credit is a negative withdrawal.
    /// These are separate from current A/B composition observation.
    #[serde(default)]
    pub position_asset_deltas: Vec<VerifiedTokenDelta>,
    /// Exact rent-bearing account changes. This is separate from transaction
    /// fees and token principal, and must name accounts pinned by the candidate.
    #[serde(default)]
    pub rent_accounts: Vec<VerifiedRentAccountDelta>,
    /// Transaction-scoped receipt/liquidity-unit change from a validated
    /// protocol event. Current position receipt quantity remains observation
    /// metadata only.
    #[serde(default)]
    pub receipt_delta_atoms: Option<SignedQuantity>,
    /// Latest protocol-owned state observed at or after the finalized slot.
    /// This is observation only, not a delta attributable to this transaction.
    #[serde(default)]
    pub position: Option<PositionInfo>,
    pub rent_lamports_delta: SignedQuantity,
    pub evidence: Value,
}

#[async_trait]
pub trait MeteoraProtocol: Send + Sync {
    fn family(&self) -> ProtocolFamily;

    async fn pools(&self, runtime: &SolanaRuntime, query: &PoolQuery) -> Result<PoolPage>;
    async fn pool(&self, runtime: &SolanaRuntime, address: &str) -> Result<PoolInfo>;
    async fn positions(&self, runtime: &SolanaRuntime, owner: &str) -> Result<PositionInventory>;
    async fn position(&self, runtime: &SolanaRuntime, address: &str) -> Result<PositionInfo>;
    async fn quote(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<QuoteInfo>;
    async fn quote_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        _candidate: &ProtocolCandidate,
    ) -> Result<QuoteInfo> {
        self.quote(runtime, intent, context).await
    }
    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<ProtocolCandidate>;
    /// Build an independently validated candidate carrying quote evidence.
    /// Protocols whose quote requires simulation of the exact candidate may
    /// override this to defer that simulation until after candidate validation.
    async fn prepare_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<ProtocolCandidate> {
        let quote = self.quote(runtime, intent, context).await?;
        let mut candidate = self.build_candidate(runtime, intent, context).await?;
        candidate.quote = quote;
        validate_candidate_family(self.family(), intent, &candidate)?;
        self.validate_candidate(runtime, intent, context, &candidate)
            .await?;
        Ok(candidate)
    }
    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
    ) -> Result<()>;
    async fn verify_finalized(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
        proof: &FinalizedReceiptProof,
    ) -> Result<VerifiedProtocolEffects>;
}

pub fn validate_candidate_family(
    family: ProtocolFamily,
    intent: &MeteoraIntent,
    candidate: &ProtocolCandidate,
) -> Result<()> {
    if family != intent.family
        || family != candidate.quote.family
        || intent.action != candidate.quote.action
        || intent.pool != candidate.quote.pool
        || candidate.instructions.is_empty()
        || candidate.last_valid_block_height == 0
    {
        return Err(GoFinError::validation(
            "The Meteora protocol candidate does not match the exact requested family, action, pool, and finite transaction lifetime.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod signer_scope_tests {
    use super::*;
    use gofin_solana::transaction::{NormalizedAccountMeta, NormalizedInstruction};

    fn candidate(signers: &[(&str, bool)]) -> ProtocolCandidate {
        ProtocolCandidate {
            quote: QuoteInfo {
                family: ProtocolFamily::Dlmm,
                action: MeteoraAction::Swap,
                pool: "pool".into(),
                position: None,
                token_a_debit_atoms: None,
                token_b_debit_atoms: None,
                token_a_credit_atoms: None,
                token_b_credit_atoms: None,
                receipt_delta_atoms: None,
                fees_a_atoms: None,
                fees_b_atoms: None,
                rewards: vec![],
                expires_at: "2030-01-01T00:00:00Z".into(),
                price_impact_bps: None,
                native_cash_delta: None,
                protocol_details: Value::Null,
            },
            instructions: vec![NormalizedInstruction {
                program_id: "program".into(),
                accounts: signers
                    .iter()
                    .map(|(address, signer)| NormalizedAccountMeta {
                        address: (*address).into(),
                        signer: *signer,
                        writable: true,
                    })
                    .collect(),
                data_base64: "".into(),
            }],
            lookup_tables: vec![],
            blockhash: "blockhash".into(),
            last_valid_block_height: 1,
            stable_position_address: None,
            required_secondary_signers: vec![],
            metadata: Value::Null,
        }
    }

    #[test]
    fn signer_scope_accepts_only_wallet_and_bound_auxiliary_signers() {
        let context = ProtocolBuildContext {
            wallet_address: "wallet".into(),
            position_signer_address: None,
            temporary_wsol_signer_address: Some("aux".into()),
        };
        let mut candidate = candidate(&[("wallet", true), ("aux", true)]);
        candidate.required_secondary_signers = vec!["aux".into()];
        assert!(validate_candidate_signer_scope(&candidate, &context).is_ok());

        candidate.instructions[0]
            .accounts
            .push(NormalizedAccountMeta {
                address: "unapproved".into(),
                signer: true,
                writable: false,
            });
        assert!(validate_candidate_signer_scope(&candidate, &context).is_err());
    }
}
