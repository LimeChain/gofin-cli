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
    BinanceWithdrawAddressList,
    BinanceDepositHistory,
    BinanceWithdrawalHistory,
    BinanceUniversalTransferHistory,
    BinanceSubAccountTransferHistory,
    BinanceUniversalTransfer,
    BinanceStandardWithdrawal,
    BinanceTravelRuleRequirements,
    BinanceTickerPrice,
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
    HyperliquidMetaAndAssetContexts,
    OkxInstruments,
    OkxTicker,
    CoinbaseProducts,
    CoinbaseTicker,
    BybitInstruments,
    BybitTicker,
}
impl Method {
    fn exchange(self) -> ExchangeId {
        match self {
            Self::KrakenTicker | Self::KrakenAssetPairs => ExchangeId::Kraken,
            Self::HyperliquidMetaAndAssetContexts => ExchangeId::Hyperliquid,
            Self::OkxInstruments | Self::OkxTicker => ExchangeId::Okx,
            Self::CoinbaseProducts | Self::CoinbaseTicker => ExchangeId::Coinbase,
            Self::BybitInstruments | Self::BybitTicker => ExchangeId::Bybit,
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
            Self::BinanceWithdrawAddressList => "sapi_get_capital_withdraw_address_list",
            Self::BinanceDepositHistory => "sapi_get_capital_deposit_hisrec",
            Self::BinanceWithdrawalHistory => "sapi_get_capital_withdraw_history",
            Self::BinanceUniversalTransferHistory => "sapi_get_sub_account_universal_transfer",
            Self::BinanceSubAccountTransferHistory => {
                "sapi_get_sub_account_transfer_sub_user_history"
            }
            Self::BinanceUniversalTransfer => "sapi_post_sub_account_universal_transfer",
            Self::BinanceStandardWithdrawal => "sapi_post_capital_withdraw_apply",
            Self::BinanceTravelRuleRequirements => {
                "sapi_get_localentity_questionnaire_requirements"
            }
            Self::BinanceTickerPrice => "public_get_ticker_price",
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
            Self::HyperliquidMetaAndAssetContexts => "public_post_info",
            Self::OkxInstruments => "public_get_public_instruments",
            Self::OkxTicker => "public_get_market_ticker",
            Self::CoinbaseProducts => "v3_public_get_brokerage_market_products",
            Self::CoinbaseTicker => "v3_public_get_brokerage_market_products_product_id_ticker",
            Self::BybitInstruments => "public_get_v5_market_instruments_info",
            Self::BybitTicker => "public_get_v5_market_tickers",
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
                | Self::BinanceStandardWithdrawal
                | Self::BybitInstruments
                | Self::BybitTicker
        )
    }
    fn requires_guard(self) -> bool {
        matches!(
            self,
            Self::BinanceSpotPlaceOrder
                | Self::BinanceSpotCancelOrder
                | Self::BinanceFuturesPlaceOrder
                | Self::BinanceFuturesCancelOrder
                | Self::BinanceWalletTransfer
        )
    }
    fn private(self) -> bool {
        !matches!(
            self,
            Self::BinanceTickerPrice
                | Self::BinanceSpotExchangeInfo
                | Self::BinanceFuturesExchangeInfo
                | Self::BinanceFuturesMarkPrice
                | Self::KrakenTicker
                | Self::KrakenAssetPairs
                | Self::HyperliquidMetaAndAssetContexts
                | Self::OkxInstruments
                | Self::OkxTicker
                | Self::CoinbaseProducts
                | Self::CoinbaseTicker
                | Self::BybitInstruments
                | Self::BybitTicker
        )
    }
}

