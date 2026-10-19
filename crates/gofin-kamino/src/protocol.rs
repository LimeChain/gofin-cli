//! Protocol-owned observation and instruction contracts; no signer or swap access.

use async_trait::async_trait;
use gofin_domain::{Result, SignedQuantity};
use gofin_solana::{FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum VaultFamily {
    Lending,
    Liquidity,
}

impl VaultFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lending => "lending",
            Self::Liquidity => "liquidity",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VaultAction {
    Deposit,
    Withdraw,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenInfo {
    pub mint: String,
    pub decimals: u8,
    pub token_program: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenAmount {
    pub token: TokenInfo,
    /// Exact base units; None is unknown, never zero.
    pub atoms: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VaultInfo {
    pub family: VaultFamily,
    pub address: String,
    pub program: String,
    pub name: Option<String>,
    pub backend: Option<String>,
    pub assets: Vec<TokenInfo>,
    pub shares: TokenInfo,
    pub farm: Option<String>,
    pub deposits_available: bool,
    pub withdrawals_available: bool,
    pub restrictions: Vec<String>,
    pub observed_slot: Option<u64>,
    pub observed_at: String,
    pub metrics: Value,
    /// Canonical decoded protocol data, distinct from unverified API metrics.
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VaultQuery {
    pub search: Option<String>,
    pub token_mint: Option<String>,
    pub limit: u16,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VaultPage {
    pub vaults: Vec<VaultInfo>,
    pub complete: bool,
    pub next_cursor: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionInfo {
    pub vault: VaultInfo,
    pub owner: String,
    pub wallet_shares_atoms: String,
    pub staked_shares_atoms: String,
    pub pending_shares_atoms: String,
    pub underlying: Vec<TokenAmount>,
    pub rewards: Vec<TokenAmount>,
    pub withdrawable_shares_atoms: Option<String>,
    pub complete: bool,
    pub warnings: Vec<String>,
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PositionInventory {
    pub positions: Vec<PositionInfo>,
    pub complete: bool,
    pub observed_at: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VaultIntent {
    pub wallet: String,
    pub family: VaultFamily,
    pub action: VaultAction,
    pub vault: String,
    /// Deposits pin each underlying mint and its maximum debit in base units.
    pub amounts: Vec<TokenAmount>,
    /// Withdrawals are concrete shares after the adapter resolves public "all".
    pub shares_atoms: Option<String>,
    pub minimum_shares_atoms: Option<String>,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
    pub stake: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VaultQuote {
    pub debits: Vec<TokenAmount>,
    pub credits: Vec<TokenAmount>,
    pub shares_atoms: String,
    pub minimum_shares_enforced: bool,
    pub protocol_fees: Vec<TokenAmount>,
    pub rent_lamports: u64,
    pub network_fee_lamports: u64,
    pub expires_at: String,
    pub warnings: Vec<String>,
    pub details: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VaultCandidate {
    pub vault: VaultInfo,
    pub quote: VaultQuote,
    pub instructions: Vec<NormalizedInstruction>,
    pub lookup_tables: Vec<String>,
    pub blockhash: String,
    pub last_valid_block_height: u64,
    pub metadata: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedTokenDelta {
    pub token: TokenInfo,
    pub atoms: SignedQuantity,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedVaultEffects {
    pub token_deltas: Vec<VerifiedTokenDelta>,
    /// Transaction-attributable change in total redeemable owned share atoms,
    /// including the integer entitlement to farm custody.
    pub shares_delta: SignedQuantity,
    /// Native shares minted or burned, proven by wallet and farm token-custody
    /// balances. This may differ from the integer redeemable entitlement change
    /// when a farm exchange rate rounds fractional shares.
    #[serde(default)]
    pub native_shares_delta: Option<SignedQuantity>,
    pub rent_lamports_delta: SignedQuantity,
    #[serde(default)]
    pub rent_accounts: Vec<VerifiedRentAccountDelta>,
    pub position: Option<PositionInfo>,
    pub evidence: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedRentAccountDelta {
    pub account: String,
    /// Positive is refundable funding locked; negative is returned rent.
    pub atoms: SignedQuantity,
}

#[async_trait]
pub trait KaminoProtocol: Send + Sync {
    fn family(&self) -> VaultFamily;
    async fn vaults(&self, runtime: &SolanaRuntime, query: &VaultQuery) -> Result<VaultPage>;
    async fn vault(&self, runtime: &SolanaRuntime, address: &str) -> Result<VaultInfo>;
    /// Read canonical custody terms without depending on wallet funding or
    /// simulating a future deposit, for balance-independent plan permissions.
    async fn staking_farm(
        &self,
        runtime: &SolanaRuntime,
        vault: &VaultInfo,
    ) -> Result<crate::farms::FarmInfo> {
        let address = vault.farm.as_deref().ok_or_else(|| {
            gofin_domain::GoFinError::validation("The vault has no associated staking farm.")
        })?;
        crate::farms::farm(runtime, address, &vault.shares.mint).await
    }
    async fn positions(&self, runtime: &SolanaRuntime, owner: &str) -> Result<PositionInventory>;
    async fn position(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        vault: &str,
    ) -> Result<PositionInfo>;
    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
    ) -> Result<VaultCandidate>;
    /// Re-read canonical RPC state and independently verify the complete native
    /// graph, current action eligibility, family/program/backend, vault, all
    /// mint/program identities and farm custody against the frozen intent.
    /// Descriptive API data must never be required on this dispatch path.
    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
        candidate: &VaultCandidate,
    ) -> Result<()>;
    async fn verify_finalized(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
        candidate: &VaultCandidate,
        proof: &FinalizedReceiptProof,
    ) -> Result<VerifiedVaultEffects>;
}
