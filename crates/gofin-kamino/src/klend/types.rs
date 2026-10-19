//! Direct KLend obligations are distinct from managed kVault share positions.
use async_trait::async_trait;
use gofin_domain::{Result, SignedQuantity};
use gofin_solana::{FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::protocol::{TokenAmount, TokenInfo, VerifiedRentAccountDelta, VerifiedTokenDelta};

pub const OP_MARKETS: &str = "kamino.lending.markets";
pub const OP_MARKET: &str = "kamino.lending.market";
pub const OP_RESERVES: &str = "kamino.lending.reserves";
pub const OP_RESERVE: &str = "kamino.lending.reserve";
pub const OP_OBLIGATIONS: &str = "kamino.lending.obligations";
pub const OP_OBLIGATION: &str = "kamino.lending.obligation";
pub const OP_QUOTE: &str = "kamino.lending.quote";
pub const OP_SUPPLY: &str = "kamino.lending.supply";
pub const OP_WITHDRAW: &str = "kamino.lending.withdraw";
pub const OP_BORROW: &str = "kamino.lending.borrow";
pub const OP_REPAY: &str = "kamino.lending.repay";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LendingAction {
    Supply,
    Withdraw,
    Borrow,
    Repay,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum LendingSide {
    Supply,
    Debt,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LendingQuery {
    pub search: Option<String>,
    pub token_mint: Option<String>,
    pub limit: u16,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketInfo {
    pub address: String,
    pub program: String,
    pub name: Option<String>,
    pub observed_slot: u64,
    pub observed_at: String,
    pub restrictions: Vec<String>,
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReserveInfo {
    pub address: String,
    pub market: String,
    pub program: String,
    pub token: TokenInfo,
    pub collateral_token: TokenInfo,
    pub available_liquidity_atoms: String,
    pub supply_available: bool,
    pub withdraw_available: bool,
    pub borrow_available: bool,
    pub repay_available: bool,
    pub restrictions: Vec<String>,
    pub observed_slot: u64,
    pub observed_at: String,
    pub metrics: Value,
    /// Exact configured fees, collateral/debt farms, oracle identities, risk
    /// parameters, rates and native fractions, independently decoded through RPC.
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LendingRisk {
    /// Native KLend scaled fractions, all with 60 fractional bits.
    pub deposited_value_sf: String,
    pub borrowed_assets_market_value_sf: String,
    pub borrow_factor_adjusted_debt_value_sf: String,
    pub allowed_borrow_value_sf: String,
    pub unhealthy_borrow_value_sf: String,
    /// False means stored protocol values have not been independently refreshed.
    pub refreshed: bool,
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LendingPosition {
    pub reserve: String,
    pub side: LendingSide,
    /// Native underlying mint for current exposure and debt.
    pub token: TokenInfo,
    /// Required on supply: exact cToken mint/program used as accounting units.
    pub collateral_token: Option<TokenInfo>,
    /// Supply: collateral cToken atoms. Debt: raw borrowed_amount_sf.
    pub native_units: String,
    /// Current exposure: floor supply backing / ceil debt. Unknown stays None.
    pub underlying_atoms: Option<String>,
    /// Decimal integer representation of the native big fraction index.
    pub cumulative_borrow_rate_bsf: Option<String>,
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObligationInfo {
    pub address: String,
    pub market: String,
    pub program: String,
    pub owner: String,
    pub exists: bool,
    pub tag: u64,
    pub elevation_group: u8,
    pub referrer: Option<String>,
    pub positions: Vec<LendingPosition>,
    pub risk: LendingRisk,
    pub writes_available: bool,
    pub restrictions: Vec<String>,
    pub observed_slot: u64,
    pub observed_at: String,
    pub complete: bool,
    pub warnings: Vec<String>,
    pub protocol_state: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketPage {
    pub markets: Vec<MarketInfo>,
    pub complete: bool,
    pub next_cursor: Option<String>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReservePage {
    pub reserves: Vec<ReserveInfo>,
    pub complete: bool,
    pub next_cursor: Option<String>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObligationPage {
    pub obligations: Vec<ObligationInfo>,
    pub complete: bool,
    pub next_cursor: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RiskLimits {
    /// unhealthy_borrow_value / borrow_factor_adjusted_debt_value; 10000 = 1.
    pub minimum_health_factor_bps: u64,
    /// Total raw debt market value in protocol USD, not wallet pricing.
    pub maximum_debt_usd_micros: String,
    /// Borrow-factor-adjusted debt / collateral market value.
    pub maximum_debt_to_collateral_bps: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LendingIntent {
    pub wallet: String,
    pub action: LendingAction,
    pub market: String,
    pub reserve: String,
    pub obligation: String,
    pub token: TokenInfo,
    /// Exact native instruction quantity. Withdraw uses collateral cToken atoms;
    /// supply/borrow/repay use underlying atoms. Supply is an input maximum:
    /// native collateral rounding may leave an unspent residual. Borrow is the
    /// exact wallet receipt.
    /// Repay-all uses a finite approved maximum, never u64::MAX.
    pub amount_atoms: String,
    /// Withdraw-all freezes exact collateral atoms. Repay-all must prove zero
    /// remaining debt and may consume only amount_atoms of underlying.
    pub all: bool,
    pub minimum_receive_atoms: Option<String>,
    /// Gross newly incurred debt, including the native origination fee.
    pub maximum_debt_increase_atoms: Option<String>,
    pub maximum_protocol_fee_atoms: String,
    pub risk_limits: Option<RiskLimits>,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LendingQuote {
    pub debits: Vec<TokenAmount>,
    pub credits: Vec<TokenAmount>,
    pub protocol_fees: Vec<TokenAmount>,
    pub projected_obligation: ObligationInfo,
    pub rent_lamports: u64,
    pub network_fee_lamports: u64,
    pub expires_at: String,
    pub warnings: Vec<String>,
    /// Distinguishes native program guards from pre-sign risk checks/estimates.
    pub details: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LendingCandidate {
    pub reserve: ReserveInfo,
    pub obligation_before: ObligationInfo,
    pub quote: LendingQuote,
    pub instructions: Vec<NormalizedInstruction>,
    pub lookup_tables: Vec<String>,
    pub blockhash: String,
    pub last_valid_block_height: u64,
    pub metadata: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedLendingTransition {
    pub reserve: String,
    pub side: LendingSide,
    /// Accounting mint: collateral cToken for supply, underlying for debt.
    pub token: TokenInfo,
    pub native_units_before: String,
    pub native_units_after: String,
    /// Exact action-attributable accounting units: cTokens for Supply,
    /// underlying debt atoms for Debt. Excludes separately proved interest.
    pub action_delta_atoms: SignedQuantity,
    /// Only present when independently proved, never inferred from a later
    /// unguarded reserve ratio or a simulation.
    pub accrual_delta_atoms: Option<SignedQuantity>,
    pub evidence: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedLendingEffects {
    pub token_deltas: Vec<VerifiedTokenDelta>,
    pub transitions: Vec<VerifiedLendingTransition>,
    pub rent_accounts: Vec<VerifiedRentAccountDelta>,
    pub protocol_fees: Vec<TokenAmount>,
    pub obligation_after: ObligationInfo,
    pub evidence: Value,
}

#[async_trait]
pub trait KlendProtocol: Send + Sync {
    /// Canonical tag-zero/id-zero vanilla obligation for first-supply discovery.
    /// The caller still performs the ordinary verified observation afterward.
    fn default_obligation_address(&self, _owner: &str, _market: &str) -> Result<String> {
        Err(gofin_domain::GoFinError::new(
            "KAMINO_LENDING_DEFAULT_OBLIGATION_UNAVAILABLE",
            "discovery",
            "This lending provider cannot derive a canonical default obligation.",
            "Select an explicit obligation from verified discovery.",
        ))
    }
    async fn markets(&self, runtime: &SolanaRuntime, query: &LendingQuery) -> Result<MarketPage>;
    async fn market(&self, runtime: &SolanaRuntime, address: &str) -> Result<MarketInfo>;
    async fn reserves(
        &self,
        runtime: &SolanaRuntime,
        market: &str,
        query: &LendingQuery,
    ) -> Result<ReservePage>;
    async fn reserve(
        &self,
        runtime: &SolanaRuntime,
        market: &str,
        address: &str,
    ) -> Result<ReserveInfo>;
    async fn obligations(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        market: Option<&str>,
        query: &LendingQuery,
    ) -> Result<ObligationPage>;
    /// A verified absent default vanilla obligation may be returned with
    /// exists=false for an explicitly disclosed create-and-supply transaction.
    async fn obligation(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        market: &str,
        address: &str,
    ) -> Result<ObligationInfo>;
    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &LendingIntent,
        owner: &str,
    ) -> Result<LendingCandidate>;
    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &LendingIntent,
        owner: &str,
        candidate: &LendingCandidate,
    ) -> Result<()>;
    async fn verify_finalized(
        &self,
        runtime: &SolanaRuntime,
        intent: &LendingIntent,
        owner: &str,
        candidate: &LendingCandidate,
        proof: &FinalizedReceiptProof,
    ) -> Result<VerifiedLendingEffects>;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicIntent {
    pub wallet: String,
    pub network: String,
    pub cluster: String,
    pub market: String,
    pub reserve: String,
    pub obligation: String,
    pub token_mint: String,
    pub action: Option<LendingAction>,
    pub amount_atoms: Option<String>,
    pub collateral_atoms: Option<String>,
    pub maximum_repay_atoms: Option<String>,
    pub minimum_receive_atoms: Option<String>,
    pub maximum_debt_increase_atoms: Option<String>,
    pub maximum_protocol_fee_atoms: String,
    pub risk_limits: Option<RiskLimits>,
    pub maximum_network_fee_lamports: u64,
    pub maximum_account_funding_lamports: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    pub wallet: Option<String>,
    pub market: Option<String>,
    pub reserve: Option<String>,
    pub obligation: Option<String>,
    pub search: Option<String>,
    pub token_mint: Option<String>,
    pub limit: Option<u16>,
    pub cursor: Option<String>,
}

pub fn operation_definitions() -> Vec<gofin_domain::Definition> {
    use gofin_domain::{Definition, ExecutionActor, OperationKind};
    use serde_json::json;
    let quantity = json!({"type":"string","pattern":"^(0|[1-9][0-9]*)$"});
    let positive = json!({"type":"string","pattern":"^[1-9][0-9]*$"});
    let all = json!({"type":"string","pattern":"^(all|[1-9][0-9]*)$"});
    let read = json!({"type":"object","properties":{"wallet":{"type":"string"},"market":{"type":"string"},"reserve":{"type":"string"},"obligation":{"type":"string"},"search":{"type":"string"},"token_mint":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100},"cursor":{"type":"string"}},"additionalProperties":false});
    let risk = json!({"type":"object","properties":{"minimum_health_factor_bps":{"type":"integer","minimum":10001},"maximum_debt_usd_micros":quantity,"maximum_debt_to_collateral_bps":{"type":"integer","minimum":1,"maximum":9999}},"required":["minimum_health_factor_bps","maximum_debt_usd_micros","maximum_debt_to_collateral_bps"],"additionalProperties":false});
    let write = json!({"type":"object","properties":{
        "wallet":{"type":"string"},"network":{"const":"solana"},"cluster":{"const":"mainnet-beta"},"market":{"type":"string"},"reserve":{"type":"string"},"obligation":{"type":"string"},"token_mint":{"type":"string"},"action":{"enum":["supply","withdraw","borrow","repay"]},
        "amount_atoms":all,"collateral_atoms":all,"maximum_repay_atoms":positive,"minimum_receive_atoms":quantity,"maximum_debt_increase_atoms":positive,"maximum_protocol_fee_atoms":quantity,"risk_limits":risk,"maximum_network_fee_lamports":{"type":"integer","minimum":1},"maximum_account_funding_lamports":{"type":"integer","minimum":0}},
        "required":["wallet","network","cluster","market","reserve","obligation","token_mint","maximum_protocol_fee_atoms","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false});
    let mut definitions = Vec::new();
    for (name, description, kind, required) in [
        (
            OP_MARKETS,
            "Discover native KLend markets and restrictions.",
            OperationKind::Read,
            vec![],
        ),
        (
            OP_MARKET,
            "Inspect one RPC-verified KLend market.",
            OperationKind::Read,
            vec!["market"],
        ),
        (
            OP_RESERVES,
            "Discover native collateral and borrow reserves in a KLend market.",
            OperationKind::Read,
            vec!["market"],
        ),
        (
            OP_RESERVE,
            "Inspect canonical reserve assets, liquidity, rates, fees and restrictions.",
            OperationKind::Read,
            vec!["market", "reserve"],
        ),
        (
            OP_OBLIGATIONS,
            "Discover current wallet obligations, including externally opened and unavailable positions.",
            OperationKind::Read,
            vec!["wallet"],
        ),
        (
            OP_OBLIGATION,
            "Inspect exact collateral, debt and health; omit obligation to discover the wallet's canonical default vanilla obligation, including verified absence before first supply.",
            OperationKind::Read,
            vec!["wallet", "market"],
        ),
        (
            OP_QUOTE,
            "Preview a native KLend action with financial limits and risk checks.",
            OperationKind::Read,
            vec!["action"],
        ),
        (
            OP_SUPPLY,
            "Supply underlying assets up to an exact input maximum as collateral; native rounding may leave unspent input.",
            OperationKind::Write,
            vec!["amount_atoms"],
        ),
        (
            OP_WITHDRAW,
            "Withdraw exact collateral units and redeem native underlying assets.",
            OperationKind::Write,
            vec!["collateral_atoms", "minimum_receive_atoms", "risk_limits"],
        ),
        (
            OP_BORROW,
            "Borrow exact underlying receipts under finite debt and obligation risk limits.",
            OperationKind::Write,
            vec!["amount_atoms", "maximum_debt_increase_atoms", "risk_limits"],
        ),
        (
            OP_REPAY,
            "Repay native variable debt; all requires a finite debit cap and zero remaining debt.",
            OperationKind::Write,
            vec!["amount_atoms"],
        ),
    ] {
        let transaction = matches!(
            name,
            OP_QUOTE | OP_SUPPLY | OP_WITHDRAW | OP_BORROW | OP_REPAY
        );
        let mut schema = if transaction {
            write.clone()
        } else {
            read.clone()
        };
        let mut keys = schema["required"].as_array().cloned().unwrap_or_default();
        keys.extend(required.into_iter().map(|key| json!(key)));
        schema["required"] = json!(keys);
        if name == OP_OBLIGATION {
            schema["properties"]["obligation"]["description"] = json!(
                "Optional explicit address. When omitted, resolve and inspect the canonical tag-zero/id-zero vanilla obligation for this wallet and market; exists=false reports verified absence and its address can be used for first supply."
            );
        }
        if matches!(name, OP_SUPPLY | OP_BORROW) {
            schema["properties"]["amount_atoms"] = positive.clone();
        }
        if name == OP_SUPPLY {
            schema["properties"]["amount_atoms"]["description"] = json!(
                "Exact maximum underlying atoms passed to KLend. Native collateral rounding may consume less; the quote discloses expected debit and unspent input, and settlement records actual debit within this maximum."
            );
        }
        definitions.push(Definition {
            name:name.into(),description:description.into(),kind:kind.clone(),execution_actor:if kind==OperationKind::Read{ExecutionActor::AgentReadable}else{ExecutionActor::AgentInvocable},
            input_schema:schema,output_schema:json!({"type":"object"}),error_schema:json!({"type":"object","required":["code","stage","message","next_action"]}),examples:vec![],
            coverage:"Solana mainnet native KLend obligations. Writes support vanilla elevation-group-zero floating debt; fixed-term debt, active borrow orders and ownership transfers remain explicit restrictions. Managed vault operations retain their existing names.".into(),
            completion:"Writes settle only from the immutable finalized transaction and attributable obligation evidence; a submitted transaction or later unguarded balance is not settlement.".into(),
            semantics:json!({"quantity_unit":"Exact atomic units. Supply uses a maximum underlying input, borrow an exact underlying receipt, repay a finite underlying payment cap; withdraw uses exact reserve collateral cToken units.","supply_rounding":"Native KLend floors minted collateral then computes needed liquidity, so supply may debit less than amount_atoms. Original input remains the approved maximum; quote and settlement expose the retained input residual.","repay_all":"Requires finite maximum_repay_atoms. The instruction never uses an unbounded sentinel and completion proves zero selected debt.","withdraw_all":"Freezes the observed exact collateral quantity before approval.","risk":"Health is unhealthy-borrow value divided by borrow-factor-adjusted debt. Custom debt/health limits are fresh pre-sign checks; the native program enforces its own limits, not custom health thresholds.","default_obligation_discovery":"Read kamino.lending.obligation with wallet and market; omit obligation to inspect the canonical default vanilla address. A verified absent account returns its top-level address with exists=false for first-supply preparation. Explicit addresses remain unchanged.","valuation":"Collateral ownership is exact cToken units; current underlying backing is independently observed and never fabricated as historical contribution or transaction yield.","composition":"Swaps and other operations remain separate agent-planned steps using verified proceeds.","boundary_parameters":{"type":"object","properties":{"minimum_amount_atoms":quantity,"maximum_amount_atoms":positive,"maximum_debt_increase_atoms":positive,"maximum_cumulative_borrow_atoms":positive,"maximum_protocol_fee_atoms":quantity,"maximum_network_fee_lamports":{"type":"integer","minimum":1},"maximum_account_funding_lamports":{"type":"integer","minimum":0}},"required":["maximum_amount_atoms","maximum_protocol_fee_atoms","maximum_network_fee_lamports","maximum_account_funding_lamports"],"additionalProperties":false},"plan_permission":{"supported":true,"requires_adapter_scope":name==OP_BORROW},"borrow_reusable_authority":"Borrowing requires named-plan cumulative debt scope; ordinary reusable boundaries are unsupported.","authority":"Wallet, market, obligation, reserve, mint/program, destination, mode, fee terms and risk limits remain fixed. Borrow plans require gross cumulative debt authority; repayment never replenishes it."}),
        });
    }
    definitions
}
