//! Narrow boundary around the official CCXT Rust engine. No CCXT types escape.
//!
//! A bounded dedicated worker owns exchange instances and rate-limit state.
//! Financial parameters are strings. A delegating response observer captures
//! exact wire JSON before CCXT's lossy dynamic-number representation is used.
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
use std::{cell::Cell, sync::Once};
use std::{fmt, time::Duration};
#[cfg(feature = "hyperliquid")]
use zeroize::Zeroize;

#[cfg(feature = "binance")]
use ccxt::exchanges::binance::BinanceCore;
#[cfg(feature = "bybit")]
use ccxt::exchanges::bybit::BybitCore;
#[cfg(feature = "coinbase")]
use ccxt::exchanges::coinbase::CoinbaseCore;
#[cfg(feature = "hyperliquid")]
use ccxt::exchanges::hyperliquid::HyperliquidCore;
#[cfg(feature = "kraken")]
use ccxt::exchanges::kraken::KrakenCore;
#[cfg(feature = "okx")]
use ccxt::exchanges::okx::OkxCore;
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
use ccxt::{
    exchange::{DerivedExchange, ExchangeRuntime},
    exchange_generated::ExchangeBase,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExchangeId {
    Binance,
    Kraken,
    Hyperliquid,
    Okx,
    Coinbase,
    Bybit,
}

/// This list is internal dispatch, never a user-controlled arbitrary CCXT method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    BinanceApiRestrictions,
    BinanceAccount,
    BinanceSubAccountList,
    BinanceSubAccountSpotAssets,
    BinanceCapitalConfig,
    BinanceDepositAddress,
    BinanceDepositAddressList,
    BinanceWithdrawAddressList,
    BinanceDepositHistory,
    BinanceWithdrawalHistory,
    BinanceUniversalTransferHistory,
    BinanceSubAccountTransferHistory,
    BinanceUniversalTransfer,
    BinanceSubToMasterTransfer,
    BinanceStandardWithdrawal,
    BinanceTravelRuleRequirements,
    BinanceTickerPrice,
    BinanceOrderBookSpot,
    BinanceOrderBookPerpetual,
    BinanceSpotExchangeInfo,
    BinanceSpotCommission,
    BinanceSpotOpenOrders,
    BinanceSpotOrder,
    BinanceSpotTrades,
    BinanceSpotPlaceOrder,
    BinanceSpotCancelOrder,
    BinanceFuturesExchangeInfo,
    BinanceFuturesAccount,
    BinanceFuturesAccountConfig,
    BinanceFuturesSymbolConfig,
    BinanceFuturesPositions,
    BinanceFuturesPositionMode,
    BinanceFuturesCommission,
    BinanceFuturesFeeBurn,
    BinanceFuturesOrder,
    BinanceFuturesOpenOrders,
    BinanceFuturesOpenAlgoOrders,
    BinanceFuturesLeverage,
    BinanceFuturesMarginType,
    BinanceFuturesTrades,
    BinanceFuturesIncome,
    BinanceFuturesMarkPrice,
    BinanceFuturesLeverageBrackets,
    BinanceFuturesPlaceOrder,
    BinanceFuturesCancelOrder,
    BinanceWalletTransfer,
    BinanceWalletTransferHistory,

    KrakenTicker,
    KrakenAssetPairs,
    KrakenOrderBook,
    HyperliquidMetaAndAssetContexts,
    HyperliquidOrderBook,
    HyperliquidTypedInfo,
    HyperliquidTypedExplorer,
    HyperliquidTypedAction,
    OkxInstruments,
    OkxTicker,
    OkxOrderBook,
    CoinbaseProducts,
    CoinbaseProduct,
    CoinbaseTicker,
    CoinbaseOrderBook,
    BybitInstruments,
    BybitTicker,
    BybitOrderBook,
}
impl Method {
    fn exchange(self) -> ExchangeId {
        match self {
            Self::KrakenTicker | Self::KrakenAssetPairs | Self::KrakenOrderBook => {
                ExchangeId::Kraken
            }
            Self::HyperliquidMetaAndAssetContexts
            | Self::HyperliquidOrderBook
            | Self::HyperliquidTypedInfo
            | Self::HyperliquidTypedExplorer
            | Self::HyperliquidTypedAction => ExchangeId::Hyperliquid,
            Self::OkxInstruments | Self::OkxTicker | Self::OkxOrderBook => ExchangeId::Okx,
            Self::CoinbaseProducts
            | Self::CoinbaseProduct
            | Self::CoinbaseTicker
            | Self::CoinbaseOrderBook => ExchangeId::Coinbase,
            Self::BybitInstruments | Self::BybitTicker | Self::BybitOrderBook => ExchangeId::Bybit,
            _ => ExchangeId::Binance,
        }
    }
    #[cfg(any(
        feature = "binance",
        feature = "kraken",
        feature = "hyperliquid",
        feature = "okx",
        feature = "coinbase",
        feature = "bybit"
    ))]
    fn name(self) -> &'static str {
        match self {
            Self::BinanceApiRestrictions => "sapi_get_account_api_restrictions",
            Self::BinanceAccount => "private_get_account",
            Self::BinanceSubAccountList => "sapi_get_sub_account_list",
            Self::BinanceSubAccountSpotAssets => "sapi_v4_get_sub_account_assets",
            Self::BinanceCapitalConfig => "sapi_get_capital_config_getall",
            Self::BinanceDepositAddress => "sapi_get_capital_deposit_address",
            Self::BinanceDepositAddressList => "sapi_get_capital_deposit_address_list",
            Self::BinanceWithdrawAddressList => "sapi_get_capital_withdraw_address_list",
            Self::BinanceDepositHistory => "sapi_get_capital_deposit_hisrec",
            Self::BinanceWithdrawalHistory => "sapi_get_capital_withdraw_history",
            Self::BinanceUniversalTransferHistory => "sapi_get_sub_account_universal_transfer",
            Self::BinanceSubAccountTransferHistory => {
                "sapi_get_sub_account_transfer_sub_user_history"
            }
            Self::BinanceUniversalTransfer => "sapi_post_sub_account_universal_transfer",
            Self::BinanceSubToMasterTransfer => "sapi_post_sub_account_transfer_sub_to_master",
            Self::BinanceStandardWithdrawal => "sapi_post_capital_withdraw_apply",
            Self::BinanceTravelRuleRequirements => {
                "sapi_get_localentity_questionnaire_requirements"
            }
            Self::BinanceTickerPrice => "public_get_ticker_price",
            Self::BinanceOrderBookSpot => "public_get_depth",
            Self::BinanceOrderBookPerpetual => "fapi_public_get_depth",
            Self::BinanceSpotExchangeInfo => "public_get_exchange_info",
            Self::BinanceSpotCommission => "private_get_account_commission",
            Self::BinanceSpotOpenOrders => "private_get_open_orders",
            Self::BinanceSpotOrder => "private_get_order",
            Self::BinanceSpotTrades => "private_get_my_trades",
            Self::BinanceSpotPlaceOrder => "private_post_order",
            Self::BinanceSpotCancelOrder => "private_delete_order",
            Self::BinanceFuturesExchangeInfo => "fapi_public_get_exchange_info",
            Self::BinanceFuturesAccount => "fapi_private_v3_get_account",
            Self::BinanceFuturesAccountConfig => "fapi_private_get_account_config",
            Self::BinanceFuturesSymbolConfig => "fapi_private_get_symbol_config",
            Self::BinanceFuturesPositions => "fapi_private_v3_get_position_risk",
            Self::BinanceFuturesPositionMode => "fapi_private_get_position_side_dual",
            Self::BinanceFuturesCommission => "fapi_private_get_commission_rate",
            Self::BinanceFuturesFeeBurn => "fapi_private_get_fee_burn",
            Self::BinanceFuturesOrder => "fapi_private_get_order",
            Self::BinanceFuturesOpenAlgoOrders => "fapi_private_get_open_algo_orders",
            Self::BinanceFuturesLeverage => "fapi_private_post_leverage",
            Self::BinanceFuturesMarginType => "fapi_private_post_margin_type",
            Self::BinanceFuturesOpenOrders => "fapi_private_get_open_orders",
            Self::BinanceFuturesTrades => "fapi_private_get_user_trades",
            Self::BinanceFuturesIncome => "fapi_private_get_income",
            Self::BinanceFuturesMarkPrice => "fapi_public_get_premium_index",
            Self::BinanceFuturesLeverageBrackets => "fapi_private_get_leverage_bracket",
            Self::BinanceFuturesPlaceOrder => "fapi_private_post_order",
            Self::BinanceFuturesCancelOrder => "fapi_private_delete_order",
            Self::BinanceWalletTransfer => "sapi_post_asset_transfer",
            Self::BinanceWalletTransferHistory => "sapi_get_asset_transfer",

            Self::KrakenTicker => "public_get_ticker",
            Self::KrakenAssetPairs => "public_get_asset_pairs",
            Self::KrakenOrderBook => "public_get_depth",
            Self::HyperliquidMetaAndAssetContexts => "public_post_info",
            Self::HyperliquidOrderBook => "public_post_info",
            Self::HyperliquidTypedInfo => "public_post_info",
            Self::HyperliquidTypedExplorer => "public_post_explorer",
            Self::HyperliquidTypedAction => "private_post_exchange",
            Self::OkxInstruments => "public_get_public_instruments",
            Self::OkxTicker => "public_get_market_ticker",
            Self::OkxOrderBook => "public_get_market_books",
            Self::CoinbaseProducts => "v3_public_get_brokerage_market_products",
            Self::CoinbaseProduct => "v3_public_get_brokerage_market_products_product_id",
            Self::CoinbaseTicker => "v3_public_get_brokerage_market_products_product_id_ticker",
            Self::CoinbaseOrderBook => "v3_public_get_brokerage_market_product_book",
            Self::BybitInstruments => "public_get_v5_market_instruments_info",
            Self::BybitTicker => "public_get_v5_market_tickers",
            Self::BybitOrderBook => "public_get_v5_market_orderbook",
        }
    }
    #[cfg(any(
        feature = "binance",
        feature = "kraken",
        feature = "hyperliquid",
        feature = "okx",
        feature = "coinbase",
        feature = "bybit"
    ))]
    fn raw_encoded(self) -> bool {
        matches!(
            self,
            Self::BinanceSubAccountList
                | Self::BinanceSubAccountSpotAssets
                | Self::BinanceUniversalTransferHistory
                | Self::BinanceSubAccountTransferHistory
                | Self::BinanceUniversalTransfer
                | Self::BinanceSubToMasterTransfer
                | Self::BinanceStandardWithdrawal
                | Self::BybitInstruments
                | Self::BybitTicker
        )
    }
    fn requires_guard(self) -> bool {
        matches!(
            self,
            Self::BinanceSpotPlaceOrder
                | Self::BinanceSubToMasterTransfer
                | Self::BinanceUniversalTransfer
                | Self::BinanceSpotCancelOrder
                | Self::BinanceFuturesPlaceOrder
                | Self::BinanceFuturesLeverage
                | Self::BinanceFuturesMarginType
                | Self::BinanceFuturesCancelOrder
                | Self::BinanceWalletTransfer
                | Self::HyperliquidTypedAction
        )
    }
    fn private(self) -> bool {
        !matches!(
            self,
            Self::BinanceTickerPrice
                | Self::BinanceSpotExchangeInfo
                | Self::BinanceFuturesExchangeInfo
                | Self::BinanceFuturesMarkPrice
                | Self::BinanceOrderBookSpot
                | Self::BinanceOrderBookPerpetual
                | Self::KrakenTicker
                | Self::KrakenAssetPairs
                | Self::KrakenOrderBook
                | Self::HyperliquidMetaAndAssetContexts
                | Self::HyperliquidOrderBook
                | Self::HyperliquidTypedInfo
                | Self::HyperliquidTypedExplorer
                | Self::HyperliquidTypedAction
                | Self::OkxInstruments
                | Self::OkxTicker
                | Self::OkxOrderBook
                | Self::CoinbaseProducts
                | Self::CoinbaseProduct
                | Self::CoinbaseTicker
                | Self::CoinbaseOrderBook
                | Self::BybitInstruments
                | Self::BybitTicker
                | Self::BybitOrderBook
        )
    }
}