/// Secrets are supplied only by the protected credential provider, never argv.
/// Deliberately neither Debug nor Serialize.
pub struct Credentials {
    pub api_key: Zeroizing<String>,
    pub secret: Zeroizing<String>,
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
/// Only safe categories and numeric exchange evidence cross the CCXT boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CcxtError {
    pub kind: CcxtErrorKind,
    pub http_status: Option<u16>,
    pub exchange_code: Option<i64>,
}
impl CcxtError {
    pub fn new(kind: CcxtErrorKind) -> Self {
        Self {
            kind,
            http_status: None,
            exchange_code: None,
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
type RequestGuard = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

struct Job {
    exchange: ExchangeId,
    method: Method,
    credentials: Option<Credentials>,
    params: Vec<(String, String)>,
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
            panic!("[PermissionDenied] Fin write approval expired before signing");
        }
        DerivedExchange::sign(&self.core, path, api, method, params, headers, body)
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
fn install_panic_redaction() {
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
struct PanicRedaction;
#[cfg(any(
    feature = "binance",
    feature = "kraken",
    feature = "hyperliquid",
    feature = "okx",
    feature = "coinbase",
    feature = "bybit"
))]
impl PanicRedaction {
    fn enter() -> Self {
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
            .user_agent(concat!("fin-cli/", env!("CARGO_PKG_VERSION")))
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
        std::thread::Builder::new().name("fin-ccxt".into()).spawn(move || runtime.block_on(async move {
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
                { hyperliquid.urls = ccxt::Value::from(serde_json::json!({"api":{"public":base}})); }
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
                *response.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *guard.lock().unwrap_or_else(|e|e.into_inner()) = job.guard;
                let request = async {
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
                    if let Err(error) = &mut result {
                        error.http_status = Some(status);
                        error.exchange_code = raw.as_ref().and_then(|v| v.get("code")).and_then(Value::as_i64);
                        if matches!(status,418|429) { error.kind = CcxtErrorKind::RateLimited; }
                        else if matches!(status,401|403) { error.kind = CcxtErrorKind::PermissionDenied; }
                        else if status >= 400 { error.kind = CcxtErrorKind::Rejected; }
                    } else if (200..300).contains(&status) {
                        result = raw.filter(|v| v.is_object() || v.is_array()).ok_or(CcxtError {kind:CcxtErrorKind::InvalidResponse,http_status:Some(status),exchange_code:None});
                    } else { result = Err(CcxtError {kind:CcxtErrorKind::InvalidResponse,http_status:Some(status),exchange_code:None}); }
                } else if result.is_ok() { result = Err(CcxtError::new(CcxtErrorKind::InvalidResponse)); }
                // CCXT internally copies secrets into dynamic values. Do not retain
                // them or signed request metadata between calls or expose them.
                let exchange: &mut ccxt::Exchange = match job.exchange { #[cfg(feature="binance")] ExchangeId::Binance => &mut binance.core.exchange, #[cfg(feature="kraken")] ExchangeId::Kraken => &mut kraken.core.exchange, #[cfg(feature="hyperliquid")] ExchangeId::Hyperliquid => &mut hyperliquid.core.exchange, #[cfg(feature="okx")] ExchangeId::Okx => &mut okx.core.exchange, #[cfg(feature="coinbase")] ExchangeId::Coinbase => &mut coinbase.core.exchange, #[cfg(feature="bybit")] ExchangeId::Bybit => &mut bybit.core.exchange, #[allow(unreachable_patterns)] _ => { let _ = job.reply.send(Err(CcxtError::new(CcxtErrorKind::Unsupported))); continue; } };
                exchange.apiKey = ccxt::Value::Null;
                exchange.secret = ccxt::Value::Null;
                exchange.last_request_url = ccxt::Value::Null;
                exchange.last_request_headers = ccxt::Value::Null;
                exchange.last_request_body = ccxt::Value::Null;
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
    async fn enqueue(
        &self,
        exchange: ExchangeId,
        method: Method,
        credentials: Option<Credentials>,
        params: Vec<(String, String)>,
        guard: Option<RequestGuard>,
    ) -> Result<Value, CcxtError> {
        if exchange != method.exchange()
            || (method.private() && credentials.is_none())
            || (!method.private() && credentials.is_some())
            || (method == Method::HyperliquidMetaAndAssetContexts
                && params != [("type".to_owned(), "metaAndAssetCtxs".to_owned())])
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
fn sanitized(error: ccxt::ExchangeError) -> CcxtError {
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
