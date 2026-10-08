//! Binance SPOT funding profiles and exact evidence.
//!
//! The adapter exposes a closed endpoint enum instead of a caller-selected
//! method/path pair. The shared CCXT runtime owns connectivity and signing;
//! protected broker credentials are supplied through [`BinanceCcxtTransport`].

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::Zero;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub mod adapter;
pub use adapter::*;
pub mod trading_reads;

pub const MAX_DECIMAL_SCALE: u32 = 36;
const TRANSFER_ID_PREFIX: &str = "fin";
const WITHDRAW_ID_PREFIX: &str = "fin";

/// An exact, unsigned base-10 quantity. The original scale is kept because
/// exchange observations may be more precise than the eventual chain unit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExactDecimal {
    coefficient: BigUint,
    scale: u32,
}

impl ExactDecimal {
    pub fn parse(value: &str) -> Result<Self, BinanceError> {
        if value.is_empty() || value.starts_with('-') || value.starts_with('+') {
            return Err(BinanceError::InvalidDecimal);
        }
        let mut parts = value.split('.');
        let whole = parts.next().ok_or(BinanceError::InvalidDecimal)?;
        let fraction = parts.next().unwrap_or("");
        if parts.next().is_some()
            || whole.is_empty()
            || (whole.len() > 1 && whole.starts_with('0'))
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
            || value.ends_with('.')
            || fraction.len() > MAX_DECIMAL_SCALE as usize
        {
            return Err(BinanceError::InvalidDecimal);
        }
        let digits = format!("{whole}{fraction}");
        let coefficient =
            BigUint::parse_bytes(digits.as_bytes(), 10).ok_or(BinanceError::InvalidDecimal)?;
        Ok(Self {
            coefficient,
            scale: fraction.len() as u32,
        })
    }

    pub fn zero() -> Self {
        Self {
            coefficient: BigUint::zero(),
            scale: 0,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.coefficient.is_zero()
    }

    pub fn scale(&self) -> u32 {
        self.scale
    }

    pub(crate) fn coefficient_for_scale(&self, scale: u32) -> BigUint {
        debug_assert!(scale >= self.scale);
        &self.coefficient * ten_pow(scale - self.scale)
    }

    pub(crate) fn from_coefficient(coefficient: BigUint, scale: u32) -> Result<Self, BinanceError> {
        if scale > MAX_DECIMAL_SCALE {
            return Err(BinanceError::InvalidDecimal);
        }
        Ok(Self { coefficient, scale })
    }

    pub fn checked_cmp(&self, other: &Self) -> std::cmp::Ordering {
        let scale = self.scale.max(other.scale);
        let lhs = &self.coefficient * ten_pow(scale - self.scale);
        let rhs = &other.coefficient * ten_pow(scale - other.scale);
        lhs.cmp(&rhs)
    }

    pub fn is_multiple_of(&self, increment: &Self) -> bool {
        if increment.is_zero() {
            return false;
        }
        let scale = self.scale.max(increment.scale);
        let lhs = &self.coefficient * ten_pow(scale - self.scale);
        let rhs = &increment.coefficient * ten_pow(scale - increment.scale);
        (&lhs % &rhs).is_zero()
    }

    pub fn canonical(&self) -> String {
        if self.scale == 0 {
            return self.coefficient.to_string();
        }
        let raw = self.coefficient.to_string();
        let scale = self.scale as usize;
        let padded = if raw.len() <= scale {
            format!("{}{}", "0".repeat(scale + 1 - raw.len()), raw)
        } else {
            raw
        };
        let split = padded.len() - scale;
        let whole = &padded[..split];
        let frac = padded[split..].trim_end_matches('0');
        if frac.is_empty() {
            whole.to_owned()
        } else {
            format!("{whole}.{frac}")
        }
    }
}

impl TryFrom<String> for ExactDecimal {
    type Error = BinanceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ExactDecimal> for String {
    fn from(value: ExactDecimal) -> Self {
        value.canonical()
    }
}

fn ten_pow(power: u32) -> BigUint {
    BigUint::from(10u8).pow(power)
}

/// Opaque, stable id sent to Binance and stored in Fin's pre-dispatch journal.
/// It is deterministically derived from the existing logical request identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientIdentity(String);

impl ClientIdentity {
    pub fn transfer(request_id: &str) -> Result<Self, BinanceError> {
        Self::derive("binance.transfer", request_id, TRANSFER_ID_PREFIX)
    }

    pub fn withdrawal(request_id: &str) -> Result<Self, BinanceError> {
        Self::derive("binance.withdraw", request_id, WITHDRAW_ID_PREFIX)
    }