/// Secrets are supplied only by the protected credential provider, never argv.
/// Deliberately neither Debug nor Serialize.
pub struct Credentials {
    pub api_key: Zeroizing<String>,
    pub secret: Zeroizing<String>,
}

/// Protected Hyperliquid signer material. This type deliberately has no
/// `Debug` or `Serialize` implementation and is consumed by the CCXT worker.
pub struct HyperliquidSigner {
    pub private_key: Zeroizing<String>,
    pub wallet_address: String,
}

/// GoFin's closed set of user-facing Hyperliquid `/info` queries.
/// Values that identify a user are explicit and never inferred from a signer.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HyperliquidInfoRequest {
    ServerTime,
    ExchangeStatus,
    MetaAndAssetCtxs,
    MetaAndAssetCtxsForDex {
        dex: String,
    },
    SpotMetaAndAssetCtxs,
    PerpDexs,
    AllMids,
    L2Book {
        coin: String,
    },
    CandleSnapshot {
        coin: String,
        interval: String,
        start_time_ms: u64,
        end_time_ms: u64,
    },
    RecentTrades {
        coin: String,
    },
    FundingHistory {
        coin: String,
        start_time_ms: u64,
        end_time_ms: u64,
    },
    ClearinghouseState {
        owner: String,
    },
    ClearinghouseStateForDex {
        owner: String,
        dex: String,
    },
    SpotClearinghouseState {
        owner: String,
    },
    OpenOrders {
        owner: String,
    },
    FrontendOpenOrders {
        owner: String,
    },
    FrontendOpenOrdersForDex {
        owner: String,
        dex: String,
    },
    OrderStatus {
        owner: String,
        order_id: u64,
    },
    OrderStatusByClientId {
        owner: String,
        client_order_id: String,
    },
    HistoricalOrders {
        owner: String,
    },
    UserFills {
        owner: String,
    },
    UserFillsByTime {
        owner: String,
        start_time_ms: u64,
        end_time_ms: u64,
    },
    UserFunding {
        owner: String,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    },
    UserNonFundingLedgerUpdates {
        owner: String,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    },
    UserTwapSliceFills {
        owner: String,
    },
    UserRateLimit {
        owner: String,
    },
    UserFees {
        owner: String,
    },
    ReferralState {
        owner: String,
    },
    MaxBuilderFee {
        owner: String,
        builder: String,
    },
    UserRole {
        owner: String,
    },
    UserVaultEquities {
        owner: String,
    },
    VaultDetails {
        vault_address: String,
        user: Option<String>,
    },
    SubAccounts {
        owner: String,
    },
    UserAbstractionState {
        owner: String,
    },
    UserDexAbstractionState {
        owner: String,
    },
    ActiveAssetData {
        owner: String,
        coin: String,
    },
}

/// Read-only explorer lookup used internally to bind a native ledger hash to
/// the exact signed L1 action. This is separate from the public `/info` API.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HyperliquidExplorerRequest {
    TxDetails { hash: String },
    UserDetails { user: String },
}

/// Canonical nonce/action pair supplied by the protected GoFin adapter. The
/// transport never creates or refreshes a nonce.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidActionEnvelope {
    pub nonce_ms: u64,
    #[serde(default)]
    pub vault_address: Option<String>,
    pub action: HyperliquidAction,
}

/// Explicitly supported Hyperliquid exchange actions. Decimal quantities stay
/// strings through canonicalization and signing.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HyperliquidAction {
    Noop {},
    Order {
        orders: Vec<HyperliquidOrder>,
        grouping: HyperliquidOrderGrouping,
        #[serde(default)]
        builder_fee: Option<HyperliquidBuilderFee>,
    },
    Cancel {
        cancels: Vec<HyperliquidCancel>,
    },
    Amend {
        modifies: Vec<HyperliquidAmend>,
    },
    UpdateLeverage {
        asset: u32,
        leverage: u32,
        is_cross: bool,
    },
    UpdateIsolatedMargin {
        asset: u32,
        amount_usdc: String,
    },
    TwapOrder {
        asset: u32,
        is_buy: bool,
        size: String,
        reduce_only: bool,
        duration_minutes: u32,
        randomize: bool,
    },
    TwapCancel {
        asset: u32,
        twap_id: u64,
    },
    ScheduledCancel {
        time_ms: Option<u64>,
    },
    VaultTransfer {
        vault_address: String,
        is_deposit: bool,
        amount_usd_micro: i64,
    },
    CreateVault {
        name: String,
        description: String,
        initial_usd_micro: u64,
    },
    SubAccountTransfer {
        sub_account_user: String,
        is_deposit: bool,
        usd_micro: u64,
    },
    SubAccountSpotTransfer {
        sub_account_user: String,
        is_deposit: bool,
        token: String,
        amount: String,
    },
    CreateSubAccount {
        name: String,
        expires_after_ms: Option<u64>,
    },
    ReserveRequestWeight {
        weight: u32,
        destination: Option<String>,
    },
    SetRef {
        code: String,
    },
    UserAbstraction {
        user: String,
        abstraction: HyperliquidAbstraction,
        agent: bool,
    },
    UserDexAbstraction {
        user: String,
        enabled: bool,
        agent: bool,
    },
    UsdSend {
        destination: String,
        amount_usdc: String,
        time_ms: u64,
    },
    Withdraw {
        destination: String,
        amount_usdc: String,
        time_ms: u64,
    },
    PapertradeCoreDeposit {
        account_mode: String,
        destination: String,
        amount: String,
        nonce: u64,
    },
    SendAsset {
        signature_chain_id: String,
        destination: String,
        source_dex: String,
        destination_dex: String,
        token: String,
        amount: String,
        from_sub_account: String,
        nonce: u64,
    },
    SendToEvmWithData {
        signature_chain_id: String,
        token: String,
        amount: String,
        source_dex: String,
        destination_recipient: String,
        address_encoding: String,
        destination_chain_id: u32,
        gas_limit: u64,
        data: String,
        nonce: u64,
    },
    UsdClassTransfer {
        amount_usdc: String,
        to_perp: bool,
        nonce: u64,
    },
    ApproveBuilderFee {
        builder: String,
        max_fee_rate: String,
        nonce: u64,
    },
}

