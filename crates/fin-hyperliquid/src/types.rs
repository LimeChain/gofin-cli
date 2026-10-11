//! Exact Hyperliquid identities, decimals and capability inputs.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Exact base-10 text as returned by Hyperliquid. Never convert to `f64`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct ExactDecimal(String);

impl ExactDecimal {
    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err("invalid decimal");
        }
        let digits = value.strip_prefix('-').unwrap_or(&value);
        let mut parts = digits.split('.');
        let whole = parts.next().unwrap_or_default();
        let frac = parts.next();
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || frac.is_some_and(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err("invalid decimal");
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ExactDecimal {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<ExactDecimal> for String {
    fn from(value: ExactDecimal) -> String {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MarketKind {
    Perpetual,
    Hip3Perpetual,
    Spot,
}

/// Identifies a Hyperliquid market without assuming ticker-name uniqueness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketRef {
    /// Exact CCXT unified symbol or native market name.
    pub symbol: String,
    pub kind: MarketKind,
    /// HIP-3 dex name when kind is `hip3_perpetual`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dex: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeMarket {
    pub symbol: String,
    /// Exact native `coin` string for info requests/actions (perp name or spot `@index`).
    pub native_coin: String,
    pub base_asset: AssetReference,
    pub quote_asset: AssetReference,
    pub kind: MarketKind,
    /// Metadata observation time in Unix milliseconds.
    pub observed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dex: Option<String>,
    /// Position in the applicable native universe.
    pub universe_index: u32,
    /// Position from `perpDexs`; present only for HIP-3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dex_index: Option<u32>,
    /// Hyperliquid action asset index (derived from market kind and native indices).
    pub asset_id: u32,
    pub sz_decimals: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_leverage: Option<u32>,
    #[serde(default)]
    pub is_delisted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<MarketContext>,
    /// Spot token indices [base, quote]. Absent on perpetuals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot_tokens: Option<[u32; 2]>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountInput {
    pub account: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountMarketInput {
    pub account: String,
    /// Optional exact HIP-3 DEX name. Omission selects the primary DEX.
    #[serde(default)]
    pub dex: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TradingStateInput {
    pub account: String,
    pub symbol: String,
    #[serde(default)]
    pub dex: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketInput {
    pub symbol: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketsInput {
    #[serde(default)]
    pub kind: Option<MarketKind>,
    #[serde(default)]
    pub dex: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketListInput {
    #[serde(default)]
    pub symbols: Option<Vec<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryInput {
    pub account: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub since_ms: Option<u64>,
    #[serde(default)]
    pub until_ms: Option<u64>,
    #[serde(default)]
    pub limit: Option<u16>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrderInput {
    pub account: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub order_id: Option<String>,
    #[serde(default)]
    pub client_order_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandleInput {
    pub symbol: String,
    pub interval: String,
    pub start_ms: u64,
    #[serde(default)]
    pub end_ms: Option<u64>,
    #[serde(default)]
    pub limit: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LimitOrderInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    pub market_type: MarketType,
    #[serde(default)]
    pub dex: Option<String>,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub size: String,
    #[serde(default)]
    pub limit_price: Option<String>,
    #[serde(default)]
    pub min_price: Option<String>,
    #[serde(default)]
    pub max_price: Option<String>,
    #[serde(default)]
    pub time_in_force: Option<TimeInForce>,
    #[serde(default)]
    pub trigger_price: Option<String>,
    #[serde(default)]
    pub trigger_kind: Option<TriggerKind>,
    #[serde(default)]
    pub trigger_is_market: Option<bool>,
    #[serde(default)]
    pub grouping: Option<OrderGrouping>,
    pub max_notional_usdc: String,
    pub max_fee_usdc: String,
    #[serde(default)]
    pub max_realized_loss_usdc: Option<String>,
    #[serde(default)]
    pub builder_fee: Option<OrderBuilderFeeInput>,
    #[serde(default)]
    pub reduce_only: bool,
}

/// A builder fee attached to this order action. Builder authorization is
/// checked independently against protected owner state during preparation.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OrderBuilderFeeInput {
    pub address: String,
    pub fee_tenths_bps: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketType {
    Spot,
    Perpetual,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Market,
    Trigger,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    TakeProfit,
    StopLoss,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderGrouping {
    Na,
    NormalTpsl,
    PositionTpsl,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Alo,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchOrdersInput {
    pub request_id: String,
    pub account: String,
    pub orders: Vec<LimitOrderInput>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelOrderInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    pub order_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelOrdersInput {
    pub request_id: String,
    pub account: String,
    pub orders: Vec<CancelOrderInput>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditOrderInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    pub market_type: MarketType,
    #[serde(default)]
    pub dex: Option<String>,
    pub order_id: String,
    /// Durable Fin order operation whose submission created this native order.
    /// Required when linking this replacement to an existing Fin reservation.
    #[serde(default)]
    pub parent_operation_id: Option<String>,
    pub size: String,
    pub limit_price: String,
    pub max_notional_usdc: String,
    pub max_fee_usdc: String,
    #[serde(default)]
    pub max_realized_loss_usdc: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditOrderItem {
    pub symbol: String,
    pub market_type: MarketType,
    #[serde(default)]
    pub dex: Option<String>,
    pub order_id: String,
    /// Durable Fin order operation whose submission created this native order.
    /// Required when linking this replacement to an existing Fin reservation.
    #[serde(default)]
    pub parent_operation_id: Option<String>,
    pub size: String,
    pub limit_price: String,
    pub max_notional_usdc: String,
    pub max_fee_usdc: String,
    #[serde(default)]
    pub max_realized_loss_usdc: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditOrdersInput {
    pub request_id: String,
    pub account: String,
    pub orders: Vec<EditOrderItem>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwapInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    /// Hyperliquid native TWAP actions are supported for perpetual markets.
    pub market_type: MarketType,
    #[serde(default)]
    pub dex: Option<String>,
    pub side: OrderSide,
    pub size: String,
    /// Native action duration, in minutes.
    pub duration_minutes: u32,
    /// Estimated gross notional used for Fin's reservation and review. It is
    /// not a venue-enforced lifetime quote-price ceiling.
    pub estimated_notional_usdc: String,
    /// Estimated fee reserve. Native TWAP execution does not guarantee this
    /// is an absolute fee ceiling.
    pub estimated_fee_reserve_usdc: String,
    /// Explicit estimate reserved for gross realized losses across slices.
    /// Native price movement does not guarantee this is a lifetime loss cap.
    pub estimated_realized_loss_reserve_usdc: String,
    /// Estimated positive realized PnL allowance used by Fin's settlement
    /// expectation. It is not a venue-enforced cap.
    pub estimated_realized_profit_usdc: String,
    /// Explicitly acknowledges native relative slice pricing and the absence
    /// of an absolute lifetime quote-price bound.
    pub accept_native_price_risk: bool,
    /// Exact native `t` parameter controlling slice schedule randomization.
    pub randomize: bool,
    /// Exact native `r` parameter.
    pub reduce_only: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeverageInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    /// Explicit HIP-3 DEX name; omitted for the primary perpetual universe.
    #[serde(default)]
    pub dex: Option<String>,
    pub leverage: u16,
    #[serde(default)]
    pub cross: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarginInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    /// Explicit HIP-3 DEX name; omitted for the primary perpetual universe.
    #[serde(default)]
    pub dex: Option<String>,
    pub amount: String,
    pub max_amount_usdc: String,
    pub max_fee_usdc: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferInput {
    pub request_id: String,
    pub account: String,
    pub asset: String,
    pub amount: String,
    pub destination: TransferDestination,
    /// Exact enrolled child wallet for a subaccount transfer.
    #[serde(default)]
    pub subaccount_address: Option<String>,
    /// Exact Hyperliquid owner address for an address-destination USDC send.
    #[serde(default)]
    pub destination_address: Option<String>,
    /// Direction is explicit because the parent account is not always the source.
    #[serde(default)]
    pub subaccount_direction: Option<SubaccountTransferDirection>,
    pub max_amount_usdc: String,
    pub max_fee_usdc: String,
}

/// Invalidate the exact nonce used by one unresolved Hyperliquid class transfer.
/// The nonce itself is loaded only from Fin's durable original submission.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InvalidateNonceInput {
    pub request_id: String,
    pub account: String,
    pub original_operation_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferDestination {
    Spot,
    Perpetual,
    Subaccount,
    Address,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubaccountTransferDirection {
    ToSubaccount,
    FromSubaccount,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalInput {
    pub request_id: String,
    pub account: String,
    pub asset: String,
    pub amount: String,
    pub address: String,
    pub max_amount_usdc: String,
    pub max_fee_usdc: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VaultActionInput {
    pub request_id: String,
    pub account: String,
    pub vault_address: String,
    pub amount: String,
    pub max_amount_usdc: String,
    pub max_fee_usdc: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AbstractionInput {
    pub request_id: String,
    pub account: String,
    pub mode: AbstractionMode,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DexAbstractionInput {
    pub request_id: String,
    pub account: String,
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AbstractionMode {
    Unified,
    Separate,
    PortfolioMargin,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuilderFeeInput {
    pub request_id: String,
    pub account: String,
    pub builder_address: String,
    pub fee_tenths_bps: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubaccountInput {
    pub request_id: String,
    pub account: String,
    pub name: String,
    #[serde(default)]
    pub expires_after_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleCancelInput {
    pub request_id: String,
    pub account: String,
    pub after_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DepositInput {
    pub request_id: String,
    pub account: String,
    pub wallet: String,
    pub amount: String,
    pub max_fee_eth: String,
    pub max_amount_usdc: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountActionInput {
    pub request_id: String,
    pub account: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferralInput {
    pub request_id: String,
    pub account: String,
    pub referral_code: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwapCancelInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    pub market_type: MarketType,
    #[serde(default)]
    pub dex: Option<String>,
    pub twap_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateVaultInput {
    pub request_id: String,
    pub account: String,
    pub name: String,
    pub description: String,
    pub seed_amount_usdc: String,
    pub max_fee_usdc: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReserveWeightInput {
    pub request_id: String,
    pub account: String,
    pub weight: u32,
    /// Optional existing L1 user who receives the reserved actions.
    #[serde(default)]
    pub destination: Option<String>,
}

/// Exact asset identity from native metadata; the ticker is descriptive only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssetReference {
    pub reference: String,
    pub symbol: String,
    pub decimals: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_index: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketContext {
    #[serde(default)]
    pub mark_price: Option<ExactDecimal>,
    #[serde(default)]
    pub mid_price: Option<ExactDecimal>,
    #[serde(default)]
    pub oracle_price: Option<ExactDecimal>,
    #[serde(default)]
    pub previous_day_price: Option<ExactDecimal>,
    #[serde(default)]
    pub day_notional_volume: Option<ExactDecimal>,
    #[serde(default)]
    pub funding_rate: Option<ExactDecimal>,
    #[serde(default)]
    pub open_interest: Option<ExactDecimal>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketHistoryInput {
    pub symbol: String,
    #[serde(default)]
    pub since_ms: Option<u64>,
    #[serde(default)]
    pub until_ms: Option<u64>,
    #[serde(default)]
    pub limit: Option<u16>,
}