    fn derive(operation: &str, request_id: &str, prefix: &str) -> Result<Self, BinanceError> {
        if request_id.is_empty() || request_id.len() > 128 {
            return Err(BinanceError::InvalidIdentity);
        }
        let mut hash = Sha256::new();
        hash.update(operation.as_bytes());
        hash.update(b":");
        hash.update(request_id.as_bytes());
        let digest = hex::encode(hash.finalize());
        Ok(Self(format!("{prefix}{}", &digest[..29])))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The exact method/path allowlist Fin supports. There is no arbitrary signed
/// method or path surface here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinanceEndpoint {
    ExchangeInfo,
    SpotOrderStatus,
    ApiRestrictions,
    Account,
    SubAccountList,
    SubAccountSpotAssets,
    CapitalConfig,
    DepositAddress,
    DepositAddressList,
    WithdrawAddressList,
    DepositHistory,
    WithdrawalHistory,
    UniversalTransferHistory,
    SubAccountTransferHistory,
    UniversalTransfer,
    SubToMasterTransfer,
    StandardWithdrawal,
    TravelRuleRequirements,
    SpotExchangeInfo,
    SpotCommission,
    SpotOpenOrders,
    SpotOrder,
    SpotTrades,
    SpotPlaceOrder,
    SpotCancelOrder,
    FuturesExchangeInfo,
    FuturesAccount,
    FuturesAccountConfig,
    FuturesSymbolConfig,
    FuturesPositions,
    FuturesPositionMode,
    FuturesCommission,
    FuturesFeeBurn,
    FuturesOrder,
    FuturesOpenOrders,
    FuturesTrades,
    FuturesIncome,
    FuturesMarkPrice,
    FuturesLeverageBrackets,
    FuturesPlaceOrder,
    FuturesCancelOrder,
    WalletTransfer,
    WalletTransferHistory,
}

impl BinanceEndpoint {
    pub fn method_path(self) -> (&'static str, &'static str) {
        match self {
            Self::ExchangeInfo => ("GET", "/api/v3/exchangeInfo"),
            Self::SpotOrderStatus => ("GET", "/api/v3/order"),
            Self::ApiRestrictions => ("GET", "/sapi/v1/account/apiRestrictions"),
            Self::Account => ("GET", "/api/v3/account"),
            Self::SpotExchangeInfo => ("GET", "/api/v3/exchangeInfo"),
            Self::SpotCommission => ("GET", "/api/v3/account/commission"),
            Self::SpotOpenOrders => ("GET", "/api/v3/openOrders"),
            Self::SpotOrder => ("GET", "/api/v3/order"),
            Self::SpotTrades => ("GET", "/api/v3/myTrades"),
            Self::SpotPlaceOrder => ("POST", "/api/v3/order"),
            Self::SpotCancelOrder => ("DELETE", "/api/v3/order"),
            Self::FuturesExchangeInfo => ("GET", "/fapi/v1/exchangeInfo"),
            Self::FuturesAccount => ("GET", "/fapi/v3/account"),
            Self::FuturesAccountConfig => ("GET", "/fapi/v1/accountConfig"),
            Self::FuturesSymbolConfig => ("GET", "/fapi/v1/symbolConfig"),
            Self::FuturesPositions => ("GET", "/fapi/v3/positionRisk"),
            Self::FuturesPositionMode => ("GET", "/fapi/v1/positionSide/dual"),
            Self::FuturesCommission => ("GET", "/fapi/v1/commissionRate"),
            Self::FuturesFeeBurn => ("GET", "/fapi/v1/feeBurn"),
            Self::FuturesOrder => ("GET", "/fapi/v1/order"),
            Self::FuturesOpenOrders => ("GET", "/fapi/v1/openOrders"),
            Self::FuturesTrades => ("GET", "/fapi/v1/userTrades"),
            Self::FuturesIncome => ("GET", "/fapi/v1/income"),
            Self::FuturesMarkPrice => ("GET", "/fapi/v1/premiumIndex"),
            Self::FuturesLeverageBrackets => ("GET", "/fapi/v1/leverageBracket"),
            Self::FuturesPlaceOrder => ("POST", "/fapi/v1/order"),
            Self::FuturesCancelOrder => ("DELETE", "/fapi/v1/order"),
            Self::WalletTransfer => ("POST", "/sapi/v1/asset/transfer"),
            Self::WalletTransferHistory => ("GET", "/sapi/v1/asset/transfer"),

            Self::SubAccountList => ("GET", "/sapi/v1/sub-account/list"),
            Self::SubAccountSpotAssets => ("GET", "/sapi/v4/sub-account/assets"),
            Self::CapitalConfig => ("GET", "/sapi/v1/capital/config/getall"),
            Self::DepositAddress => ("GET", "/sapi/v1/capital/deposit/address"),
            Self::DepositAddressList => ("GET", "/sapi/v1/capital/deposit/address/list"),
            Self::WithdrawAddressList => ("GET", "/sapi/v1/capital/withdraw/address/list"),
            Self::DepositHistory => ("GET", "/sapi/v1/capital/deposit/hisrec"),
            Self::WithdrawalHistory => ("GET", "/sapi/v1/capital/withdraw/history"),
            Self::UniversalTransferHistory => ("GET", "/sapi/v1/sub-account/universalTransfer"),
            Self::SubAccountTransferHistory => {
                ("GET", "/sapi/v1/sub-account/transfer/subUserHistory")
            }
            Self::UniversalTransfer => ("POST", "/sapi/v1/sub-account/universalTransfer"),
            Self::SubToMasterTransfer => ("POST", "/sapi/v1/sub-account/transfer/subToMaster"),
            Self::StandardWithdrawal => ("POST", "/sapi/v1/capital/withdraw/apply"),
            Self::TravelRuleRequirements => {
                ("GET", "/sapi/v1/localentity/questionnaire-requirements")
            }
        }
    }
}

/// Finite parameters are assembled from endpoint-specific builders. Values
/// remain decimal or identifiers strings at the signing boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BinanceRequest {
    pub endpoint: BinanceEndpoint,
    pub params: Vec<Param>,
}

impl BinanceRequest {
    pub fn new(endpoint: BinanceEndpoint) -> Self {
        Self {
            endpoint,
            params: Vec::new(),
        }
    }

    pub fn push(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.params.push(Param {
            key: key.to_owned(),
            value: value.into(),
        });
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|param| param.key == key)
            .map(|param| param.value.as_str())
    }