/// Native action-level Hyperliquid builder metadata. The `f` wire value uses
/// integer tenths of a basis point, unlike `approveBuilderFee.maxFeeRate`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidBuilderFee {
    pub address: String,
    pub fee_tenths_bps: u16,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidOrder {
    pub asset: u32,
    pub is_buy: bool,
    pub price: String,
    pub size: String,
    pub reduce_only: bool,
    pub order_type: HyperliquidOrderType,
    pub client_order_id: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidCancel {
    pub asset: u32,
    pub order_id: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidAmend {
    pub asset: u32,
    pub order_id: u64,
    pub order: HyperliquidOrder,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HyperliquidOrderType {
    Limit {
        time_in_force: HyperliquidTimeInForce,
    },
    Trigger {
        trigger_price: String,
        is_market: bool,
        tpsl: HyperliquidTpsl,
    },
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HyperliquidTimeInForce {
    Gtc,
    Ioc,
    Alo,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HyperliquidOrderGrouping {
    Na,
    NormalTpsl,
    PositionTpsl,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HyperliquidTpsl {
    TakeProfit,
    StopLoss,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HyperliquidAbstraction {
    Disabled,
    UnifiedAccount,
    PortfolioMargin,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HyperliquidRequest {
    Info(HyperliquidInfoRequest),
    Explorer(HyperliquidExplorerRequest),
    Action(HyperliquidActionEnvelope),
}

impl HyperliquidActionEnvelope {
    /// Exact unsigned wire binding covered by the GoFin journal and approval.
    #[cfg(feature = "hyperliquid")]
    pub fn action_wire_json(&self) -> Result<serde_json::Value, CcxtError> {
        if i64::try_from(self.nonce_ms).is_err()
            || self
                .vault_address
                .as_ref()
                .is_some_and(|address| !valid_hl_address(address))
            || (self.vault_address.is_some()
                && matches!(
                    self.action,
                    HyperliquidAction::UsdSend { .. }
                        | HyperliquidAction::Withdraw { .. }
                        | HyperliquidAction::PapertradeCoreDeposit { .. }
                        | HyperliquidAction::SendAsset { .. }
                        | HyperliquidAction::SendToEvmWithData { .. }
                        | HyperliquidAction::UsdClassTransfer { .. }
                        | HyperliquidAction::ApproveBuilderFee { .. }
                        | HyperliquidAction::UserAbstraction { agent: false, .. }
                        | HyperliquidAction::UserDexAbstraction { agent: false, .. }
                ))
        {
            return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
        }
        match &self.action {
            HyperliquidAction::UsdSend { time_ms, .. }
            | HyperliquidAction::Withdraw { time_ms, .. }
                if *time_ms != self.nonce_ms =>
            {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            HyperliquidAction::UsdClassTransfer { nonce, .. }
            | HyperliquidAction::ApproveBuilderFee { nonce, .. }
            | HyperliquidAction::PapertradeCoreDeposit { nonce, .. }
            | HyperliquidAction::SendAsset { nonce, .. }
            | HyperliquidAction::SendToEvmWithData { nonce, .. }
                if *nonce != self.nonce_ms =>
            {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            _ => {}
        }
        let action = hl_action_value(&self.action, self.nonce_ms)?;
        let mut fields = vec![
            ("action", ccxt_value_to_json(action)?),
            ("nonce", serde_json::Value::from(self.nonce_ms)),
        ];
        if let Some(address) = &self.vault_address {
            fields.push((
                "vaultAddress",
                serde_json::Value::String(address.trim_start_matches("0x").to_owned()),
            ));
        } else if matches!(
            self.action,
            HyperliquidAction::UsdSend { .. }
                | HyperliquidAction::Withdraw { .. }
                | HyperliquidAction::PapertradeCoreDeposit { .. }
                | HyperliquidAction::SendAsset { .. }
                | HyperliquidAction::SendToEvmWithData { .. }
                | HyperliquidAction::UsdClassTransfer { .. }
                | HyperliquidAction::ApproveBuilderFee { .. }
                | HyperliquidAction::UserAbstraction { agent: false, .. }
                | HyperliquidAction::UserDexAbstraction { agent: false, .. }
        ) {
            fields.push(("vaultAddress", serde_json::Value::Null));
        }
        if let HyperliquidAction::CreateSubAccount {
            expires_after_ms: Some(time),
            ..
        } = &self.action
        {
            i64::try_from(*time).map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?;
            fields.push(("expiresAfter", serde_json::Value::from(*time)));
        }
        Ok(serde_json::Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        ))
    }
}

#[cfg(feature = "hyperliquid")]
fn ccxt_value_to_json(value: ccxt::Value) -> Result<serde_json::Value, CcxtError> {
    Ok(match value {
        ccxt::Value::Null => serde_json::Value::Null,
        ccxt::Value::Bool(value) => serde_json::Value::Bool(value),
        ccxt::Value::Int(value) => serde_json::Value::from(value),
        ccxt::Value::Float(value) if value.is_finite() => serde_json::Value::from(value),
        ccxt::Value::Float(_) => return Err(CcxtError::new(CcxtErrorKind::InvalidRequest)),
        ccxt::Value::Str(value) => serde_json::Value::String(value.into_owned()),
        ccxt::Value::Arr(values) => serde_json::Value::Array(
            values
                .iter()
                .cloned()
                .map(ccxt_value_to_json)
                .collect::<Result<_, _>>()?,
        ),
        ccxt::Value::Dict(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), ccxt_value_to_json(value.clone())?)))
                .collect::<Result<_, CcxtError>>()?,
        ),
    })
}

#[cfg(feature = "hyperliquid")]
fn zeroize_ccxt_string(value: &mut ccxt::Value) {
    if let ccxt::Value::Str(text) = value {
        text.to_mut().zeroize();
    }
}

#[cfg(feature = "hyperliquid")]
fn hl_map(entries: impl IntoIterator<Item = (String, ccxt::Value)>) -> ccxt::Value {
    ccxt::Value::Map(entries.into_iter().collect::<indexmap::IndexMap<_, _>>())
}

#[cfg(feature = "hyperliquid")]
fn hl_object(entries: impl IntoIterator<Item = (&'static str, ccxt::Value)>) -> ccxt::Value {
    hl_map(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    )
}

#[cfg(feature = "hyperliquid")]
fn hl_str(value: impl Into<String>) -> ccxt::Value {
    ccxt::Value::str(value)
}

#[cfg(feature = "hyperliquid")]
fn hl_user_typed_signature(
    hyperliquid: &mut Observed<HyperliquidCore>,
    private_key: &str,
    primary_type: &'static str,
    fields: Vec<(&'static str, &'static str, ccxt::Value)>,
) -> Result<ccxt::Value, ccxt::ExchangeError> {
    let field_types = ccxt::Value::from(
        fields
            .iter()
            .map(|(name, kind, _)| hl_object([("name", hl_str(*name)), ("type", hl_str(*kind))]))
            .collect::<Vec<_>>(),
    );
    let types = hl_object([(primary_type, field_types)]);
    let message = hl_object(fields.into_iter().map(|(name, _, value)| (name, value)));
    let domain = hl_object([
        ("name", hl_str("HyperliquidSignTransaction")),
        ("version", hl_str("1")),
        ("chainId", ccxt::Value::Int(999)),
        (
            "verifyingContract",
            hl_str("0x0000000000000000000000000000000000000000"),
        ),
    ]);
    let encoded = hyperliquid
        .core
        .eth_encode_structured_data(domain, types, message);
    if !matches!(&encoded, ccxt::Value::Arr(bytes) if bytes.len() == 66 && bytes[0].as_i64() == Some(0x19) && bytes[1].as_i64() == Some(0x01))
    {
        return Err(ccxt::ExchangeError::new(
            "InvalidRequest",
            "Hyperliquid typed-data encoder returned an invalid EIP-712 payload",
        ));
    }
    let mut signing_key = ccxt::Value::str(private_key.to_owned());
    let signature = hyperliquid.core.sign_message(encoded, signing_key.clone());
    zeroize_ccxt_string(&mut signing_key);
    Ok(signature)
}

#[cfg(feature = "hyperliquid")]
fn hl_info_value(request: &HyperliquidInfoRequest) -> Result<ccxt::Value, CcxtError> {
    use HyperliquidInfoRequest as R;
    let mut pairs: Vec<(&'static str, ccxt::Value)> = Vec::new();
    macro_rules! owner {
        ($owner:expr) => {{
            if !valid_hl_address($owner) {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("user", hl_str($owner.clone())));
        }};
    }
    macro_rules! times {
        ($start:expr, $end:expr) => {{
            pairs.push((
                "startTime",
                ccxt::Value::Int(
                    i64::try_from(*$start)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ));
            pairs.push((
                "endTime",
                ccxt::Value::Int(
                    i64::try_from(*$end)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ));
        }};
    }
    let query = match request {
        R::ServerTime => "exchangeStatus",
        R::ExchangeStatus => "exchangeStatus",
        R::MetaAndAssetCtxs => "metaAndAssetCtxs",
        R::MetaAndAssetCtxsForDex { dex } => {
            if dex.trim().is_empty() {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("dex", hl_str(dex.clone())));
            "metaAndAssetCtxs"
        }
        R::SpotMetaAndAssetCtxs => "spotMetaAndAssetCtxs",
        R::PerpDexs => "perpDexs",
        R::AllMids => "allMids",
        R::L2Book { coin } => {
            if coin.is_empty() {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("coin", hl_str(coin.clone())));
            "l2Book"
        }
        R::CandleSnapshot {
            coin,
            interval,
            start_time_ms,
            end_time_ms,
        } => {
            if coin.is_empty() || !valid_hl_interval(interval) || start_time_ms >= end_time_ms {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("coin", hl_str(coin.clone())));
            pairs.push(("interval", hl_str(interval.clone())));
            times!(start_time_ms, end_time_ms);
            "candleSnapshot"
        }
        R::RecentTrades { coin } => {
            if coin.is_empty() {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("coin", hl_str(coin.clone())));
            "recentTrades"
        }
        R::FundingHistory {
            coin,
            start_time_ms,
            end_time_ms,
        } => {
            if coin.is_empty() || start_time_ms >= end_time_ms {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("coin", hl_str(coin.clone())));
            times!(start_time_ms, end_time_ms);
            "fundingHistory"
        }
        R::ClearinghouseState { owner: address } => {
            owner!(address);
            "clearinghouseState"
        }
        R::ClearinghouseStateForDex {
            owner: address,
            dex,
        } => {
            owner!(address);
            if dex.trim().is_empty() {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("dex", hl_str(dex.clone())));
            "clearinghouseState"
        }
        R::SpotClearinghouseState { owner: address } => {
            owner!(address);
            "spotClearinghouseState"
        }
        R::OpenOrders { owner: address } => {
            owner!(address);
            "openOrders"
        }
        R::FrontendOpenOrders { owner: address } => {
            owner!(address);
            "frontendOpenOrders"
        }
        R::FrontendOpenOrdersForDex {
            owner: address,
            dex,
        } => {
            owner!(address);
            if dex.trim().is_empty() || dex.trim() != dex {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("dex", hl_str(dex.clone())));
            "frontendOpenOrders"
        }
        R::OrderStatus {
            owner: address,
            order_id,
        } => {
            owner!(address);
            pairs.push((
                "oid",
                ccxt::Value::Int(
                    i64::try_from(*order_id)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ));
            "orderStatus"
        }
        R::OrderStatusByClientId {
            owner: address,
            client_order_id,
        } => {
            owner!(address);
            if client_order_id.len() != 34
                || !client_order_id.starts_with("0x")
                || !client_order_id.as_bytes()[2..]
                    .iter()
                    .all(u8::is_ascii_hexdigit)
            {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("oid", hl_str(client_order_id.clone())));
            "orderStatus"
        }
        R::HistoricalOrders { owner: address } => {
            owner!(address);
            "historicalOrders"
        }
        R::UserFills { owner: address } => {
            owner!(address);
            "userFills"
        }
        R::UserFillsByTime {
            owner: address,
            start_time_ms,
            end_time_ms,
        } => {
            owner!(address);
            if start_time_ms >= end_time_ms {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            times!(start_time_ms, end_time_ms);
            "userFillsByTime"
        }
        R::UserFunding {
            owner: address,
            start_time_ms,
            end_time_ms,
        } => {
            owner!(address);
            pairs.push((
                "startTime",
                ccxt::Value::Int(
                    i64::try_from(*start_time_ms)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ));
            if let Some(end) = end_time_ms {
                if end < start_time_ms {
                    return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
                }
                pairs.push((
                    "endTime",
                    ccxt::Value::Int(
                        i64::try_from(*end)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ));
            }
            "userFunding"
        }
        R::UserNonFundingLedgerUpdates {
            owner: address,
            start_time_ms,
            end_time_ms,
        } => {
            owner!(address);
            pairs.push((
                "startTime",
                ccxt::Value::Int(
                    i64::try_from(*start_time_ms)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ));
            if let Some(end) = end_time_ms {
                if end < start_time_ms {
                    return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
                }
                pairs.push((
                    "endTime",
                    ccxt::Value::Int(
                        i64::try_from(*end)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ));
            }
            "userNonFundingLedgerUpdates"
        }
        R::UserTwapSliceFills { owner: address } => {
            owner!(address);
            "userTwapSliceFills"
        }
        R::UserRateLimit { owner: address } => {
            owner!(address);
            "userRateLimit"
        }
        R::UserFees { owner: address } => {
            owner!(address);
            "userFees"
        }
        R::ReferralState { owner: address } => {
            owner!(address);
            "referral"
        }
        R::MaxBuilderFee {
            owner: address,
            builder,
        } => {
            owner!(address);
            if !valid_hl_address(builder) {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("builder", hl_str(builder.clone())));
            "maxBuilderFee"
        }
        R::UserRole { owner: address } => {
            owner!(address);
            "userRole"
        }
        R::UserVaultEquities { owner: address } => {
            owner!(address);
            "userVaultEquities"
        }
        R::VaultDetails {
            vault_address,
            user,
        } => {
            if !valid_hl_address(vault_address)
                || user
                    .as_deref()
                    .is_some_and(|address| !valid_hl_address(address))
            {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("vaultAddress", hl_str(vault_address.clone())));
            if let Some(address) = user {
                pairs.push(("user", hl_str(address.clone())));
            }
            "vaultDetails"
        }
        R::SubAccounts { owner: address } => {
            owner!(address);
            "subAccounts"
        }
        R::UserAbstractionState { owner: address } => {
            owner!(address);
            "userAbstraction"
        }
        R::ActiveAssetData {
            owner: address,
            coin,
        } => {
            owner!(address);
            if coin.is_empty() {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            pairs.push(("coin", hl_str(coin.clone())));
            "activeAssetData"
        }
        R::UserDexAbstractionState { owner: address } => {
            owner!(address);
            "userDexAbstraction"
        }
    };
    pairs.insert(0, ("type", hl_str(query)));
    Ok(hl_object(pairs))
}

#[cfg(feature = "hyperliquid")]
fn hl_explorer_value(request: &HyperliquidExplorerRequest) -> Result<ccxt::Value, CcxtError> {
    match request {
        HyperliquidExplorerRequest::TxDetails { hash }
            if hash.len() == 66
                && hash.starts_with("0x")
                && hash.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit) =>
        {
            Ok(hl_object([
                ("type", hl_str("txDetails")),
                ("hash", hl_str(hash.clone())),
            ]))
        }
        HyperliquidExplorerRequest::UserDetails { user } if valid_hl_address(user) => {
            Ok(hl_object([
                ("type", hl_str("userDetails")),
                ("user", hl_str(user.clone())),
            ]))
        }
        _ => Err(CcxtError::new(CcxtErrorKind::InvalidRequest)),
    }
}

#[cfg(feature = "hyperliquid")]
fn valid_hl_interval(interval: &str) -> bool {
    matches!(
        interval,
        "1m" | "3m"
            | "5m"
            | "15m"
            | "30m"
            | "1h"
            | "2h"
            | "4h"
            | "8h"
            | "12h"
            | "1d"
            | "3d"
            | "1w"
            | "1M"
    )
}

#[cfg(feature = "hyperliquid")]
fn valid_hl_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

#[cfg(feature = "hyperliquid")]
fn hl_vault_address(value: Option<&str>) -> ccxt::Value {
    value
        .map(|address| hl_str(address.strip_prefix("0x").unwrap_or(address).to_owned()))
        .unwrap_or(ccxt::Value::Null)
}

#[cfg(feature = "hyperliquid")]
fn valid_hl_decimal(value: &str, allow_zero: bool) -> bool {
    if value.is_empty() || value.starts_with('+') || value.contains(['e', 'E']) {
        return false;
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || parts.next().is_some() {
        return false;
    }
    if fraction.is_some_and(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let zero =
        whole.bytes().all(|b| b == b'0') && fraction.unwrap_or("").bytes().all(|b| b == b'0');
    (allow_zero || !zero) && (!value.starts_with('-') || !zero)
}

#[cfg(feature = "hyperliquid")]
fn is_positive_hl_decimal(value: &str) -> bool {
    valid_hl_decimal(value, false) && !value.starts_with('-')
}

#[cfg(feature = "hyperliquid")]
fn valid_hl_usdc_amount(value: &str) -> bool {
    is_positive_hl_decimal(value)
        && value
            .split_once('.')
            .is_none_or(|(_, fraction)| fraction.len() <= 6)
}

#[cfg(feature = "hyperliquid")]
fn valid_hl_data(value: &str) -> bool {
    value
        .strip_prefix("0x")
        .is_some_and(|hex| hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(feature = "hyperliquid")]
fn hl_order_value(order: &HyperliquidOrder) -> Result<ccxt::Value, CcxtError> {
    if !is_positive_hl_decimal(&order.price)
        || !is_positive_hl_decimal(&order.size)
        || order.client_order_id.as_ref().is_some_and(|id| {
            id.len() != 34
                || !id.starts_with("0x")
                || !id[2..].bytes().all(|b| b.is_ascii_hexdigit())
        })
    {
        return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
    }
    let order_type = match &order.order_type {
        HyperliquidOrderType::Limit { time_in_force } => {
            let tif = match time_in_force {
                HyperliquidTimeInForce::Gtc => "Gtc",
                HyperliquidTimeInForce::Ioc => "Ioc",
                HyperliquidTimeInForce::Alo => "Alo",
            };
            hl_object([("limit", hl_object([("tif", hl_str(tif))]))])
        }
        HyperliquidOrderType::Trigger {
            trigger_price,
            is_market,
            tpsl,
        } => {
            if !is_positive_hl_decimal(trigger_price) {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            let tpsl = match tpsl {
                HyperliquidTpsl::TakeProfit => "tp",
                HyperliquidTpsl::StopLoss => "sl",
            };
            hl_object([(
                "trigger",
                hl_object([
                    ("isMarket", ccxt::Value::Bool(*is_market)),
                    ("triggerPx", hl_str(trigger_price.clone())),
                    ("tpsl", hl_str(tpsl)),
                ]),
            )])
        }
    };
    let mut entries = vec![
        ("a", ccxt::Value::Int(order.asset as i64)),
        ("b", ccxt::Value::Bool(order.is_buy)),
        ("p", hl_str(order.price.clone())),
        ("s", hl_str(order.size.clone())),
        ("r", ccxt::Value::Bool(order.reduce_only)),
        ("t", order_type),
    ];
    if let Some(cloid) = &order.client_order_id {
        entries.push(("c", hl_str(cloid.clone())));
    }
    Ok(hl_object(entries))
}

#[cfg(feature = "hyperliquid")]
fn hl_action_value(action: &HyperliquidAction, nonce_ms: u64) -> Result<ccxt::Value, CcxtError> {
    use HyperliquidAction as A;
    Ok(match action {
        A::Noop {} => hl_object([("type", hl_str("noop"))]),
        A::Order {
            orders,
            grouping,
            builder_fee,
        } if !orders.is_empty() => {
            let grouping = match grouping {
                HyperliquidOrderGrouping::Na => "na",
                HyperliquidOrderGrouping::NormalTpsl => "normalTpsl",
                HyperliquidOrderGrouping::PositionTpsl => "positionTpsl",
            };
            let orders = orders
                .iter()
                .map(hl_order_value)
                .collect::<Result<Vec<_>, _>>()?;
            let mut entries = vec![
                ("type", hl_str("order")),
                ("orders", ccxt::Value::Array(orders)),
                ("grouping", hl_str(grouping)),
            ];
            if let Some(builder) = builder_fee {
                if !valid_hl_address(&builder.address)
                    || builder.fee_tenths_bps == 0
                    || builder.fee_tenths_bps > 1000
                {
                    return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
                }
                entries.push((
                    "builder",
                    hl_object([
                        ("b", hl_str(builder.address.clone())),
                        ("f", ccxt::Value::Int(i64::from(builder.fee_tenths_bps))),
                    ]),
                ));
            }
            hl_object(entries)
        }
        A::Cancel { cancels } if !cancels.is_empty() => {
            let cancels = cancels
                .iter()
                .map(|c| {
                    Ok(hl_object([
                        ("a", ccxt::Value::Int(c.asset as i64)),
                        (
                            "o",
                            ccxt::Value::Int(
                                i64::try_from(c.order_id)
                                    .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                            ),
                        ),
                    ]))
                })
                .collect::<Result<Vec<_>, CcxtError>>()?;
            hl_object([
                ("type", hl_str("cancel")),
                ("cancels", ccxt::Value::Array(cancels)),
            ])
        }
        A::Amend { modifies } if !modifies.is_empty() => {
            let modifies = modifies
                .iter()
                .map(|m| {
                    if m.asset != m.order.asset {
                        return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
                    }
                    Ok(hl_object([
                        (
                            "oid",
                            ccxt::Value::Int(
                                i64::try_from(m.order_id)
                                    .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                            ),
                        ),
                        ("order", hl_order_value(&m.order)?),
                    ]))
                })
                .collect::<Result<Vec<_>, CcxtError>>()?;
            hl_object([
                ("type", hl_str("batchModify")),
                ("modifies", ccxt::Value::Array(modifies)),
            ])
        }
        A::UpdateLeverage {
            asset,
            leverage,
            is_cross,
        } if *leverage > 0 => hl_object([
            ("type", hl_str("updateLeverage")),
            ("asset", ccxt::Value::Int(*asset as i64)),
            ("isCross", ccxt::Value::Bool(*is_cross)),
            ("leverage", ccxt::Value::Int(*leverage as i64)),
        ]),
        A::UpdateIsolatedMargin { asset, amount_usdc } if valid_hl_decimal(amount_usdc, false) => {
            // Hyperliquid's action expresses margin change in integer USD
            // micro-units. isBuy is reserved for future hedge mode and stays true;
            // the sign of ntli selects addition/removal.
            if amount_usdc.split('.').nth(1).is_some_and(|f| f.len() > 6) {
                return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
            }
            let amount = decimal_usdc_to_micro(amount_usdc)?;
            let amount = amount
                .parse::<i64>()
                .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?;
            hl_object([
                ("type", hl_str("updateIsolatedMargin")),
                ("asset", ccxt::Value::Int(*asset as i64)),
                ("isBuy", ccxt::Value::Bool(true)),
                ("ntli", ccxt::Value::Int(amount)),
            ])
        }
        A::TwapOrder {
            asset,
            is_buy,
            size,
            reduce_only,
            duration_minutes,
            randomize,
        } if is_positive_hl_decimal(size) && *duration_minutes > 0 => hl_object([
            ("type", hl_str("twapOrder")),
            (
                "twap",
                hl_object([
                    ("a", ccxt::Value::Int(*asset as i64)),
                    ("b", ccxt::Value::Bool(*is_buy)),
                    ("s", hl_str(size.clone())),
                    ("r", ccxt::Value::Bool(*reduce_only)),
                    ("m", ccxt::Value::Int(*duration_minutes as i64)),
                    ("t", ccxt::Value::Bool(*randomize)),
                ]),
            ),
        ]),
        A::TwapCancel { asset, twap_id } => hl_object([
            ("type", hl_str("twapCancel")),
            ("a", ccxt::Value::Int(*asset as i64)),
            (
                "t",
                ccxt::Value::Int(
                    i64::try_from(*twap_id)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ),
        ]),
        A::ScheduledCancel { time_ms } => {
            let mut fields = vec![("type", hl_str("scheduledCancel"))];
            if let Some(time) = time_ms {
                fields.push((
                    "time",
                    ccxt::Value::Int(
                        i64::try_from(*time)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ));
            }
            hl_object(fields)
        }
        A::VaultTransfer {
            vault_address,
            is_deposit,
            amount_usd_micro,
        } if valid_hl_address(vault_address) && *amount_usd_micro > 0 => hl_object([
            ("type", hl_str("vaultTransfer")),
            ("vaultAddress", hl_str(vault_address.clone())),
            ("isDeposit", ccxt::Value::Bool(*is_deposit)),
            ("usd", ccxt::Value::Int(*amount_usd_micro)),
        ]),
        A::CreateVault {
            name,
            description,
            initial_usd_micro,
        } if !name.trim().is_empty()
            && !description.trim().is_empty()
            && *initial_usd_micro > 0 =>
        {
            hl_object([
                ("type", hl_str("createVault")),
                ("name", hl_str(name.clone())),
                ("description", hl_str(description.clone())),
                (
                    "initialUsd",
                    ccxt::Value::Int(
                        i64::try_from(*initial_usd_micro)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ),
            ])
        }
        A::SubAccountTransfer {
            sub_account_user,
            is_deposit,
            usd_micro,
        } if valid_hl_address(sub_account_user) && *usd_micro > 0 => hl_object([
            ("type", hl_str("subAccountTransfer")),
            ("subAccountUser", hl_str(sub_account_user.clone())),
            ("isDeposit", ccxt::Value::Bool(*is_deposit)),
            (
                "usd",
                ccxt::Value::Int(
                    i64::try_from(*usd_micro)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ),
        ]),
        A::SubAccountSpotTransfer {
            sub_account_user,
            is_deposit,
            token,
            amount,
        } if valid_hl_address(sub_account_user)
            && !token.is_empty()
            && is_positive_hl_decimal(amount) =>
        {
            hl_object([
                ("type", hl_str("subAccountSpotTransfer")),
                ("subAccountUser", hl_str(sub_account_user.clone())),
                ("isDeposit", ccxt::Value::Bool(*is_deposit)),
                ("token", hl_str(token.clone())),
                ("amount", hl_str(amount.clone())),
            ])
        }
        A::CreateSubAccount { name, .. } if !name.trim().is_empty() => hl_object([
            ("type", hl_str("createSubAccount")),
            ("name", hl_str(name.clone())),
        ]),
        A::ReserveRequestWeight {
            weight,
            destination,
        } if *weight > 0 => {
            let mut fields = vec![
                ("type", hl_str("reserveRequestWeight")),
                ("weight", ccxt::Value::Int(*weight as i64)),
            ];
            if let Some(destination) = destination {
                if !valid_hl_address(destination) {
                    return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
                }
                fields.push(("destination", hl_str(destination.clone())));
            }
            hl_object(fields)
        }
        A::SetRef { code } if !code.trim().is_empty() => hl_object([
            ("type", hl_str("setReferrer")),
            ("code", hl_str(code.clone())),
        ]),
        A::PapertradeCoreDeposit {
            account_mode,
            destination,
            amount,
            nonce,
        } if matches!(account_mode.as_str(), "default" | "unified_account")
            && valid_hl_address(destination)
            && valid_hl_usdc_amount(amount)
            && *nonce == nonce_ms =>
        {
            if account_mode == "default" {
                hl_object([
                    ("type", hl_str("spotSend")),
                    ("signatureChainId", hl_str("0x3e7")),
                    ("hyperliquidChain", hl_str("Mainnet")),
                    ("destination", hl_str(destination.to_ascii_lowercase())),
                    ("token", hl_str("USDC")),
                    ("amount", hl_str(amount.clone())),
                    (
                        "time",
                        ccxt::Value::Int(
                            i64::try_from(*nonce)
                                .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                        ),
                    ),
                ])
            } else {
                hl_object([
                    ("type", hl_str("sendAsset")),
                    ("signatureChainId", hl_str("0x3e7")),
                    ("hyperliquidChain", hl_str("Mainnet")),
                    ("destination", hl_str(destination.to_ascii_lowercase())),
                    ("sourceDex", hl_str("spot")),
                    ("destinationDex", hl_str("spot")),
                    ("token", hl_str("USDC")),
                    ("amount", hl_str(amount.clone())),
                    ("fromSubAccount", hl_str("")),
                    (
                        "nonce",
                        ccxt::Value::Int(
                            i64::try_from(*nonce)
                                .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                        ),
                    ),
                ])
            }
        }
        A::SendAsset {
            signature_chain_id,
            destination,
            source_dex,
            destination_dex,
            token,
            amount,
            from_sub_account,
            nonce,
        } if signature_chain_id.eq_ignore_ascii_case("0x3e7")
            && destination.eq_ignore_ascii_case("0x2000000000000000000000000000000000000000")
            && source_dex == "spot"
            && destination_dex == "spot"
            && token == "USDC"
            && valid_hl_usdc_amount(amount)
            && from_sub_account.is_empty()
            && *nonce == nonce_ms =>
        {
            hl_object([
                ("type", hl_str("sendAsset")),
                ("signatureChainId", hl_str("0x3e7")),
                ("hyperliquidChain", hl_str("Mainnet")),
                ("destination", hl_str(destination.to_ascii_lowercase())),
                ("sourceDex", hl_str("spot")),
                ("destinationDex", hl_str("spot")),
                ("token", hl_str("USDC")),
                ("amount", hl_str(amount.clone())),
                ("fromSubAccount", hl_str("")),
                (
                    "nonce",
                    ccxt::Value::Int(
                        i64::try_from(*nonce)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ),
            ])
        }
        A::SendToEvmWithData {
            signature_chain_id,
            token,
            amount,
            source_dex,
            destination_recipient,
            address_encoding,
            destination_chain_id,
            gas_limit,
            data,
            nonce,
        } if signature_chain_id.eq_ignore_ascii_case("0x3e7")
            && token == "USDC"
            && valid_hl_usdc_amount(amount)
            && source_dex == "spot"
            && matches!(*destination_chain_id, 0 | 3 | 5)
            && *gas_limit > 0
            && valid_hl_data(data)
            && *nonce == nonce_ms
            && (match address_encoding.as_str() {
                "hex" => valid_hl_address(destination_recipient),
                "base58" => bs58::decode(destination_recipient)
                    .into_vec()
                    .is_ok_and(|b| b.len() == 32),
                _ => false,
            }) =>
        {
            hl_object([
                ("type", hl_str("sendToEvmWithData")),
                ("signatureChainId", hl_str("0x3e7")),
                ("hyperliquidChain", hl_str("Mainnet")),
                ("token", hl_str("USDC")),
                ("amount", hl_str(amount.clone())),
                ("sourceDex", hl_str("spot")),
                (
                    "destinationRecipient",
                    hl_str(destination_recipient.clone()),
                ),
                ("addressEncoding", hl_str(address_encoding.clone())),
                (
                    "destinationChainId",
                    ccxt::Value::Int(i64::from(*destination_chain_id)),
                ),
                (
                    "gasLimit",
                    ccxt::Value::Int(
                        i64::try_from(*gas_limit)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ),
                ("data", hl_str(data.clone())),
                (
                    "nonce",
                    ccxt::Value::Int(
                        i64::try_from(*nonce)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ),
            ])
        }
        A::UserAbstraction {
            user,
            abstraction,
            agent,
        } if valid_hl_address(user) => {
            let abstraction = hl_abstraction_value(*abstraction);
            if *agent {
                hl_object([
                    ("type", hl_str("agentSetAbstraction")),
                    (
                        "abstraction",
                        hl_str(match abstraction.as_str() {
                            Some("disabled") => "i",
                            Some("unifiedAccount") => "u",
                            Some("portfolioMargin") => "p",
                            _ => unreachable!("closed abstraction enum"),
                        }),
                    ),
                ])
            } else {
                hl_object([
                    ("hyperliquidChain", hl_str("Mainnet")),
                    ("signatureChainId", hl_str("0x66eee")),
                    ("abstraction", abstraction),
                    ("user", hl_str(user.clone())),
                    (
                        "nonce",
                        ccxt::Value::Int(
                            i64::try_from(nonce_ms)
                                .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                        ),
                    ),
                    ("type", hl_str("userSetAbstraction")),
                ])
            }
        }
        A::UserDexAbstraction {
            user,
            enabled: true,
            agent: true,
        } if valid_hl_address(user) => hl_object([("type", hl_str("agentEnableDexAbstraction"))]),
        A::UserDexAbstraction {
            user,
            enabled,
            agent: false,
        } if valid_hl_address(user) => hl_object([
            ("hyperliquidChain", hl_str("Mainnet")),
            ("signatureChainId", hl_str("0x66eee")),
            ("enabled", ccxt::Value::Bool(*enabled)),
            ("user", hl_str(user.clone())),
            (
                "nonce",
                ccxt::Value::Int(
                    i64::try_from(nonce_ms)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ),
            ("type", hl_str("userDexAbstraction")),
        ]),
        A::UsdSend {
            destination,
            amount_usdc,
            time_ms,
        } if valid_hl_address(destination)
            && valid_hl_decimal(amount_usdc, false)
            && !amount_usdc.starts_with('-') =>
        {
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("signatureChainId", hl_str("0x66eee")),
                ("destination", hl_str(destination.clone())),
                ("amount", hl_str(amount_usdc.clone())),
                (
                    "time",
                    ccxt::Value::Int(
                        i64::try_from(*time_ms)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ),
                ("type", hl_str("usdSend")),
            ])
        }
        A::Withdraw {
            destination,
            amount_usdc,
            time_ms,
        } if valid_hl_address(destination)
            && valid_hl_decimal(amount_usdc, false)
            && !amount_usdc.starts_with('-') =>
        {
            hl_object([
                ("hyperliquidChain", hl_str("Mainnet")),
                ("signatureChainId", hl_str("0x66eee")),
                ("destination", hl_str(destination.clone())),
                ("amount", hl_str(amount_usdc.clone())),
                (
                    "time",
                    ccxt::Value::Int(
                        i64::try_from(*time_ms)
                            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                    ),
                ),
                ("type", hl_str("withdraw3")),
            ])
        }
        A::UsdClassTransfer {
            amount_usdc,
            to_perp,
            nonce,
        } if is_positive_hl_decimal(amount_usdc) => hl_object([
            ("hyperliquidChain", hl_str("Mainnet")),
            ("signatureChainId", hl_str("0x66eee")),
            ("type", hl_str("usdClassTransfer")),
            ("amount", hl_str(amount_usdc.clone())),
            ("toPerp", ccxt::Value::Bool(*to_perp)),
            (
                "nonce",
                ccxt::Value::Int(
                    i64::try_from(*nonce)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ),
        ]),
        A::ApproveBuilderFee {
            builder,
            max_fee_rate,
            nonce,
        } if valid_hl_address(builder) && valid_hl_percent(max_fee_rate) => hl_object([
            ("hyperliquidChain", hl_str("Mainnet")),
            ("signatureChainId", hl_str("0x66eee")),
            ("maxFeeRate", hl_str(max_fee_rate.clone())),
            ("builder", hl_str(builder.clone())),
            (
                "nonce",
                ccxt::Value::Int(
                    i64::try_from(*nonce)
                        .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?,
                ),
            ),
            ("type", hl_str("approveBuilderFee")),
        ]),
        _ => return Err(CcxtError::new(CcxtErrorKind::InvalidRequest)),
    })
}

#[cfg(feature = "hyperliquid")]
fn hl_abstraction_value(value: HyperliquidAbstraction) -> ccxt::Value {
    hl_str(match value {
        HyperliquidAbstraction::Disabled => "disabled",
        HyperliquidAbstraction::UnifiedAccount => "unifiedAccount",
        HyperliquidAbstraction::PortfolioMargin => "portfolioMargin",
    })
}

#[cfg(feature = "hyperliquid")]
fn valid_hl_percent(value: &str) -> bool {
    value.strip_suffix('%').map_or_else(
        || valid_hl_decimal(value, true),
        |number| valid_hl_decimal(number, true),
    )
}

#[cfg(feature = "hyperliquid")]
fn decimal_usdc_to_micro(value: &str) -> Result<String, CcxtError> {
    let negative = value.starts_with('-');
    let positive = value.trim_start_matches('-');
    let (whole, fraction) = positive.split_once('.').unwrap_or((positive, ""));
    if !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 6
    {
        return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
    }
    let digits = format!("{}{:0<6}", whole, fraction);
    let micro = digits.trim_start_matches('0');
    let micro = if micro.is_empty() { "0" } else { micro };
    Ok(if negative {
        format!("-{micro}")
    } else {
        micro.to_owned()
    })
}

pub trait CredentialProvider: Send + Sync {
    fn credentials(&self, reference: &str) -> Result<Credentials, CcxtError>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcxtErrorKind {
    Unavailable,
    RateLimited,
    PermissionDenied,
    Authentication,
    Timeout,
    Rejected,
    Unsupported,
    InvalidResponse,
    InvalidRequest,
}
/// Closed Funding error codes; provider messages and unrecognized codes never cross
/// the transport boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingErrorCode {
    InvalidArgument,
    InsufficientFunds,
    PermissionDenied,
    Unauthorized,
    NotFound,
    Internal,
    Unavailable,
}
impl FundingErrorCode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "invalid_argument" => Self::InvalidArgument,
            "insufficient_funds" => Self::InsufficientFunds,
            "permission_denied" => Self::PermissionDenied,
            "unauthorized" => Self::Unauthorized,
            "not_found" => Self::NotFound,
            "internal" => Self::Internal,
            "unavailable" => Self::Unavailable,
            _ => return None,
        })
    }
}
/// Only closed safe categories and numeric exchange evidence cross the CCXT boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CcxtError {
    pub kind: CcxtErrorKind,
    pub http_status: Option<u16>,
    pub exchange_code: Option<i64>,
    pub funding_error_code: Option<FundingErrorCode>,
}

#[cfg(feature = "hyperliquid")]
fn is_native_hyperliquid_action_rejection(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("status").and_then(Value::as_str) == Some("err")
        && object
            .get("response")
            .and_then(Value::as_str)
            .is_some_and(|message| !message.trim().is_empty())
}
impl CcxtError {
    pub fn new(kind: CcxtErrorKind) -> Self {
        Self {
            kind,
            http_status: None,
            exchange_code: None,
            funding_error_code: None,
        }
    }
}
impl fmt::Display for CcxtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Exchange request failed ({:?})", self.kind)
    }
}
impl std::error::Error for CcxtError {}

#[cfg_attr(
    not(any(
        feature = "binance",
        feature = "kraken",
        feature = "hyperliquid",
        feature = "okx",
        feature = "coinbase",
        feature = "bybit"
    )),
    allow(dead_code)
)]
pub type RequestGuard = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

struct Job {
    exchange: ExchangeId,
    method: Method,
    credentials: Option<Credentials>,
    params: Vec<(String, String)>,
    hyperliquid: Option<HyperliquidRequest>,
    hyperliquid_signer: Option<HyperliquidSigner>,
    reply: oneshot::Sender<Result<Value, CcxtError>>,
    guard: Option<RequestGuard>,
}
#[derive(Clone)]
pub struct CcxtClient {
    sender: mpsc::Sender<Job>,
}

/// Observe CCXT's existing error-handler hook, without replacing HTTP, signing,
/// endpoint dispatch, throttling or exchange error interpretation.
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
struct Observed<T> {
    core: T,
    response: std::sync::Arc<std::sync::Mutex<Option<(u16, String)>>>,
    guard: std::sync::Arc<std::sync::Mutex<Option<RequestGuard>>>,
}
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
impl<T: ExchangeBase> std::ops::Deref for Observed<T> {
    type Target = ccxt::Exchange;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
impl<T: ExchangeBase> std::ops::DerefMut for Observed<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
impl<T: ExchangeBase> DerivedExchange for Observed<T> {
    fn nonce(&self) -> ccxt::Value {
        DerivedExchange::nonce(&self.core)
    }
    fn sign(
        &self,
        path: ccxt::Value,
        api: ccxt::Value,
        method: ccxt::Value,
        params: ccxt::Value,
        headers: ccxt::Value,
        body: ccxt::Value,
    ) -> ccxt::Value {
        if self
            .guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|guard| !guard())
        {
            panic!("[PermissionDenied] GoFin write approval expired before signing");
        }
        let universal_transfer_post = matches!(
            (&path, &api, &method),
            (
                ccxt::Value::Str(path),
                ccxt::Value::Str(api),
                ccxt::Value::Str(method)
            ) if path.as_ref() == "sub-account/universalTransfer"
                && api.as_ref() == "sapi"
                && method.as_ref() == "POST"
        );
        let mut signed =
            DerivedExchange::sign(&self.core, path, api, method, params, headers, body);
        if universal_transfer_post {
            // CCXT signs the URL-encoded payload before returning it as a POST
            // body. Binance also accepts that exact signed payload in the URL
            // query. Move the bytes CCXT produced; never re-encode or re-sign.
            let ccxt::Value::Dict(fields) = &mut signed else {
                panic!("[InvalidRequest] Binance transfer signing result was invalid");
            };
            let fields = std::sync::Arc::make_mut(fields);
            let (url, payload) = match (fields.get("url"), fields.get("body")) {
                (Some(ccxt::Value::Str(url)), Some(ccxt::Value::Str(payload)))
                    if !url.contains('?') && !payload.is_empty() =>
                {
                    (url.to_string(), payload.to_string())
                }
                _ => panic!("[InvalidRequest] Binance transfer signing result was invalid"),
            };
            fields.insert(
                "url".to_owned(),
                ccxt::Value::from(format!("{url}?{payload}")),
            );
            fields.insert("body".to_owned(), ccxt::Value::Null);
            if let Some(ccxt::Value::Dict(headers)) = fields.get_mut("headers") {
                let headers = std::sync::Arc::make_mut(headers);
                headers.retain(|name, _| !name.eq_ignore_ascii_case("Content-Type"));
            }
        }
        signed
    }
    fn handle_errors(
        &self,
        code: ccxt::Value,
        reason: ccxt::Value,
        url: ccxt::Value,
        method: ccxt::Value,
        headers: ccxt::Value,
        body: ccxt::Value,
        response: ccxt::Value,
        request_headers: ccxt::Value,
        request_body: ccxt::Value,
    ) -> ccxt::Value {
        if let (ccxt::Value::Int(status), ccxt::Value::Str(text)) = (&code, &body) {
            *self.response.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((*status as u16, text.to_string()));
        }
        DerivedExchange::handle_errors(
            &self.core,
            code,
            reason,
            url,
            method,
            headers,
            body,
            response,
            request_headers,
            request_body,
        )
    }
}
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
impl<T: ExchangeBase> ExchangeBase for Observed<T> {
    fn call_dynamic<'a>(
        &'a mut self,
        method: &'a str,
        args: Vec<ccxt::Value>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ccxt::Value> + Send + 'a>> {
        self.core.call_dynamic(method, args)
    }
}

#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
thread_local! { static IN_CCXT: Cell<bool> = const { Cell::new(false) }; }
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
pub(crate) fn install_panic_redaction() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !IN_CCXT.get() {
                previous(info);
            }
        }));
    });
}
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
pub(crate) struct PanicRedaction;
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
impl PanicRedaction {
    pub(crate) fn enter() -> Self {
        IN_CCXT.set(true);
        Self
    }
}
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
impl Drop for PanicRedaction {
    fn drop(&mut self) {
        IN_CCXT.set(false);
    }
}

impl CcxtClient {
    pub fn new() -> Result<Self, CcxtError> {
        Self::start(None, Duration::from_secs(15))
    }
    /// Synthetic HTTP fixtures only; never usable with non-loopback hosts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(endpoint: &str) -> Result<Self, CcxtError> {
        Self::for_test_with_timeout(endpoint, Duration::from_secs(5))
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test_with_timeout(endpoint: &str, timeout: Duration) -> Result<Self, CcxtError> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| CcxtError::new(CcxtErrorKind::InvalidRequest))?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
        }
        Self::start(Some(endpoint.trim_end_matches('/').to_owned()), timeout)
    }
    #[cfg(not(any(
        feature = "binance",
        feature = "kraken",
        feature = "hyperliquid",
        feature = "okx",
        feature = "coinbase",
        feature = "bybit"
    )))]
    fn start(_endpoint: Option<String>, _timeout: Duration) -> Result<Self, CcxtError> {
        Err(CcxtError::new(CcxtErrorKind::Unsupported))
    }
    #[cfg(any(
        feature = "binance",
        feature = "kraken",
        feature = "hyperliquid",
        feature = "okx",
        feature = "coinbase",
        feature = "bybit"
    ))]
    fn start(endpoint: Option<String>, timeout: Duration) -> Result<Self, CcxtError> {
        install_panic_redaction();
        let client = reqwest::Client::builder()
            .user_agent(concat!("gofin-cli/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(timeout + Duration::from_secs(1))
            .build()
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?;
        let (sender, mut receiver) = mpsc::channel::<Job>(32);
        #[cfg(feature = "hyperliquid")]
        let hyperliquid_explorer_endpoint = endpoint
            .clone()
            .unwrap_or_else(|| "https://rpc.hyperliquid.xyz".to_owned());
        std::thread::Builder::new().name("gofin-ccxt".into()).spawn(move || runtime.block_on(async move {
            let _redaction = PanicRedaction::enter();
            let guard = std::sync::Arc::new(std::sync::Mutex::new(None));
            let response = std::sync::Arc::new(std::sync::Mutex::new(None));
            let config = ccxt::Value::from(serde_json::json!({"enableRateLimit":true,"verbose":false,"maxRetriesOnFailure":0,"options":{"maxRetriesOnFailure":0,"adjustForTimeDifference":false,"recvWindow":5000}}));
            #[cfg(feature="binance")]
            let mut binance = Observed { core: BinanceCore::new(Some(config.clone())), response: response.clone(), guard:guard.clone() };
            #[cfg(feature="kraken")]
            let mut kraken = Observed { core: KrakenCore::new(Some(config.clone())), response: response.clone(), guard:guard.clone() };
            #[cfg(feature="hyperliquid")]
            let mut hyperliquid = Observed { core:HyperliquidCore::new(Some(config.clone())), response:response.clone(),guard:guard.clone() };
            #[cfg(feature="hyperliquid")]
            { hyperliquid.internals.http_client = Some(client.clone()); }
            #[cfg(feature="okx")]
            let mut okx = Observed { core:OkxCore::new(Some(config.clone())), response:response.clone(),guard:guard.clone() };
            #[cfg(feature="okx")]
            { okx.internals.http_client = Some(client.clone()); }
            #[cfg(feature="coinbase")]
            let mut coinbase = Observed { core:CoinbaseCore::new(Some(config.clone())), response:response.clone(),guard:guard.clone() };
            #[cfg(feature="coinbase")]
            { coinbase.internals.http_client = Some(client.clone()); }
            #[cfg(feature="bybit")]
            let mut bybit = Observed { core:BybitCore::new(Some(config.clone())), response:response.clone(),guard:guard.clone() };
            #[cfg(feature="bybit")]
            { bybit.internals.http_client = Some(client.clone()); }
            #[cfg(feature="hyperliquid")]
            let mut last_explorer_request: Option<tokio::time::Instant> = None;
            #[cfg(feature="binance")]
            { binance.internals.http_client = Some(client.clone()); }
            #[cfg(feature="kraken")]
            { kraken.internals.http_client = Some(client.clone()); }
            if let Some(base) = endpoint {
                #[cfg(feature="binance")]
                { binance.urls = ccxt::Value::from(serde_json::json!({"api":{"public":format!("{base}/api/v3"),"private":format!("{base}/api/v3"),"sapi":format!("{base}/sapi/v1"),"sapiV4":format!("{base}/sapi/v4"),"fapiPublic":format!("{base}/fapi/v1"),"fapiPrivate":format!("{base}/fapi/v1"),"fapiPrivateV3":format!("{base}/fapi/v3")}})); }
                #[cfg(feature="kraken")]
                { kraken.urls = ccxt::Value::from(serde_json::json!({"api":{"public":base}})); }
                #[cfg(feature="hyperliquid")]
                { hyperliquid.urls = ccxt::Value::from(serde_json::json!({"api":{"public":base,"private":base}})); }
                #[cfg(feature="okx")]
                { okx.urls = ccxt::Value::from(serde_json::json!({"api":{"rest":base}})); }
                #[cfg(feature="coinbase")]
                { coinbase.urls = ccxt::Value::from(serde_json::json!({"api":{"rest":base}})); }
                #[cfg(feature="bybit")]
                { bybit.urls = ccxt::Value::from(serde_json::json!({"api":{"public":base}})); }
            }
            while let Some(job) = receiver.recv().await {
                if job.reply.is_closed() { continue; }
                let params = ccxt::Value::Map(job.params.into_iter().map(|(k,v)| {
                    // CCXT's Binance rawencode branch preserves parameter text
                    // verbatim. Escape data before that branch signs the wire form.
                    let v = if job.method.raw_encoded() { wire_component(&v) } else { v };
                    (k,ccxt::Value::Str(v.into()))
                }).collect());
                let credentials = match normalize_credentials(job.credentials) {
                    Ok(value) => value,
                    Err(error) => { let _ = job.reply.send(Err(error)); continue; }
                };
                let exchange: &mut ccxt::Exchange = match job.exchange { #[cfg(feature="binance")] ExchangeId::Binance => &mut binance.core.exchange, #[cfg(feature="kraken")] ExchangeId::Kraken => &mut kraken.core.exchange, #[cfg(feature="hyperliquid")] ExchangeId::Hyperliquid => &mut hyperliquid.core.exchange, #[cfg(feature="okx")] ExchangeId::Okx => &mut okx.core.exchange, #[cfg(feature="coinbase")] ExchangeId::Coinbase => &mut coinbase.core.exchange, #[cfg(feature="bybit")] ExchangeId::Bybit => &mut bybit.core.exchange, #[allow(unreachable_patterns)] _ => { let _ = job.reply.send(Err(CcxtError::new(CcxtErrorKind::Unsupported))); continue; } };
                exchange.apiKey = credentials.as_ref().map(|c| ccxt::Value::Str(c.api_key.to_string().into())).unwrap_or_default();
                exchange.secret = credentials.as_ref().map(|c| ccxt::Value::Str(c.secret.to_string().into())).unwrap_or_default();
                #[cfg(feature="hyperliquid")]
                if let Some(signer) = job.hyperliquid_signer.as_ref() {
                    hyperliquid.core.privateKey = ccxt::Value::str(signer.private_key.to_string());
                    hyperliquid.core.walletAddress = ccxt::Value::str(signer.wallet_address.clone());
                }
                *response.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *guard.lock().unwrap_or_else(|e|e.into_inner()) = job.guard;
                #[cfg(feature="hyperliquid")]
                let valid_response_shape = |value: &Value| match job.hyperliquid.as_ref() {
                    Some(HyperliquidRequest::Info(
                        HyperliquidInfoRequest::UserAbstractionState { .. },
                    )) => value.is_string(),
                    Some(HyperliquidRequest::Info(
                        HyperliquidInfoRequest::UserDexAbstractionState { .. },
                    )) => value.is_boolean() || value.is_null(),
                    Some(HyperliquidRequest::Info(
                        HyperliquidInfoRequest::MaxBuilderFee { .. },
                    )) => value.as_u64().is_some(),
                    _ => value.is_object() || value.is_array(),
                };
                #[cfg(not(feature="hyperliquid"))]
                let valid_response_shape = |value: &Value| value.is_object() || value.is_array();
                let request = async {
                    #[cfg(feature="hyperliquid")]
                    if let Some(typed_request) = job.hyperliquid.as_ref() {
                        match typed_request {
                            HyperliquidRequest::Info(info) => {
                                let payload = hl_info_value(info).map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid info request"))?;
                                return ccxt::runtime::call_typed(hyperliquid.implicit_api_call("public_post_info", payload)).await;
                            }
                            HyperliquidRequest::Explorer(explorer) => {
                                let payload = hl_explorer_value(explorer).map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid explorer request"))?;
                                let payload = ccxt_value_to_json(payload).map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid explorer request"))?;
                                if let Some(last) = last_explorer_request {
                                    // Explorer calls cost 40 IP-rate-limit units; the
                                    // documented aggregate budget is 1200/minute.
                                    let remaining = Duration::from_secs(2).saturating_sub(last.elapsed());
                                    if !remaining.is_zero() { tokio::time::sleep(remaining).await; }
                                }
                                last_explorer_request = Some(tokio::time::Instant::now());
                                let http_response = client
                                    .post(format!("{}/explorer", hyperliquid_explorer_endpoint.trim_end_matches('/')))
                                    .json(&payload)
                                    .send()
                                    .await
                                    .map_err(|error| ccxt::ExchangeError::new(if error.is_timeout() { "RequestTimeout" } else { "NetworkError" }, "Hyperliquid explorer request failed"))?;
                                let status = http_response.status();
                                let bytes = http_response.bytes().await.map_err(|_| ccxt::ExchangeError::new("NetworkError", "Hyperliquid explorer response was unavailable"))?;
                                let text = String::from_utf8_lossy(&bytes).into_owned();
                                *response.lock().unwrap_or_else(|e| e.into_inner()) = Some((status.as_u16(), text.clone()));
                                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                                    return Err(ccxt::ExchangeError::new("RateLimitExceeded", "Hyperliquid explorer rate limit reached"));
                                }
                                if !status.is_success() {
                                    return Err(ccxt::ExchangeError::new("ExchangeError", "Hyperliquid explorer rejected the typed read"));
                                }
                                let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| ccxt::ExchangeError::new("InvalidResponse", "Hyperliquid explorer returned invalid JSON"))?;
                                return Ok(Ok(ccxt::Value::from(value)));
                            }
                            HyperliquidRequest::Action(envelope) => {
                                let Some(signer) = job.hyperliquid_signer.as_ref() else { return Err(ccxt::ExchangeError::new("AuthenticationError", "missing Hyperliquid signer")); };
                                let mut key = ccxt::Value::str(signer.private_key.to_string());
                                let derived = hyperliquid.eth_get_address_from_private_key(key.clone(), &[]);
                                zeroize_ccxt_string(&mut key);
                                let derived = derived.as_str().unwrap_or_default();
                                if !valid_hl_address(&signer.wallet_address) || !derived.eq_ignore_ascii_case(&signer.wallet_address) {
                                    return Err(ccxt::ExchangeError::new("AuthenticationError", "Hyperliquid wallet and signer do not match"));
                                }
                                let nonce = i64::try_from(envelope.nonce_ms).map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid nonce"))?;
                                let _ = envelope.action_wire_json().map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid action"))?;
                                let action = hl_action_value(&envelope.action, envelope.nonce_ms).map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid action"))?;
                                let (signature, include_vault) = match &envelope.action {
                                    HyperliquidAction::UsdSend { destination, amount_usdc, time_ms } => {
                                        if *time_ms != envelope.nonce_ms { return Err(ccxt::ExchangeError::new("ArgumentsRequired", "action nonce mismatch")); }
                                        let payload = hl_object([("hyperliquidChain", hl_str("Mainnet")), ("destination", hl_str(destination.clone())), ("amount", hl_str(amount_usdc.clone())), ("time", ccxt::Value::Int(nonce))]);
                                        (ccxt::runtime::call_typed(hyperliquid.call_dynamic("build_usd_send_sig", vec![payload])).await?, true)
                                    }
                                    HyperliquidAction::Withdraw { destination, amount_usdc, time_ms } => {
                                        if *time_ms != envelope.nonce_ms { return Err(ccxt::ExchangeError::new("ArgumentsRequired", "action nonce mismatch")); }
                                        let payload = hl_object([("hyperliquidChain", hl_str("Mainnet")), ("destination", hl_str(destination.clone())), ("amount", hl_str(amount_usdc.clone())), ("time", ccxt::Value::Int(nonce))]);
                                        (ccxt::runtime::call_typed(hyperliquid.call_dynamic("build_withdraw_sig", vec![payload])).await?, true)
                                    }
                                    HyperliquidAction::UsdClassTransfer { amount_usdc, to_perp, nonce: action_nonce } => {
                                        if *action_nonce != envelope.nonce_ms { return Err(ccxt::ExchangeError::new("ArgumentsRequired", "action nonce mismatch")); }
                                        let payload = hl_object([("hyperliquidChain", hl_str("Mainnet")), ("amount", hl_str(amount_usdc.clone())), ("toPerp", ccxt::Value::Bool(*to_perp)), ("nonce", ccxt::Value::Int(nonce))]);
                                        (ccxt::runtime::call_typed(hyperliquid.call_dynamic("build_usd_class_send_sig", vec![payload])).await?, true)
                                    }
                                    HyperliquidAction::ApproveBuilderFee { builder, max_fee_rate, nonce: action_nonce } => {
                                        if *action_nonce != envelope.nonce_ms { return Err(ccxt::ExchangeError::new("ArgumentsRequired", "action nonce mismatch")); }
                                        let payload = hl_object([("hyperliquidChain", hl_str("Mainnet")), ("maxFeeRate", hl_str(max_fee_rate.clone())), ("builder", hl_str(builder.clone())), ("nonce", ccxt::Value::Int(nonce))]);
                                        (ccxt::runtime::call_typed(hyperliquid.call_dynamic("build_approve_builder_fee_sig", vec![payload])).await?, true)
                                    }
                                    HyperliquidAction::UserAbstraction { user, abstraction, agent: false } => {
                                        let payload = hl_object([("hyperliquidChain", hl_str("Mainnet")), ("user", hl_str(user.clone())), ("abstraction", hl_abstraction_value(*abstraction)), ("nonce", ccxt::Value::Int(nonce))]);
                                        (ccxt::runtime::call_typed(hyperliquid.call_dynamic("build_user_abstraction_sig", vec![payload])).await?, true)
                                    }
                                    HyperliquidAction::UserDexAbstraction { user, enabled, agent: false } => {
                                        let payload = hl_object([("hyperliquidChain", hl_str("Mainnet")), ("user", hl_str(user.clone())), ("enabled", ccxt::Value::Bool(*enabled)), ("nonce", ccxt::Value::Int(nonce))]);
                                        (ccxt::runtime::call_typed(hyperliquid.call_dynamic("build_user_dex_abstraction_sig", vec![payload])).await?, true)
                                    }
                                    HyperliquidAction::PapertradeCoreDeposit { account_mode, destination, amount, nonce: action_nonce }
                                        if *action_nonce == envelope.nonce_ms =>
                                    {
                                        let signature = if account_mode == "default" {
                                            hl_user_typed_signature(
                                                &mut hyperliquid,
                                                signer.private_key.as_str(),
                                                "HyperliquidTransaction:SpotSend",
                                                vec![
                                                    ("hyperliquidChain", "string", hl_str("Mainnet")),
                                                    ("destination", "string", hl_str(destination.to_ascii_lowercase())),
                                                    ("token", "string", hl_str("USDC")),
                                                    ("amount", "string", hl_str(amount.clone())),
                                                    ("time", "uint64", ccxt::Value::Int(nonce)),
                                                ],
                                            )?
                                        } else if account_mode == "unified_account" {
                                            hl_user_typed_signature(
                                                &mut hyperliquid,
                                                signer.private_key.as_str(),
                                                "HyperliquidTransaction:SendAsset",
                                                vec![
                                                    ("hyperliquidChain", "string", hl_str("Mainnet")),
                                                    ("destination", "string", hl_str(destination.to_ascii_lowercase())),
                                                    ("sourceDex", "string", hl_str("spot")),
                                                    ("destinationDex", "string", hl_str("spot")),
                                                    ("token", "string", hl_str("USDC")),
                                                    ("amount", "string", hl_str(amount.clone())),
                                                    ("fromSubAccount", "string", hl_str("")),
                                                    ("nonce", "uint64", ccxt::Value::Int(nonce)),
                                                ],
                                            )?
                                        } else {
                                            return Err(ccxt::ExchangeError::new("ArgumentsRequired", "invalid Papertrade deposit account mode"));
                                        };
                                        (signature, false)
                                    }
                                    HyperliquidAction::SendAsset {
                                        signature_chain_id,
                                        destination,
                                        source_dex,
                                        destination_dex,
                                        token,
                                        amount,
                                        from_sub_account,
                                        nonce: action_nonce,
                                    } if signature_chain_id.eq_ignore_ascii_case("0x3e7")
                                        && *action_nonce == envelope.nonce_ms =>
                                    {
                                        let signature = hl_user_typed_signature(
                                            &mut hyperliquid,
                                            signer.private_key.as_str(),
                                            "HyperliquidTransaction:SendAsset",
                                            vec![
                                                ("hyperliquidChain", "string", hl_str("Mainnet")),
                                                ("destination", "string", hl_str(destination.to_ascii_lowercase())),
                                                ("sourceDex", "string", hl_str(source_dex.clone())),
                                                ("destinationDex", "string", hl_str(destination_dex.clone())),
                                                ("token", "string", hl_str(token.clone())),
                                                ("amount", "string", hl_str(amount.clone())),
                                                ("fromSubAccount", "string", hl_str(from_sub_account.clone())),
                                                ("nonce", "uint64", ccxt::Value::Int(nonce)),
                                            ],
                                        )?;
                                        (signature, false)
                                    }
                                    HyperliquidAction::SendToEvmWithData {
                                        signature_chain_id,
                                        token,
                                        amount,
                                        source_dex,
                                        destination_recipient,
                                        address_encoding,
                                        destination_chain_id,
                                        gas_limit,
                                        data,
                                        nonce: action_nonce,
                                    } if signature_chain_id.eq_ignore_ascii_case("0x3e7")
                                        && *action_nonce == envelope.nonce_ms =>
                                    {
                                        let data_bytes = hex::decode(data.trim_start_matches("0x"))
                                            .map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid data bytes"))?;
                                        let signature = hl_user_typed_signature(
                                            &mut hyperliquid,
                                            signer.private_key.as_str(),
                                            "HyperliquidTransaction:SendToEvmWithData",
                                            vec![
                                                ("hyperliquidChain", "string", hl_str("Mainnet")),
                                                ("token", "string", hl_str(token.clone())),
                                                ("amount", "string", hl_str(amount.clone())),
                                                ("sourceDex", "string", hl_str(source_dex.clone())),
                                                ("destinationRecipient", "string", hl_str(destination_recipient.clone())),
                                                ("addressEncoding", "string", hl_str(address_encoding.clone())),
                                                ("destinationChainId", "uint32", ccxt::Value::Int(i64::from(*destination_chain_id))),
                                                ("gasLimit", "uint64", ccxt::Value::Int(i64::try_from(*gas_limit).map_err(|_| ccxt::ExchangeError::new("ArgumentsRequired", "invalid Hyperliquid gas limit"))?)),
                                                ("data", "bytes", ccxt::Value::Array(data_bytes.into_iter().map(|byte| ccxt::Value::Int(i64::from(byte))).collect())),
                                                ("nonce", "uint64", ccxt::Value::Int(nonce)),
                                            ],
                                        )?;
                                        (signature, false)
                                    }
                                    _ => {
                                        let vault = hl_vault_address(envelope.vault_address.as_deref());
                                        let expiry = match &envelope.action { HyperliquidAction::CreateSubAccount { expires_after_ms, .. } => expires_after_ms.map(|n| ccxt::Value::Int(n as i64)).unwrap_or(ccxt::Value::Null), _ => ccxt::Value::Null };
                                        (ccxt::runtime::call_typed(hyperliquid.call_dynamic("sign_l1_action", vec![action.clone(), ccxt::Value::Int(nonce), vault, expiry])).await?, envelope.vault_address.is_some())
                                    }
                                };
                                let mut fields = vec![("action", action), ("nonce", ccxt::Value::Int(nonce)), ("signature", signature)];
                                if include_vault { fields.push(("vaultAddress", hl_vault_address(envelope.vault_address.as_deref()))); }
                                if let HyperliquidAction::CreateSubAccount { expires_after_ms: Some(time), .. } = &envelope.action { fields.push(("expiresAfter", ccxt::Value::Int(*time as i64))); }
                                let request_value = hl_object(fields);
                                return ccxt::runtime::call_typed(hyperliquid.implicit_api_call("private_post_exchange", request_value)).await;
                            }
                        }
                    }
                    match job.exchange {
                        #[cfg(feature="binance")]
                        ExchangeId::Binance => {
                            // Upstream 4.5.85 lacks this Binance travel-rule API
                            // entry. Keep its one fixed read path inside CCXT's
                            // signed/rate-limited request engine.
                            if job.method == Method::BinanceTravelRuleRequirements {
                                ccxt::runtime::call_typed(binance.request_typed("localentity/questionnaire-requirements", &["sapi".to_owned()], "GET", params, ccxt::Value::Int(1))).await
                            } else {
                                ccxt::runtime::call_typed(binance.implicit_api_call(job.method.name(),params)).await
                            }
                        },
                        #[cfg(feature="kraken")]
                        ExchangeId::Kraken => ccxt::runtime::call_typed(kraken.implicit_api_call(job.method.name(),params)).await,
                        #[cfg(feature="hyperliquid")]
                        ExchangeId::Hyperliquid => ccxt::runtime::call_typed(hyperliquid.implicit_api_call(job.method.name(),params)).await,
                        #[cfg(feature="okx")]
                        ExchangeId::Okx => ccxt::runtime::call_typed(okx.implicit_api_call(job.method.name(),params)).await,
                        #[cfg(feature="coinbase")]
                        ExchangeId::Coinbase => ccxt::runtime::call_typed(coinbase.implicit_api_call(job.method.name(),params)).await,
                        #[cfg(feature="bybit")]
                        ExchangeId::Bybit => ccxt::runtime::call_typed(bybit.implicit_api_call(job.method.name(),params)).await,
                        #[allow(unreachable_patterns)] _ => unreachable!("validated compiled exchange"),
                    }
                };
                let mut result = match tokio::time::timeout(timeout, request).await {
                    Ok(Ok(Ok(_))) => Ok(Value::Null),
                    Ok(Ok(Err(error))) | Ok(Err(error)) => Err(sanitized(error)),
                    Err(_) => Err(CcxtError::new(CcxtErrorKind::Timeout)),
                };
                if let Some((status, text)) = response.lock().unwrap_or_else(|e|e.into_inner()).take() {
                    let raw = serde_json::from_str::<Value>(&text).ok();
                    #[cfg(feature = "hyperliquid")]
                    let native_action_rejection = status == 200
                        && matches!(
                            job.hyperliquid.as_ref(),
                            Some(HyperliquidRequest::Action(_))
                        )
                        && raw
                            .as_ref()
                            .is_some_and(is_native_hyperliquid_action_rejection);
                    #[cfg(not(feature = "hyperliquid"))]
                    let native_action_rejection = false;
                    if native_action_rejection {
                        result = Ok(raw.expect("validated native rejection body"));
                    } else if let Err(error) = &mut result {
                        error.http_status = Some(status);
                        error.exchange_code = raw.as_ref().and_then(|v| v.get("code")).and_then(Value::as_i64);
                        if matches!(status,418|429) { error.kind = CcxtErrorKind::RateLimited; }
                        else if matches!(status,401|403) { error.kind = CcxtErrorKind::PermissionDenied; }
                        else if status >= 400 { error.kind = CcxtErrorKind::Rejected; }
                    } else if (200..300).contains(&status) {
                        result = raw.filter(valid_response_shape).ok_or(CcxtError {kind:CcxtErrorKind::InvalidResponse,http_status:Some(status),exchange_code:None,funding_error_code:None});
                    } else { result = Err(CcxtError {kind:CcxtErrorKind::InvalidResponse,http_status:Some(status),exchange_code:None,funding_error_code:None}); }
                } else if result.is_ok() { result = Err(CcxtError::new(CcxtErrorKind::InvalidResponse)); }
                // CCXT internally copies secrets into dynamic values. Do not retain
                // them or signed request metadata between calls or expose them.
                let exchange: &mut ccxt::Exchange = match job.exchange { #[cfg(feature="binance")] ExchangeId::Binance => &mut binance.core.exchange, #[cfg(feature="kraken")] ExchangeId::Kraken => &mut kraken.core.exchange, #[cfg(feature="hyperliquid")] ExchangeId::Hyperliquid => &mut hyperliquid.core.exchange, #[cfg(feature="okx")] ExchangeId::Okx => &mut okx.core.exchange, #[cfg(feature="coinbase")] ExchangeId::Coinbase => &mut coinbase.core.exchange, #[cfg(feature="bybit")] ExchangeId::Bybit => &mut bybit.core.exchange, #[allow(unreachable_patterns)] _ => { let _ = job.reply.send(Err(CcxtError::new(CcxtErrorKind::Unsupported))); continue; } };
                exchange.apiKey = ccxt::Value::Null;
                exchange.secret = ccxt::Value::Null;
                exchange.last_request_url = ccxt::Value::Null;
                exchange.last_request_headers = ccxt::Value::Null;
                exchange.last_request_body = ccxt::Value::Null;
                #[cfg(feature="hyperliquid")]
                {
                    zeroize_ccxt_string(&mut hyperliquid.core.privateKey);
                    zeroize_ccxt_string(&mut hyperliquid.core.walletAddress);
                    hyperliquid.core.privateKey = ccxt::Value::Null;
                    hyperliquid.core.walletAddress = ccxt::Value::Null;
                }
                *guard.lock().unwrap_or_else(|e|e.into_inner()) = None;
                let _ = job.reply.send(result);
            }
        })).map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?;
        Ok(Self { sender })
    }
    pub async fn request(
        &self,
        exchange: ExchangeId,
        method: Method,
        credentials: Option<Credentials>,
        params: Vec<(String, String)>,
    ) -> Result<Value, CcxtError> {
        self.enqueue(exchange, method, credentials, params, None)
            .await
    }
    /// Re-check a protected approval after CCXT throttling, immediately before
    /// signing. A queued write cannot outlive the approval that authorized it.
    pub async fn request_guarded(
        &self,
        exchange: ExchangeId,
        method: Method,
        credentials: Option<Credentials>,
        params: Vec<(String, String)>,
        guard: RequestGuard,
    ) -> Result<Value, CcxtError> {
        self.enqueue(exchange, method, credentials, params, Some(guard))
            .await
    }

    /// Issue one explicitly allowlisted Hyperliquid `/info` read. The `owner`
    /// for account-scoped variants is part of the request, not signer state.
    #[cfg(feature = "hyperliquid")]
    pub async fn hyperliquid_info(
        &self,
        request: HyperliquidInfoRequest,
    ) -> Result<Value, CcxtError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Job {
                exchange: ExchangeId::Hyperliquid,
                method: Method::HyperliquidTypedInfo,
                credentials: None,
                params: Vec::new(),
                hyperliquid: Some(HyperliquidRequest::Info(request)),
                hyperliquid_signer: None,
                reply,
                guard: None,
            })
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?
    }

    /// Issue one typed read-only Hyperliquid `/explorer` txDetails lookup.
    /// This is reserved for correlation of a persisted native ledger hash.
    #[cfg(feature = "hyperliquid")]
    pub async fn hyperliquid_explorer(
        &self,
        request: HyperliquidExplorerRequest,
    ) -> Result<Value, CcxtError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Job {
                exchange: ExchangeId::Hyperliquid,
                method: Method::HyperliquidTypedExplorer,
                credentials: None,
                params: Vec::new(),
                hyperliquid: Some(HyperliquidRequest::Explorer(request)),
                hyperliquid_signer: None,
                reply,
                guard: None,
            })
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?
    }

    /// Sign and submit one typed Hyperliquid action. Authorization is checked
    /// by the existing CCXT dispatch hook after rate-limit waiting.
    #[cfg(feature = "hyperliquid")]
    pub async fn hyperliquid_action(
        &self,
        action: HyperliquidActionEnvelope,
        signer: HyperliquidSigner,
        guard: RequestGuard,
    ) -> Result<Value, CcxtError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Job {
                exchange: ExchangeId::Hyperliquid,
                method: Method::HyperliquidTypedAction,
                credentials: None,
                params: Vec::new(),
                hyperliquid: Some(HyperliquidRequest::Action(action)),
                hyperliquid_signer: Some(signer),
                reply,
                guard: Some(guard),
            })
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?
    }
    async fn enqueue(
        &self,
        exchange: ExchangeId,
        method: Method,
        credentials: Option<Credentials>,
        params: Vec<(String, String)>,
        guard: Option<RequestGuard>,
    ) -> Result<Value, CcxtError> {
        if exchange != method.exchange()
            || matches!(
                method,
                Method::HyperliquidTypedInfo
                    | Method::HyperliquidTypedExplorer
                    | Method::HyperliquidTypedAction
            )
            || (method.private() && credentials.is_none())
            || (!method.private() && credentials.is_some())
            || (method == Method::HyperliquidMetaAndAssetContexts
                && params != [("type".to_owned(), "metaAndAssetCtxs".to_owned())])
            || (method == Method::HyperliquidOrderBook
                && (params.len() != 2
                    || !params.iter().any(|(k, v)| k == "type" && v == "l2Book")
                    || !params.iter().any(|(k, v)| k == "coin" && !v.is_empty())))
        {
            return Err(CcxtError::new(CcxtErrorKind::InvalidRequest));
        }
        if method.requires_guard() && guard.is_none() {
            return Err(CcxtError::new(CcxtErrorKind::PermissionDenied));
        }
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Job {
                exchange,
                method,
                credentials,
                params,
                hyperliquid: None,
                hyperliquid_signer: None,
                reply,
                guard,
            })
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| CcxtError::new(CcxtErrorKind::Unavailable))?
    }
}

#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
pub(crate) fn sanitized(error: ccxt::ExchangeError) -> CcxtError {
    let kind = if error.is("RateLimitExceeded") || error.is("DDoSProtection") {
        CcxtErrorKind::RateLimited
    } else if error.is("PermissionDenied") {
        CcxtErrorKind::PermissionDenied
    } else if error.is("AuthenticationError") {
        CcxtErrorKind::Authentication
    } else if error.is("RequestTimeout") || error.message.contains("timed out") {
        CcxtErrorKind::Timeout
    } else if error.is("NetworkError") {
        CcxtErrorKind::Unavailable
    } else if error.is("NotSupported") {
        CcxtErrorKind::Unsupported
    } else {
        CcxtErrorKind::Rejected
    };
    let http_status = error
        .message
        .strip_prefix("HTTP ")
        .and_then(|s| s.split(':').next())
        .and_then(|s| s.parse().ok());
    let exchange_code = error
        .message
        .find('{')
        .and_then(|start| serde_json::from_str::<Value>(&error.message[start..]).ok())
        .and_then(|v| v.get("code").and_then(Value::as_i64));
    CcxtError {
        kind,
        http_status,
        exchange_code,
        funding_error_code: None,
    }
}

#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
fn wire_component(value: &str) -> String {
    const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, COMPONENT).to_string()
}