    pub fn validate(&self) -> Result<(), BinanceError> {
        let keys: Vec<&str> = self.params.iter().map(|p| p.key.as_str()).collect();
        let unique: std::collections::BTreeSet<&str> = keys.iter().copied().collect();
        if unique.len() != keys.len() {
            return Err(BinanceError::DuplicateParameter);
        }
        let (required, allowed): (&[&str], &[&str]) = match self.endpoint {
            BinanceEndpoint::ExchangeInfo => (&["symbol"], &["symbol"]),
            BinanceEndpoint::SpotOrderStatus => (
                &["symbol", "origClientOrderId"],
                &["symbol", "origClientOrderId", "recvWindow"],
            ),
            BinanceEndpoint::ApiRestrictions
            | BinanceEndpoint::Account
            | BinanceEndpoint::CapitalConfig
            | BinanceEndpoint::WithdrawAddressList
            | BinanceEndpoint::TravelRuleRequirements => (&[], &["recvWindow"]),
            BinanceEndpoint::SpotExchangeInfo => (&[], &["symbol"]),
            BinanceEndpoint::FuturesExchangeInfo => (&[], &[]),
            BinanceEndpoint::FuturesAccount
            | BinanceEndpoint::FuturesPositionMode
            | BinanceEndpoint::FuturesAccountConfig
            | BinanceEndpoint::FuturesFeeBurn => (&[], &["recvWindow"]),
            BinanceEndpoint::SpotCommission | BinanceEndpoint::FuturesCommission => {
                (&["symbol"], &["symbol", "recvWindow"])
            }
            BinanceEndpoint::SpotOpenOrders
            | BinanceEndpoint::FuturesOpenOrders
            | BinanceEndpoint::FuturesPositions
            | BinanceEndpoint::FuturesLeverageBrackets
            | BinanceEndpoint::FuturesSymbolConfig => (&[], &["symbol", "recvWindow"]),
            BinanceEndpoint::FuturesMarkPrice => (&[], &["symbol"]),
            BinanceEndpoint::SpotOrder
            | BinanceEndpoint::FuturesOrder
            | BinanceEndpoint::FuturesCancelOrder => (
                &["symbol"],
                &["symbol", "orderId", "origClientOrderId", "recvWindow"],
            ),
            BinanceEndpoint::SpotCancelOrder => (
                &["symbol"],
                &[
                    "symbol",
                    "orderId",
                    "origClientOrderId",
                    "newClientOrderId",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::SpotTrades | BinanceEndpoint::FuturesTrades => (
                &["symbol"],
                &[
                    "symbol",
                    "orderId",
                    "fromId",
                    "startTime",
                    "endTime",
                    "limit",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::FuturesIncome => (
                &[],
                &[
                    "symbol",
                    "incomeType",
                    "startTime",
                    "endTime",
                    "page",
                    "limit",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::SpotPlaceOrder => (
                &["symbol", "side", "type", "quantity", "newClientOrderId"],
                &[
                    "symbol",
                    "side",
                    "type",
                    "quantity",
                    "price",
                    "timeInForce",
                    "newClientOrderId",
                    "newOrderRespType",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::FuturesPlaceOrder => (
                &[
                    "symbol",
                    "side",
                    "type",
                    "quantity",
                    "newClientOrderId",
                    "positionSide",
                ],
                &[
                    "symbol",
                    "side",
                    "type",
                    "quantity",
                    "price",
                    "timeInForce",
                    "newClientOrderId",
                    "newOrderRespType",
                    "positionSide",
                    "reduceOnly",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::WalletTransfer => (
                &["type", "asset", "amount"],
                &["type", "asset", "amount", "recvWindow"],
            ),
            BinanceEndpoint::WalletTransferHistory => (
                &["type"],
                &[
                    "type",
                    "startTime",
                    "endTime",
                    "current",
                    "size",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::SubAccountList => (&[], &["email", "page", "limit", "recvWindow"]),
            BinanceEndpoint::SubAccountSpotAssets => (&["email"], &["email", "recvWindow"]),
            BinanceEndpoint::DepositAddress | BinanceEndpoint::DepositAddressList => {
                (&["coin", "network"], &["coin", "network", "recvWindow"])
            }
            BinanceEndpoint::DepositHistory | BinanceEndpoint::WithdrawalHistory => (
                &["startTime", "endTime"],
                &[
                    "coin",
                    "startTime",
                    "endTime",
                    "offset",
                    "limit",
                    "txId",
                    "withdrawOrderId",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::UniversalTransferHistory => (
                &["startTime", "endTime"],
                &[
                    "startTime",
                    "endTime",
                    "page",
                    "limit",
                    "clientTranId",
                    "fromEmail",
                    "toEmail",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::SubAccountTransferHistory => (
                &[],
                &[
                    "asset",
                    "type",
                    "startTime",
                    "endTime",
                    "limit",
                    "returnFailHistory",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::UniversalTransfer => (
                &[
                    "asset",
                    "amount",
                    "fromAccountType",
                    "toAccountType",
                    "clientTranId",
                ],
                &[
                    "asset",
                    "amount",
                    "fromAccountType",
                    "toAccountType",
                    "clientTranId",
                    "fromEmail",
                    "toEmail",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::StandardWithdrawal => (
                &[
                    "coin",
                    "withdrawOrderId",
                    "network",
                    "address",
                    "amount",
                    "walletType",
                ],
                &[
                    "coin",
                    "withdrawOrderId",
                    "network",
                    "address",
                    "addressTag",
                    "amount",
                    "walletType",
                    "recvWindow",
                ],
            ),
            BinanceEndpoint::SubToMasterTransfer => {
                (&["asset", "amount"], &["asset", "amount", "recvWindow"])
            }
        };
        if required.iter().any(|key| self.get(key).is_none()) {
            return Err(BinanceError::MissingParameter);
        }
        if self
            .params
            .iter()
            .any(|param| !allowed.contains(&param.key.as_str()))
        {
            return Err(BinanceError::UnsupportedParameter);
        }
        if matches!(
            self.endpoint,
            BinanceEndpoint::SpotExchangeInfo
                | BinanceEndpoint::SpotCommission
                | BinanceEndpoint::SpotOpenOrders
                | BinanceEndpoint::SpotOrder
                | BinanceEndpoint::SpotTrades
                | BinanceEndpoint::SpotPlaceOrder
                | BinanceEndpoint::SpotCancelOrder
                | BinanceEndpoint::FuturesExchangeInfo
                | BinanceEndpoint::FuturesAccount
                | BinanceEndpoint::FuturesAccountConfig
                | BinanceEndpoint::FuturesSymbolConfig
                | BinanceEndpoint::FuturesPositions
                | BinanceEndpoint::FuturesPositionMode
                | BinanceEndpoint::FuturesCommission
                | BinanceEndpoint::FuturesOrder
                | BinanceEndpoint::FuturesOpenOrders
                | BinanceEndpoint::FuturesTrades
                | BinanceEndpoint::FuturesIncome
                | BinanceEndpoint::FuturesMarkPrice
                | BinanceEndpoint::FuturesLeverageBrackets
                | BinanceEndpoint::FuturesPlaceOrder
                | BinanceEndpoint::FuturesCancelOrder
                | BinanceEndpoint::WalletTransfer
                | BinanceEndpoint::WalletTransferHistory
        ) && (self.params.iter().any(|p| p.value.is_empty())
            || self
                .get("orderId")
                .is_some_and(|v| v.parse::<u64>().map_or(true, |id| id == 0))
            || self.get("origClientOrderId").is_some_and(|v| {
                v.len() > 36
                    || !v
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
            }))
        {
            return Err(BinanceError::InvalidRequest);
        }
        if self.endpoint == BinanceEndpoint::SpotCancelOrder
            && self.get("newClientOrderId").is_some_and(|id| {
                id.is_empty()
                    || id.len() > 36
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
            })
        {
            return Err(BinanceError::InvalidRequest);
        }
        if matches!(
            self.endpoint,
            BinanceEndpoint::SpotOrder
                | BinanceEndpoint::FuturesOrder
                | BinanceEndpoint::SpotCancelOrder
                | BinanceEndpoint::FuturesCancelOrder
        ) && self.get("orderId").is_none() == self.get("origClientOrderId").is_none()
        {
            return Err(BinanceError::InvalidRequest);
        }
        if matches!(
            self.endpoint,
            BinanceEndpoint::SpotPlaceOrder | BinanceEndpoint::FuturesPlaceOrder
        ) {
            if !matches!(self.get("side"), Some("BUY" | "SELL"))
                || !matches!(self.get("type"), Some("MARKET" | "LIMIT"))
                || ExactDecimal::parse(self.get("quantity").unwrap())?.is_zero()
                || self.get("newClientOrderId").is_none_or(|id| {
                    id.is_empty()
                        || id.len() > 36
                        || !id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
                })
                || self
                    .get("newOrderRespType")
                    .is_some_and(|v| !matches!(v, "ACK" | "RESULT" | "FULL"))
            {
                return Err(BinanceError::InvalidRequest);
            }
            match self.get("type") {
                Some("LIMIT") => {
                    if self.get("price").is_none_or(|p| {
                        ExactDecimal::parse(p).is_err()
                            || ExactDecimal::parse(p).is_ok_and(|n| n.is_zero())
                    }) || !matches!(self.get("timeInForce"), Some("GTC" | "IOC" | "FOK"))
                    {
                        return Err(BinanceError::InvalidRequest);
                    }
                }
                Some("MARKET")
                    if self.get("price").is_some() || self.get("timeInForce").is_some() =>
                {
                    return Err(BinanceError::InvalidRequest);
                }
                _ => {}
            }
            if self.endpoint == BinanceEndpoint::FuturesPlaceOrder
                && (!matches!(self.get("positionSide"), Some("BOTH" | "LONG" | "SHORT"))
                    || self
                        .get("reduceOnly")
                        .is_some_and(|v| !matches!(v, "true" | "false"))
                    || (self.get("positionSide") != Some("BOTH")
                        && self.get("reduceOnly").is_some())
                    || self.get("newOrderRespType") == Some("FULL"))
            {
                return Err(BinanceError::InvalidRequest);
            }
        }
        if matches!(
            self.endpoint,
            BinanceEndpoint::WalletTransfer | BinanceEndpoint::WalletTransferHistory
        ) && (!matches!(self.get("type"), Some("MAIN_UMFUTURE" | "UMFUTURE_MAIN"))
            || (self.endpoint == BinanceEndpoint::WalletTransfer
                && (ExactDecimal::parse(self.get("amount").unwrap())?.is_zero()
                    || self.get("asset").is_none_or(str::is_empty))))
        {
            return Err(BinanceError::InvalidRequest);
        }
        if matches!(self.endpoint, BinanceEndpoint::UniversalTransfer) {
            let amount = ExactDecimal::parse(self.get("amount").unwrap())?;
            if amount.is_zero()
                || self.get("fromAccountType") != Some("SPOT")
                || self.get("toAccountType") != Some("SPOT")
            {
                return Err(BinanceError::InvalidRequest);
            }
            if self.get("fromEmail").is_none() == self.get("toEmail").is_none() {
                return Err(BinanceError::InvalidRequest);
            }
        }
        if self.endpoint == BinanceEndpoint::SubToMasterTransfer
            && (ExactDecimal::parse(self.get("amount").unwrap())?.is_zero()
                || self.get("asset").is_none_or(|asset| {
                    asset.is_empty()
                        || asset.len() > 40
                        || asset.trim() != asset
                        || asset.chars().any(char::is_control)
                }))
        {
            return Err(BinanceError::InvalidRequest);
        }
        if matches!(self.endpoint, BinanceEndpoint::StandardWithdrawal) {
            let safe_text = |value: &str, max_bytes: usize| {
                !value.is_empty()
                    && value.len() <= max_bytes
                    && value.trim() == value
                    && !value.chars().any(char::is_control)
            };
            if ExactDecimal::parse(self.get("amount").unwrap())?.is_zero()
                || !safe_text(self.get("coin").unwrap(), 40)
                || !safe_text(self.get("network").unwrap(), 40)
                || !safe_text(self.get("address").unwrap(), 256)
                || self
                    .get("addressTag")
                    .is_some_and(|memo| !safe_text(memo, 256))
                || self.get("walletType") != Some("0")
            {
                return Err(BinanceError::InvalidRequest);
            }
        }
        if matches!(self.endpoint, BinanceEndpoint::SubAccountTransferHistory) {
            let valid_u64 = |key: &str| {
                self.get(key)
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some()
            };
            if self.get("asset").is_none_or(str::is_empty)
                || !matches!(self.get("type"), Some("1" | "2"))
                || !valid_u64("startTime")
                || !valid_u64("endTime")
                || !valid_u64("limit")
                || self
                    .get("startTime")
                    .and_then(|value| value.parse::<u64>().ok())
                    > self
                        .get("endTime")
                        .and_then(|value| value.parse::<u64>().ok())
                || !matches!(
                    self.get("limit")
                        .and_then(|value| value.parse::<u64>().ok()),
                    Some(1..=200)
                )
                || self.get("returnFailHistory") != Some("true")
            {
                return Err(BinanceError::InvalidRequest);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub key: String,
    pub value: String,
}

/// The broker-owned implementation receives the endpoint as a closed enum,
/// resolves a credential handle and signer internally, signs the exact encoded
/// parameter string, and returns unnormalized JSON. Adapters never hold keys.
#[async_trait]
pub trait BinanceTransport: Send + Sync {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError>;

    async fn request_authorized(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
        journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<Value, TransportError> {
        journal
            .assert_authorized()
            .map_err(|_| TransportError::PermissionDenied)?;
        self.request(credential, request).await
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportError {
    Unavailable,
    RateLimited,
    PermissionDenied,
    InvalidResponse,
    Timeout,
    Rejected,
    HttpFailure {
        status: u16,
        binance_code: Option<i64>,
        kind: HttpFailureKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpFailureKind {
    RateLimited,
    PermissionDenied,
    Rejected,
    InvalidResponse,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "Binance request unavailable",
            Self::RateLimited => "Binance rate limited the request",
            Self::PermissionDenied => "Binance request permission denied",
            Self::InvalidResponse => "Binance returned an invalid response",
            Self::Timeout => "Binance request timed out",
            Self::Rejected => "Binance rejected the request",
            Self::HttpFailure { kind, .. } => match kind {
                HttpFailureKind::RateLimited => "Binance rate limited the request",
                HttpFailureKind::PermissionDenied => "Binance request permission denied",
                HttpFailureKind::Rejected => "Binance rejected the request",
                HttpFailureKind::InvalidResponse => "Binance returned an invalid response",
            },
        })
    }
}

impl std::error::Error for TransportError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BinanceError {
    InvalidDecimal,
    InvalidIdentity,
    MissingParameter,
    DuplicateParameter,
    InvalidRequest,
    UnsupportedParameter,
    InvalidResponse,
}

impl fmt::Display for BinanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidDecimal => "invalid exact decimal",
            Self::InvalidIdentity => "invalid logical request identity",
            Self::MissingParameter => "required Binance parameter is missing",
            Self::DuplicateParameter => "duplicate Binance parameter",
            Self::InvalidRequest => "invalid Binance request",
            Self::UnsupportedParameter => "unsupported Binance endpoint parameter",
            Self::InvalidResponse => "invalid Binance response",
        })
    }
}

impl std::error::Error for BinanceError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct CredentialHandle(String);

impl CredentialHandle {
    pub fn new(handle: impl Into<String>) -> Result<Self, BinanceError> {
        let handle = handle.into();
        if handle.is_empty() || handle.len() > 128 {
            return Err(BinanceError::InvalidIdentity);
        }
        Ok(Self(handle))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubToMasterTransferIntent {
    pub asset: String,
    pub amount: ExactDecimal,
}

impl SubToMasterTransferIntent {
    /// Binance derives the recipient from the signing child's parent. This
    /// endpoint accepts no client transaction identifier or arbitrary recipient.
    pub fn request(&self) -> Result<BinanceRequest, BinanceError> {
        let request = BinanceRequest::new(BinanceEndpoint::SubToMasterTransfer)
            .push("asset", self.asset.clone())
            .push("amount", self.amount.canonical());
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UniversalTransferIntent {
    pub asset: String,
    pub amount: ExactDecimal,
    pub from_email: Option<String>,
    pub to_email: Option<String>,
    pub client_id: ClientIdentity,
}

impl UniversalTransferIntent {
    pub fn request(&self) -> Result<BinanceRequest, BinanceError> {
        let mut request = BinanceRequest::new(BinanceEndpoint::UniversalTransfer)
            .push("asset", self.asset.clone())
            .push("amount", self.amount.canonical())
            .push("fromAccountType", "SPOT")
            .push("toAccountType", "SPOT")
            .push("clientTranId", self.client_id.as_str());
        if let Some(email) = &self.from_email {
            request = request.push("fromEmail", email.clone());
        }
        if let Some(email) = &self.to_email {
            request = request.push("toEmail", email.clone());
        }
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandardWithdrawalIntent {
    pub coin: String,
    pub network: String,
    pub amount: ExactDecimal,
    pub address: String,
    pub memo: Option<String>,
    pub client_id: ClientIdentity,
}

impl StandardWithdrawalIntent {
    pub fn request(&self) -> Result<BinanceRequest, BinanceError> {
        let mut request = BinanceRequest::new(BinanceEndpoint::StandardWithdrawal)
            .push("coin", self.coin.clone())
            .push("withdrawOrderId", self.client_id.as_str())
            .push("network", self.network.clone())
            .push("address", self.address.clone())
            .push("amount", self.amount.canonical())
            .push("walletType", "0");
        if let Some(memo) = &self.memo {
            request = request.push("addressTag", memo.clone());
        }
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryWindow {
    pub start_time_ms: u64,
    pub end_time_ms: u64,
}

impl HistoryWindow {
    pub fn request(
        &self,
        endpoint: BinanceEndpoint,
        coin: Option<&str>,
        identity: Option<&ClientIdentity>,
    ) -> Result<BinanceRequest, BinanceError> {
        if self.start_time_ms > self.end_time_ms {
            return Err(BinanceError::InvalidRequest);
        }
        let mut request = BinanceRequest::new(endpoint)
            .push("startTime", self.start_time_ms.to_string())
            .push("endTime", self.end_time_ms.to_string());
        if let Some(coin) = coin {
            request = request.push("coin", coin);
        }
        match endpoint {
            BinanceEndpoint::UniversalTransferHistory => {
                request = request.push("page", "1").push("limit", "500");
                if let Some(id) = identity {
                    request = request.push("clientTranId", id.as_str());
                }
            }
            BinanceEndpoint::DepositHistory | BinanceEndpoint::WithdrawalHistory => {
                request = request.push("offset", "0").push("limit", "1000");
                if let Some(id) = identity {
                    if endpoint == BinanceEndpoint::WithdrawalHistory {
                        request = request.push("withdrawOrderId", id.as_str());
                    } else {
                        return Err(BinanceError::InvalidRequest);
                    }
                }
            }
            _ => return Err(BinanceError::InvalidRequest),
        }
        request.validate()?;
        Ok(request)
    }
}

/// Build the bounded child-credential history request used to corroborate a
/// main-account universal-transfer row whose main-side email is not enrolled.
pub fn subaccount_transfer_history_request(
    asset: &str,
    transfer_type: u64,
    start_time_ms: u64,
    end_time_ms: u64,
    limit: u64,
) -> Result<BinanceRequest, BinanceError> {
    if asset.is_empty()
        || !matches!(transfer_type, 1 | 2)
        || start_time_ms > end_time_ms
        || !(1..=200).contains(&limit)
    {
        return Err(BinanceError::InvalidRequest);
    }
    let request = BinanceRequest::new(BinanceEndpoint::SubAccountTransferHistory)
        .push("asset", asset)
        .push("type", transfer_type.to_string())
        .push("startTime", start_time_ms.to_string())
        .push("endTime", end_time_ms.to_string())
        .push("limit", limit.to_string())
        .push("returnFailHistory", "true");
    request.validate()?;
    Ok(request)
}

/// Exact financial fields are deserialized only from venue decimal strings.
/// Serde will reject JSON numeric values instead of accepting a rounded float.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct VenueDecimal(pub ExactDecimal);

impl TryFrom<String> for VenueDecimal {
    type Error = BinanceError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        ExactDecimal::parse(&value).map(Self)
    }
}

impl From<VenueDecimal> for String {
    fn from(value: VenueDecimal) -> Self {
        value.0.canonical()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SpotBalanceRow {
    pub asset: String,
    pub free: ExactDecimal,
    pub locked: ExactDecimal,
    #[serde(default)]
    pub freeze: Option<ExactDecimal>,
    #[serde(default)]
    pub withdrawing: Option<ExactDecimal>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SpotBalancesResponse {
    pub balances: Vec<SpotBalanceRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ApiRestrictions {
    #[serde(default)]
    #[serde(rename = "enableReading")]
    pub enable_reading: Option<bool>,
    #[serde(default)]
    #[serde(rename = "enableWithdrawals")]
    pub enable_withdrawals: Option<bool>,
    #[serde(default)]
    #[serde(rename = "enableInternalTransfer")]
    pub enable_internal_transfer: Option<bool>,
    #[serde(default)]
    #[serde(rename = "permitsUniversalTransfer")]
    pub permits_universal_transfer: Option<bool>,
    #[serde(default)]
    #[serde(rename = "ipRestrict")]
    pub ip_restrict: Option<bool>,
    #[serde(default)]
    #[serde(rename = "enableSpotAndMarginTrading")]
    pub enable_spot_and_margin_trading: Option<bool>,
    #[serde(default, rename = "enableFutures")]
    pub enable_futures: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct TransferHistoryResponse {
    #[serde(rename = "totalCount")]
    pub total_count: Option<u64>,
    #[serde(default)]
    pub result: Vec<TransferHistoryRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct TransferHistoryRow {
    #[serde(rename = "tranId")]
    pub tran_id: VenueIdentifier,
    #[serde(default, rename = "clientTranId")]
    pub client_tran_id: Option<String>,
    pub asset: String,
    pub amount: ExactDecimal,
    #[serde(rename = "fromAccountType")]
    pub from_account_type: String,
    #[serde(rename = "toAccountType")]
    pub to_account_type: String,
    #[serde(rename = "fromEmail")]
    pub from_email: Option<String>,
    #[serde(rename = "toEmail")]
    pub to_email: Option<String>,
    pub status: String,
    #[serde(rename = "createTimeStamp")]
    pub timestamp: u64,
}

/// Signed history returned by the child credential for spot transfers with
/// its master. Binance documents this as a bare array and does not include the
/// master universal-transfer client's `clientTranId`, so reconciliation must
/// correlate through the immutable master-side `tranId` and peer fields.
pub type SubAccountTransferHistoryResponse = Vec<SubAccountTransferHistoryRow>;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SubAccountTransferHistoryRow {
    #[serde(rename = "counterParty")]
    pub counter_party: String,
    /// Counterparty email (the master email for a master-child transfer).
    pub email: String,
    #[serde(rename = "type")]
    pub transfer_type: u64,
    pub asset: String,
    #[serde(rename = "qty")]
    pub quantity: ExactDecimal,
    #[serde(rename = "fromAccountType")]
    pub from_account_type: String,
    #[serde(rename = "toAccountType")]
    pub to_account_type: String,
    pub status: String,
    #[serde(rename = "tranId")]
    pub tran_id: VenueIdentifier,
    #[serde(rename = "time")]
    pub timestamp: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum FundingCompleteTime {
    UnixMillis(u64),
    UtcText(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct FundingHistoryRow {
    pub id: VenueIdentifier,
    pub coin: String,
    pub amount: ExactDecimal,
    #[serde(default)]
    #[serde(rename = "transactionFee")]
    pub transaction_fee: Option<ExactDecimal>,
    pub network: Option<String>,
    pub address: Option<String>,
    #[serde(rename = "addressTag")]
    pub address_tag: Option<String>,
    #[serde(rename = "txId")]
    pub tx_id: Option<String>,
    pub status: u64,
    #[serde(rename = "withdrawOrderId")]
    pub withdraw_order_id: Option<String>,
    #[serde(rename = "insertTime")]
    pub insert_time: Option<u64>,
    #[serde(rename = "applyTime")]
    pub apply_time: Option<String>,
    #[serde(rename = "completeTime")]
    pub complete_time: Option<FundingCompleteTime>,
    #[serde(rename = "confirmTimes")]
    pub confirm_times: Option<String>,
    #[serde(rename = "travelRuleStatus")]
    pub travel_rule_status: Option<u64>,
    #[serde(rename = "transferType")]
    pub transfer_type: Option<u8>,
    #[serde(rename = "walletType")]
    pub wallet_type: Option<u8>,
}

impl FundingHistoryRow {
    pub fn has_valid_deposit_complete_time(&self) -> bool {
        matches!(
            self.complete_time,
            None | Some(FundingCompleteTime::UnixMillis(_))
        )
    }

    pub fn has_valid_withdrawal_complete_time(&self) -> bool {
        match &self.complete_time {
            None => true,
            Some(FundingCompleteTime::UtcText(value)) => {
                chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                    .is_ok_and(|parsed| parsed.format("%Y-%m-%d %H:%M:%S").to_string() == *value)
            }
            Some(FundingCompleteTime::UnixMillis(_)) => false,
        }
    }

    pub fn usable_deposit_credit(&self) -> bool {
        self.status == 1 && self.travel_rule_status.is_none_or(|status| status == 0)
    }

    pub fn locked_deposit_credit(&self) -> bool {
        self.status == 6
    }

    pub fn completed_external_withdrawal(&self) -> bool {
        self.status == 6 && self.transfer_type == Some(0) && self.wallet_type == Some(0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct CapitalConfigCurrency {
    pub coin: String,
    #[serde(rename = "withdrawAllEnable")]
    pub withdraw_all_enable: Option<bool>,
    #[serde(rename = "depositAllEnable")]
    pub deposit_all_enable: Option<bool>,
    #[serde(default)]
    #[serde(rename = "networkList")]
    pub network_list: Vec<CapitalNetwork>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct CapitalNetwork {
    pub network: String,
    pub coin: String,
    #[serde(rename = "depositEnable")]
    pub deposit_enable: Option<bool>,
    #[serde(rename = "withdrawEnable")]
    pub withdraw_enable: Option<bool>,
    #[serde(rename = "withdrawIntegerMultiple")]
    pub withdraw_integer_multiple: Option<ExactDecimal>,
    #[serde(rename = "withdrawFee")]
    pub withdraw_fee: Option<ExactDecimal>,
    #[serde(rename = "withdrawMin")]
    pub withdraw_min: Option<ExactDecimal>,
    #[serde(rename = "withdrawMax")]
    pub withdraw_max: Option<ExactDecimal>,
    #[serde(rename = "depositDust")]
    pub deposit_dust: Option<ExactDecimal>,
    #[serde(rename = "minConfirm")]
    pub min_confirm: Option<u64>,
    #[serde(rename = "unLockConfirm")]
    pub unlock_confirm: Option<u64>,
    #[serde(rename = "withdrawTag")]
    pub withdraw_tag: Option<bool>,
    #[serde(rename = "addressRegex")]
    pub address_regex: Option<String>,
    #[serde(rename = "memoRegex")]
    pub memo_regex: Option<String>,
    pub busy: Option<bool>,
    #[serde(rename = "contractAddress")]
    pub contract_address: Option<String>,
    pub denomination: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct DepositAddress {
    pub coin: Option<String>,
    pub address: String,
    #[serde(default)]
    pub tag: Option<String>,
    pub network: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawWhitelistStatus {
    Enabled,
    Disabled,
    #[default]
    Unknown,
}

/// Binance currently documents originType as a string. Preserve older
/// numeric responses without coercion so callers can retain either observed
/// wire representation exactly.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum WithdrawAddressOriginType {
    Label(String),
    LegacyCode(u64),
}

fn deserialize_whitelist_status<'de, D>(
    deserializer: D,
) -> Result<WithdrawWhitelistStatus, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::Bool(true) => WithdrawWhitelistStatus::Enabled,
        Value::Bool(false) => WithdrawWhitelistStatus::Disabled,
        _ => WithdrawWhitelistStatus::Unknown,
    })
}

/// One row from Binance's fixed withdrawal-address allowlist endpoint.
/// Addresses are operational data and callers must avoid logging them.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct WithdrawAddressListEntry {
    pub address: String,
    #[serde(default, rename = "addressTag")]
    pub address_tag: Option<String>,
    pub coin: Option<String>,
    pub name: Option<String>,
    pub network: Option<String>,
    pub origin: Option<String>,
    #[serde(rename = "originType")]
    pub origin_type: Option<WithdrawAddressOriginType>,
    #[serde(
        default,
        rename = "whiteStatus",
        deserialize_with = "deserialize_whitelist_status"
    )]
    pub whitelist_status: WithdrawWhitelistStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct TravelRuleRequirements {
    #[serde(rename = "questionnaireCountryCode")]
    pub questionnaire_country_code: String,
}

/// Parse one financial response type while rejecting unexpected/non-string
/// amounts. The resulting error deliberately excludes server response text.
pub fn parse_response<T: for<'de> Deserialize<'de>>(raw: Value) -> Result<T, BinanceError> {
    serde_json::from_value(raw).map_err(|_| BinanceError::InvalidResponse)
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct VenueIdentifier(String);

impl VenueIdentifier {
    pub fn parse(value: &Value) -> Result<Self, BinanceError> {
        let text = match value {
            Value::String(value) => value.clone(),
            Value::Number(value) => {
                if let Some(number) = value.as_u64() {
                    number.to_string()
                } else if let Some(number) = value.as_i64() {
                    if number < 0 {
                        return Err(BinanceError::InvalidResponse);
                    }
                    number.to_string()
                } else {
                    return Err(BinanceError::InvalidResponse);
                }
            }
            _ => return Err(BinanceError::InvalidResponse),
        };
        if text.is_empty() || text.len() > 160 {
            return Err(BinanceError::InvalidResponse);
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for VenueIdentifier {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SubAccountListResponse {
    #[serde(default, rename = "subAccounts")]
    pub sub_accounts: Vec<SubAccountRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SubAccountRecord {
    pub email: String,
    #[serde(rename = "isFreeze")]
    pub is_freeze: Option<bool>,
    pub is_master: Option<bool>,
    #[serde(default, rename = "subAccountId")]
    pub sub_account_id: Option<VenueIdentifier>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DepositCreditState {
    pub credited: bool,
    pub locked: bool,
}

impl From<&FundingHistoryRow> for DepositCreditState {
    fn from(row: &FundingHistoryRow) -> Self {
        Self {
            credited: row.usable_deposit_credit(),
            locked: row.locked_deposit_credit(),
        }
    }
}

/// Output redaction helper: retain only an explicit, reviewed set of funding
/// fields. The raw API response must never be persisted or emitted wholesale.
pub fn redact_funding_row(row: &FundingHistoryRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id.as_str(),
        "coin": row.coin,
        "amount": row.amount.canonical(),
        "transactionFee": row.transaction_fee.as_ref().map(ExactDecimal::canonical),
        "network": row.network,
        "address": row.address,
        "addressTag": row.address_tag,
        "txId": row.tx_id,
        "status": row.status,
        "withdrawOrderId": row.withdraw_order_id,
        "insertTime": row.insert_time,
        "applyTime": row.apply_time,
        "completeTime": row.complete_time,
        "confirmTimes": row.confirm_times,
        "travelRuleStatus": row.travel_rule_status,
        "transferType": row.transfer_type,
        "walletType": row.wallet_type
    })
}

/// Binance-specific dispatch into the shared, protected CCXT runtime.
/// Persisted BinanceRequest remains unchanged; only connectivity is replaced.
pub struct BinanceCcxtTransport {
    client: Arc<fin_exchange::ccxt::CcxtClient>,
    credentials: Arc<dyn fin_exchange::ccxt::CredentialProvider>,
}

impl BinanceCcxtTransport {
    pub fn new(
        client: Arc<fin_exchange::ccxt::CcxtClient>,
        credentials: Arc<dyn fin_exchange::ccxt::CredentialProvider>,
    ) -> Self {
        Self {
            client,
            credentials,
        }
    }
}

pub fn ccxt_transport_error(error: fin_exchange::ccxt::CcxtError) -> TransportError {
    use fin_exchange::ccxt::CcxtErrorKind as Kind;
    let kind = match error.kind {
        Kind::RateLimited => HttpFailureKind::RateLimited,
        Kind::Authentication | Kind::PermissionDenied => HttpFailureKind::PermissionDenied,
        Kind::Rejected => HttpFailureKind::Rejected,
        _ => HttpFailureKind::InvalidResponse,
    };
    if let Some(status) = error.http_status {
        return TransportError::HttpFailure {
            status,
            binance_code: error.exchange_code,
            kind,
        };
    }
    match error.kind {
        Kind::Unavailable => TransportError::Unavailable,
        Kind::RateLimited => TransportError::RateLimited,
        Kind::Authentication | Kind::PermissionDenied => TransportError::PermissionDenied,
        Kind::Timeout => TransportError::Timeout,
        Kind::Rejected => TransportError::Rejected,
        _ => TransportError::InvalidResponse,
    }
}

#[async_trait]
impl BinanceTransport for BinanceCcxtTransport {
    async fn request(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
    ) -> Result<Value, TransportError> {
        self.request_inner(credential, request, None).await
    }

    async fn request_authorized(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
        journal: Arc<dyn fin_domain::SubmissionJournal>,
    ) -> Result<Value, TransportError> {
        self.request_inner(
            credential,
            request,
            Some(Arc::new(move || journal.assert_authorized().is_ok())),
        )
        .await
    }
}

impl BinanceCcxtTransport {
    async fn request_inner(
        &self,
        credential: &CredentialHandle,
        request: BinanceRequest,
        guard: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Value, TransportError> {
        use fin_exchange::ccxt::{ExchangeId, Method};
        request
            .validate()
            .map_err(|_| TransportError::InvalidResponse)?;
        let method = match request.endpoint {
            BinanceEndpoint::ExchangeInfo => Method::BinanceSpotExchangeInfo,
            BinanceEndpoint::SpotOrderStatus => Method::BinanceSpotOrder,
            BinanceEndpoint::ApiRestrictions => Method::BinanceApiRestrictions,
            BinanceEndpoint::Account => Method::BinanceAccount,
            BinanceEndpoint::SpotExchangeInfo => Method::BinanceSpotExchangeInfo,
            BinanceEndpoint::SpotCommission => Method::BinanceSpotCommission,
            BinanceEndpoint::SpotOpenOrders => Method::BinanceSpotOpenOrders,
            BinanceEndpoint::SpotOrder => Method::BinanceSpotOrder,
            BinanceEndpoint::SpotTrades => Method::BinanceSpotTrades,
            BinanceEndpoint::SpotPlaceOrder => Method::BinanceSpotPlaceOrder,
            BinanceEndpoint::SpotCancelOrder => Method::BinanceSpotCancelOrder,
            BinanceEndpoint::FuturesExchangeInfo => Method::BinanceFuturesExchangeInfo,
            BinanceEndpoint::FuturesAccount => Method::BinanceFuturesAccount,
            BinanceEndpoint::FuturesAccountConfig => Method::BinanceFuturesAccountConfig,
            BinanceEndpoint::FuturesSymbolConfig => Method::BinanceFuturesSymbolConfig,
            BinanceEndpoint::FuturesPositions => Method::BinanceFuturesPositions,
            BinanceEndpoint::FuturesPositionMode => Method::BinanceFuturesPositionMode,
            BinanceEndpoint::FuturesCommission => Method::BinanceFuturesCommission,
            BinanceEndpoint::FuturesFeeBurn => Method::BinanceFuturesFeeBurn,
            BinanceEndpoint::FuturesOrder => Method::BinanceFuturesOrder,
            BinanceEndpoint::FuturesOpenOrders => Method::BinanceFuturesOpenOrders,
            BinanceEndpoint::FuturesTrades => Method::BinanceFuturesTrades,
            BinanceEndpoint::FuturesIncome => Method::BinanceFuturesIncome,
            BinanceEndpoint::FuturesMarkPrice => Method::BinanceFuturesMarkPrice,
            BinanceEndpoint::FuturesLeverageBrackets => Method::BinanceFuturesLeverageBrackets,
            BinanceEndpoint::FuturesPlaceOrder => Method::BinanceFuturesPlaceOrder,
            BinanceEndpoint::FuturesCancelOrder => Method::BinanceFuturesCancelOrder,
            BinanceEndpoint::WalletTransfer => Method::BinanceWalletTransfer,
            BinanceEndpoint::WalletTransferHistory => Method::BinanceWalletTransferHistory,

            BinanceEndpoint::SubAccountList => Method::BinanceSubAccountList,
            BinanceEndpoint::SubAccountSpotAssets => Method::BinanceSubAccountSpotAssets,
            BinanceEndpoint::CapitalConfig => Method::BinanceCapitalConfig,
            BinanceEndpoint::DepositAddress => Method::BinanceDepositAddress,
            BinanceEndpoint::DepositAddressList => Method::BinanceDepositAddressList,
            BinanceEndpoint::WithdrawAddressList => Method::BinanceWithdrawAddressList,
            BinanceEndpoint::DepositHistory => Method::BinanceDepositHistory,
            BinanceEndpoint::WithdrawalHistory => Method::BinanceWithdrawalHistory,
            BinanceEndpoint::UniversalTransferHistory => Method::BinanceUniversalTransferHistory,
            BinanceEndpoint::SubAccountTransferHistory => Method::BinanceSubAccountTransferHistory,
            BinanceEndpoint::UniversalTransfer => Method::BinanceUniversalTransfer,
            BinanceEndpoint::SubToMasterTransfer => Method::BinanceSubToMasterTransfer,
            BinanceEndpoint::StandardWithdrawal => Method::BinanceStandardWithdrawal,
            BinanceEndpoint::TravelRuleRequirements => Method::BinanceTravelRuleRequirements,
        };
        let credentials = if matches!(
            request.endpoint,
            BinanceEndpoint::ExchangeInfo
                | BinanceEndpoint::SpotExchangeInfo
                | BinanceEndpoint::FuturesExchangeInfo
                | BinanceEndpoint::FuturesMarkPrice
        ) {
            None
        } else {
            Some(
                self.credentials
                    .credentials(credential.as_str())
                    .map_err(ccxt_transport_error)?,
            )
        };
        let params = request
            .params
            .into_iter()
            .map(|p| (p.key, p.value))
            .collect();
        match guard {
            Some(guard) => {
                self.client
                    .request_guarded(ExchangeId::Binance, method, credentials, params, guard)
                    .await
            }
            None => {
                self.client
                    .request(ExchangeId::Binance, method, credentials, params)
                    .await
            }
        }
        .map_err(ccxt_transport_error)
    }
}

#[cfg(test)]
mod trading_transport_contract_tests {
    use super::*;

    fn order(endpoint: BinanceEndpoint) -> BinanceRequest {
        let request = BinanceRequest::new(endpoint)
            .push("symbol", "BTCUSDT")
            .push("side", "BUY")
            .push("type", "LIMIT")
            .push("quantity", "0.000000000000000001")
            .push("price", "12345678901234567890.123456789012345678")
            .push("timeInForce", "GTC")
            .push("newClientOrderId", "fin-synthetic-local-only");
        if endpoint == BinanceEndpoint::FuturesPlaceOrder {
            request
                .push("positionSide", "BOTH")
                .push("reduceOnly", "true")
        } else {
            request
        }
    }

    #[test]
    fn fixed_order_requests_require_identity_and_exact_bounded_types() {
        for endpoint in [
            BinanceEndpoint::SpotPlaceOrder,
            BinanceEndpoint::FuturesPlaceOrder,
        ] {
            let valid = order(endpoint);
            valid.validate().unwrap();
            let mut missing_identity = valid.clone();
            missing_identity
                .params
                .retain(|p| p.key != "newClientOrderId");
            assert_eq!(
                missing_identity.validate(),
                Err(BinanceError::MissingParameter)
            );
            let mut invalid_decimal = valid.clone();
            invalid_decimal
                .params
                .iter_mut()
                .find(|p| p.key == "quantity")
                .unwrap()
                .value = "1e-18".into();
            assert!(invalid_decimal.validate().is_err());
            let mut unsupported_type = valid.clone();
            unsupported_type
                .params
                .iter_mut()
                .find(|p| p.key == "type")
                .unwrap()
                .value = "STOP_MARKET".into();
            assert_eq!(
                unsupported_type.validate(),
                Err(BinanceError::InvalidRequest)
            );
            assert_eq!(
                valid.clone().push("timestamp", "1").validate(),
                Err(BinanceError::UnsupportedParameter)
            );
            assert_eq!(
                valid.push("quantity", "1").validate(),
                Err(BinanceError::DuplicateParameter)
            );
        }
    }

    #[test]
    fn futures_orders_reject_hedge_reduce_only_and_full_response() {
        let mut hedge = order(BinanceEndpoint::FuturesPlaceOrder);
        hedge
            .params
            .iter_mut()
            .find(|p| p.key == "positionSide")
            .unwrap()
            .value = "LONG".into();
        assert_eq!(hedge.validate(), Err(BinanceError::InvalidRequest));
        hedge.params.retain(|p| p.key != "reduceOnly");
        hedge.validate().unwrap();
        assert_eq!(
            hedge.push("newOrderRespType", "FULL").validate(),
            Err(BinanceError::InvalidRequest)
        );
    }

    #[test]
    fn cancel_and_lookup_require_one_unambiguous_order_identity() {
        for endpoint in [
            BinanceEndpoint::SpotOrder,
            BinanceEndpoint::FuturesOrder,
            BinanceEndpoint::SpotCancelOrder,
            BinanceEndpoint::FuturesCancelOrder,
        ] {
            let request = BinanceRequest::new(endpoint).push("symbol", "BTCUSDT");
            assert_eq!(request.validate(), Err(BinanceError::InvalidRequest));
            request
                .clone()
                .push("orderId", "18446744073709551615")
                .validate()
                .unwrap();
            request
                .clone()
                .push("origClientOrderId", "fin-synthetic-local-only")
                .validate()
                .unwrap();
            assert_eq!(
                request
                    .push("orderId", "1")
                    .push("origClientOrderId", "fin-synthetic-local-only")
                    .validate(),
                Err(BinanceError::InvalidRequest)
            );
        }
    }

    #[test]
    fn wallet_transfer_is_only_spot_and_usdm_cash() {
        for route in ["MAIN_UMFUTURE", "UMFUTURE_MAIN"] {
            BinanceRequest::new(BinanceEndpoint::WalletTransfer)
                .push("type", route)
                .push("asset", "USDT")
                .push("amount", "0.123456789012345678")
                .validate()
                .unwrap();
            BinanceRequest::new(BinanceEndpoint::WalletTransferHistory)
                .push("type", route)
                .validate()
                .unwrap();
        }
        assert_eq!(
            BinanceRequest::new(BinanceEndpoint::WalletTransfer)
                .push("type", "MAIN_MARGIN")
                .push("asset", "USDT")
                .push("amount", "1")
                .validate(),
            Err(BinanceError::InvalidRequest)
        );
    }

    #[tokio::test]
    async fn public_market_transport_never_resolves_credentials() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct RefuseCredentials(Arc<AtomicUsize>);
        impl fin_exchange::ccxt::CredentialProvider for RefuseCredentials {
            fn credentials(
                &self,
                _: &str,
            ) -> Result<fin_exchange::ccxt::Credentials, fin_exchange::ccxt::CcxtError>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(fin_exchange::ccxt::CcxtError::new(
                    fin_exchange::ccxt::CcxtErrorKind::Authentication,
                ))
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0; 2048];
                    let n = stream.read(&mut buf).unwrap();
                    bytes.extend_from_slice(&buf[..n]);
                    if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let wire = String::from_utf8(bytes).unwrap();
                assert!(!wire.to_ascii_lowercase().contains("x-mbx-apikey"));
                assert!(!wire.contains("signature="));
                paths.push(wire.lines().next().unwrap().to_owned());
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}").unwrap();
            }
            paths
        });
        let lookups = Arc::new(AtomicUsize::new(0));
        let transport = BinanceCcxtTransport::new(
            Arc::new(fin_exchange::ccxt::CcxtClient::for_test(&url).unwrap()),
            Arc::new(RefuseCredentials(lookups.clone())),
        );
        let handle = CredentialHandle::new("synthetic-public-market-data").unwrap();
        for endpoint in [
            BinanceEndpoint::SpotExchangeInfo,
            BinanceEndpoint::FuturesExchangeInfo,
            BinanceEndpoint::FuturesMarkPrice,
        ] {
            transport
                .request(&handle, BinanceRequest::new(endpoint))
                .await
                .unwrap();
        }
        assert_eq!(lookups.load(Ordering::SeqCst), 0);
        assert_eq!(
            worker.join().unwrap(),
            [
                "GET /api/v3/exchangeInfo HTTP/1.1",
                "GET /fapi/v1/exchangeInfo HTTP/1.1",
                "GET /fapi/v1/premiumIndex HTTP/1.1"
            ]
        );
    }

    #[test]
    fn futures_permission_is_reported_separately_and_defaults_unknown() {
        let reported: ApiRestrictions =
            parse_response(serde_json::json!({"enableFutures": true})).unwrap();
        assert_eq!(reported.enable_futures, Some(true));
        assert_eq!(reported.enable_spot_and_margin_trading, None);
        let unknown: ApiRestrictions = parse_response(serde_json::json!({})).unwrap();
        assert_eq!(unknown.enable_futures, None);
    }
}