#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
fn normalize_credentials(
    credentials: Option<Credentials>,
) -> Result<Option<Credentials>, CcxtError> {
    use base64::Engine;
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey};
    let Some(mut credentials) = credentials else {
        return Ok(None);
    };
    if !credentials.secret.contains("PRIVATE KEY") {
        return Ok(Some(credentials));
    }
    if let Ok(key) = ed25519_dalek::SigningKey::from_pkcs8_pem(&credentials.secret) {
        // CCXT Binance detects Ed25519 by PEM length. Remove optional public-key
        // attributes with a minimal RFC8410 encoding (not a custom signer).
        let mut der = Zeroizing::new(vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ]);
        der.extend_from_slice(&key.to_bytes());
        let encoded = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(&*der));
        credentials.secret = Zeroizing::new(format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            *encoded
        ));
    } else if let Ok(key) = rsa::RsaPrivateKey::from_pkcs8_pem(&credentials.secret)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(&credentials.secret))
    {
        credentials.secret = key
            .to_pkcs1_pem(Default::default())
            .map_err(|_| CcxtError::new(CcxtErrorKind::Authentication))?;
    } else {
        return Err(CcxtError::new(CcxtErrorKind::Authentication));
    }
    Ok(Some(credentials))
}

#[cfg(all(test, feature = "binance", feature = "kraken"))]
mod tests;

#[cfg(all(test, feature = "hyperliquid"))]
#[path = "ccxt/hyperliquid_tests.rs"]
mod hyperliquid_tests;
