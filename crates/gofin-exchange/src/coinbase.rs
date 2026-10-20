//! Coinbase REST transport with a closed route set and explicit authentication.
//! Raw paths, headers and tokens are never accepted from callers.
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ed25519_dalek::SigningKey as EdSigningKey;
use hmac::{Hmac, Mac};
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use serde::Serialize;
use sha2::Sha256;
use std::fmt;
use std::sync::Arc;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

mod ccxt_request;
#[cfg(test)]
mod ccxt_request_tests;
pub mod reads;

type HmacSha256 = Hmac<Sha256>;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Only reviewed Coinbase endpoints can be addressed through this transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoinbaseRoute {
    NativeRead {
        route: reads::CoinbaseReadRoute,
        parameters: Vec<String>,
        query: CoinbaseQuery,
    },
    KeyPermissions,
    LegacyCurrencies,
    LegacyCryptoCurrencies,
    LegacyAccounts {
        query: CoinbaseQuery,
    },
    Accounts {
        query: CoinbaseQuery,
    },
    Account {
        account_id: String,
    },
    Portfolios,
    PortfolioCreate,
    PortfolioDetail {
        portfolio_id: String,
    },
    PortfolioUpdate {
        portfolio_id: String,
    },
    PortfolioDelete {
        portfolio_id: String,
    },
    CfmSweepSchedule,
    CfmMarginSetting,
    CfmCancelSweeps,
    Products {
        query: CoinbaseQuery,
    },
    Product {
        product_id: String,
    },
    ProductBook {
        product_id: String,
    },
    OrderPreview,
    OrderEditPreview,
    Orders,
    OrderEdit,
    OrderCancel,
    OrderCancelBatch,
    Order {
        order_id: String,
    },
    HistoricalOrders {
        query: CoinbaseQuery,
    },
    Fills {
        query: CoinbaseQuery,
    },
    Fees,
    DepositAddresses {
        account_id: String,
    },
    DepositAddressCreate {
        account_id: String,
    },
    Transactions {
        account_id: String,
        query: CoinbaseQuery,
    },
    Transaction {
        account_id: String,
        transaction_id: String,
    },
    TransactionSend {
        account_id: String,
    },
    ConvertQuote,
    ConvertTrade {
        trade_id: String,
    },
    ConvertTradeExact {
        trade_id: String,
        from_account: String,
        to_account: String,
    },
    ConvertCommit {
        trade_id: String,
    },
    PortfolioMoveFunds,
    ExchangeAccounts,
    ExchangeAccount {
        account_id: String,
    },
    ExchangeProfiles,
    ExchangeProfile {
        profile_id: String,
    },
    ExchangeProfileCreate,
    ExchangeProfileRename {
        profile_id: String,
    },
    ExchangeOrders,
    ExchangeOrdersList {
        query: CoinbaseQuery,
    },
    ExchangeCancelOrder {
        order_id: String,
    },
    ExchangeCancelOrderScoped {
        order_id: String,
        product_id: String,
        profile_id: String,
    },
    ExchangeOrder {
        order_id: String,
    },
    ExchangeOrderByClientId {
        client_oid: String,
    },
    ExchangeFills,
    ExchangeFillsFiltered {
        query: CoinbaseQuery,
    },
    ExchangeFees,
    ExchangeReportCreate,
    ExchangeLoanLendingOverview,
    ExchangeDepositAddresses {
        coinbase_account_id: String,
    },
    ExchangeDepositAddressCreate {
        coinbase_account_id: String,
    },
    ExchangeTransfers {
        account_id: String,
        query: CoinbaseQuery,
    },
    ExchangeProfileTransfers {
        query: CoinbaseQuery,
    },
    ExchangeProfileTransfer {
        transfer_id: String,
    },
    ExchangeDeposit,
    ExchangeWithdraw,
    InternationalAccounts,
    InternationalOrders,
    InternationalOrdersList,
    InternationalOrdersFiltered {
        query: CoinbaseQuery,
    },
    InternationalCancelOrder {
        order_id: String,
    },
    InternationalCancelOrderScoped {
        order_id: String,
        portfolio_id: String,
    },
    InternationalPortfolioTransfer,
    InternationalPortfolioDetail {
        portfolio_id: String,
    },
    InternationalPortfolioSummary {
        portfolio_id: String,
    },
    InternationalPortfolioBalances {
        portfolio_id: String,
    },
    InternationalPortfolioBalance {
        portfolio_id: String,
        asset: String,
    },
    InternationalPortfolioPositions {
        portfolio_id: String,
    },
    InternationalPortfolioPosition {
        portfolio_id: String,
        instrument: String,
    },
    InternationalPortfolioLoans {
        portfolio_id: String,
    },
    InternationalPortfolioLoan {
        portfolio_id: String,
        asset: String,
    },
    InternationalPortfolioLoanAvailability {
        portfolio_id: String,
        asset: String,
    },
    InternationalPortfolioMarginCallStatus {
        portfolio_id: String,
    },
    InternationalPortfolioCreate,
    InternationalPortfolioUpdate {
        portfolio_id: String,
    },
    InternationalPortfolioPatch {
        portfolio_id: String,
    },
    InternationalPortfolioMargin,
    InternationalCrossCollateral {
        portfolio_id: String,
    },
    InternationalAutoMargin {
        portfolio_id: String,
    },
    InternationalTransfers {
        query: CoinbaseQuery,
    },
    InternationalTransfer {
        transfer_uuid: String,
    },
    InternationalEditOrder {
        order_id: String,
    },
    InternationalOrder {
        order_id: String,
    },
    InternationalOrderScoped {
        order_id: String,
        portfolio_id: String,
    },
    InternationalFills,
    InternationalPortfolioFills {
        portfolio_id: String,
    },
    InternationalPortfolioFillsFiltered {
        portfolio_id: String,
        query: CoinbaseQuery,
    },
}
/// The supported query vocabulary. Each endpoint validates which fields it accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoinbaseQuery {
    pub offset: Option<u32>,
    pub level: Option<u8>,
    pub granularity: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub currency: Option<String>,
    pub date: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u16>,
    pub product_id: Option<String>,
    pub account_id: Option<String>,
    pub portfolio_id: Option<String>,
    pub profile_id: Option<String>,
    pub status: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub starting_after: Option<String>,
    pub ending_before: Option<String>,
    pub order_id: Option<String>,
    pub product_type: Option<String>,
    pub product_venue: Option<String>,
    pub order_ids: Vec<String>,
    pub product_ids: Vec<String>,
    pub transfer_type: Option<String>,
    pub time_from: Option<String>,
    pub time_to: Option<String>,
    pub result_limit: Option<u16>,
    pub result_offset: Option<u32>,
    pub portfolios: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub instrument: Option<String>,
    pub client_order_id: Option<String>,
    pub ref_datetime: Option<String>,
}
pub type CoinbaseEndpoint = CoinbaseRoute;

#[derive(Clone, Copy)]
enum Host {
    App,
    Exchange,
    International,
}
impl CoinbaseRoute {
    fn host(&self) -> Host {
        match self {
            Self::NativeRead { route, .. } => route.host(),
            Self::ExchangeAccounts
            | Self::ExchangeAccount { .. }
            | Self::ExchangeProfiles
            | Self::ExchangeProfile { .. }
            | Self::ExchangeProfileCreate
            | Self::ExchangeProfileRename { .. }
            | Self::ExchangeOrders
            | Self::ExchangeOrdersList { .. }
            | Self::ExchangeCancelOrder { .. }
            | Self::ExchangeCancelOrderScoped { .. }
            | Self::ExchangeOrder { .. }
            | Self::ExchangeOrderByClientId { .. }
            | Self::ExchangeFills
            | Self::ExchangeFillsFiltered { .. }
            | Self::ExchangeFees
            | Self::ExchangeReportCreate
            | Self::ExchangeLoanLendingOverview
            | Self::ExchangeDepositAddresses { .. }
            | Self::ExchangeDepositAddressCreate { .. }
            | Self::ExchangeTransfers { .. }
            | Self::ExchangeProfileTransfers { .. }
            | Self::ExchangeProfileTransfer { .. }
            | Self::ExchangeDeposit
            | Self::ExchangeWithdraw => Host::Exchange,
            Self::InternationalAccounts
            | Self::InternationalOrders
            | Self::InternationalOrdersList
            | Self::InternationalOrdersFiltered { .. }
            | Self::InternationalCancelOrder { .. }
            | Self::InternationalCancelOrderScoped { .. }
            | Self::InternationalEditOrder { .. }
            | Self::InternationalOrder { .. }
            | Self::InternationalOrderScoped { .. }
            | Self::InternationalFills
            | Self::InternationalPortfolioFills { .. }
            | Self::InternationalPortfolioFillsFiltered { .. }
            | Self::InternationalPortfolioTransfer
            | Self::InternationalPortfolioDetail { .. }
            | Self::InternationalPortfolioSummary { .. }
            | Self::InternationalPortfolioBalances { .. }
            | Self::InternationalPortfolioBalance { .. }
            | Self::InternationalPortfolioPositions { .. }
            | Self::InternationalPortfolioPosition { .. }
            | Self::InternationalPortfolioLoans { .. }
            | Self::InternationalPortfolioLoan { .. }
            | Self::InternationalPortfolioLoanAvailability { .. }
            | Self::InternationalPortfolioMarginCallStatus { .. }
            | Self::InternationalPortfolioCreate
            | Self::InternationalPortfolioUpdate { .. }
            | Self::InternationalPortfolioPatch { .. }
            | Self::InternationalPortfolioMargin
            | Self::InternationalCrossCollateral { .. }
            | Self::InternationalAutoMargin { .. }
            | Self::InternationalTransfers { .. }
            | Self::InternationalTransfer { .. } => Host::International,
            _ => Host::App,
        }
    }
    fn route_path(&self) -> Result<String, CoinbaseError> {
        let path = match self {
            Self::NativeRead {
                route,
                parameters,
                query,
            } => route.path(parameters, query),
            Self::KeyPermissions => Ok("/api/v3/brokerage/key_permissions".into()),
            Self::LegacyCurrencies => Ok("/v2/currencies".into()),
            Self::LegacyCryptoCurrencies => Ok("/v2/currencies/crypto".into()),
            Self::LegacyAccounts { query } => query_path(
                "/v2/accounts",
                query,
                &[
                    ("starting_after", "starting_after"),
                    ("ending_before", "ending_before"),
                    ("limit", "limit"),
                ],
            ),
            Self::Accounts { query } => query_path(
                "/api/v3/brokerage/accounts",
                query,
                &[
                    ("cursor", "cursor"),
                    ("limit", "limit"),
                    ("portfolio_id", "retail_portfolio_id"),
                ],
            ),
            Self::Account { account_id } => Ok(format!(
                "/api/v3/brokerage/accounts/{}",
                segment(account_id)
            )),
            Self::Portfolios => Ok("/api/v3/brokerage/portfolios".into()),
            Self::PortfolioCreate => Ok("/api/v3/brokerage/portfolios".into()),
            Self::PortfolioDetail { portfolio_id }
            | Self::PortfolioUpdate { portfolio_id }
            | Self::PortfolioDelete { portfolio_id } => Ok(format!(
                "/api/v3/brokerage/portfolios/{}",
                segment(portfolio_id)
            )),
            Self::CfmSweepSchedule => Ok("/api/v3/brokerage/cfm/sweeps/schedule".into()),
            Self::CfmMarginSetting => Ok("/api/v3/brokerage/cfm/intraday/margin_setting".into()),
            Self::CfmCancelSweeps => Ok("/api/v3/brokerage/cfm/sweeps".into()),
            Self::Products { query } => query_path(
                "/api/v3/brokerage/market/products",
                query,
                &[
                    ("cursor", "cursor"),
                    ("limit", "limit"),
                    ("product_type", "product_type"),
                    ("product_id", "product_id"),
                    ("product_venue", "product_venue"),
                ],
            ),
            Self::Product { product_id } => Ok(format!(
                "/api/v3/brokerage/market/products/{}",
                segment(product_id)
            )),
            Self::ProductBook { product_id } => Ok(format!(
                "/api/v3/brokerage/market/product_book?product_id={}",
                segment(product_id)
            )),
            Self::Orders => Ok("/api/v3/brokerage/orders".into()),
            Self::OrderPreview => Ok("/api/v3/brokerage/orders/preview".into()),
            Self::OrderEditPreview => Ok("/api/v3/brokerage/orders/edit_preview".into()),
            Self::OrderEdit => Ok("/api/v3/brokerage/orders/edit".into()),
            Self::OrderCancel | Self::OrderCancelBatch => {
                Ok("/api/v3/brokerage/orders/batch_cancel".into())
            }
            Self::Order { order_id } => Ok(format!(
                "/api/v3/brokerage/orders/historical/{}",
                segment(order_id)
            )),
            Self::HistoricalOrders { query } => query_path(
                "/api/v3/brokerage/orders/historical/batch",
                query,
                &[
                    ("cursor", "cursor"),
                    ("limit", "limit"),
                    ("product_ids", "product_ids"),
                    ("status", "order_status"),
                    ("order_ids", "order_ids"),
                ],
            ),
            Self::Fills { query } => query_path(
                "/api/v3/brokerage/orders/historical/fills",
                query,
                &[
                    ("cursor", "cursor"),
                    ("limit", "limit"),
                    ("product_ids", "product_ids"),
                    ("order_ids", "order_ids"),
                    ("start_time", "start_sequence_timestamp"),
                    ("end_time", "end_sequence_timestamp"),
                ],
            ),
            Self::Fees => Ok("/api/v3/brokerage/transaction_summary".into()),
            Self::DepositAddresses { account_id } => {
                Ok(format!("/v2/accounts/{}/addresses", segment(account_id)))
            }
            Self::DepositAddressCreate { account_id } => {
                Ok(format!("/v2/accounts/{}/addresses", segment(account_id)))
            }
            Self::Transactions { account_id, query } => query_path(
                &format!("/v2/accounts/{}/transactions", segment(account_id)),
                query,
                &[
                    ("starting_after", "starting_after"),
                    ("ending_before", "ending_before"),
                    ("limit", "limit"),
                ],
            ),
            Self::Transaction {
                account_id,
                transaction_id,
            } => Ok(format!(
                "/v2/accounts/{}/transactions/{}",
                segment(account_id),
                segment(transaction_id)
            )),
            Self::TransactionSend { account_id } => {
                Ok(format!("/v2/accounts/{}/transactions", segment(account_id)))
            }
            Self::ConvertQuote => Ok("/api/v3/brokerage/convert/quote".into()),
            Self::ConvertTrade { trade_id } | Self::ConvertCommit { trade_id } => Ok(format!(
                "/api/v3/brokerage/convert/trade/{}",
                segment(trade_id)
            )),
            Self::ConvertTradeExact {
                trade_id,
                from_account,
                to_account,
            } => Ok(format!(
                "/api/v3/brokerage/convert/trade/{}?from_account={}&to_account={}",
                segment(trade_id),
                segment(from_account),
                segment(to_account)
            )),
            Self::ExchangeAccounts => Ok("/accounts".into()),
            Self::ExchangeAccount { account_id } => {
                Ok(format!("/accounts/{}", segment(account_id)))
            }
            Self::ExchangeProfiles => Ok("/profiles".into()),
            Self::ExchangeProfile { profile_id } | Self::ExchangeProfileRename { profile_id } => {
                Ok(format!("/profiles/{}", segment(profile_id)))
            }
            Self::ExchangeProfileCreate => Ok("/profiles".into()),
            Self::ExchangeOrders => Ok("/orders".into()),
            Self::ExchangeOrdersList { query } => query_path(
                "/orders",
                query,
                &[
                    ("status", "status"),
                    ("product_id", "product_id"),
                    ("profile_id", "profile_id"),
                    ("before", "before"),
                    ("after", "after"),
                    ("limit", "limit"),
                ],
            ),
            Self::ExchangeCancelOrder { order_id } => Ok(format!("/orders/{}", segment(order_id))),
            Self::ExchangeCancelOrderScoped {
                order_id,
                product_id,
                profile_id,
            } => Ok(format!(
                "/orders/{}?product_id={}&profile_id={}",
                segment(order_id),
                segment(product_id),
                segment(profile_id)
            )),
            Self::ExchangeOrder { order_id } => Ok(format!("/orders/{}", segment(order_id))),
            Self::ExchangeOrderByClientId { client_oid } => {
                Ok(format!("/orders/client:{}", segment(client_oid)))
            }
            Self::ExchangeFills => Ok("/fills".into()),
            Self::ExchangeFillsFiltered { query } => query_path(
                "/fills",
                query,
                &[
                    ("product_id", "product_id"),
                    ("order_id", "order_id"),
                    ("profile_id", "profile_id"),
                    ("before", "before"),
                    ("after", "after"),
                    ("limit", "limit"),
                ],
            ),
            Self::ExchangeFees => Ok("/fees".into()),
            Self::ExchangeReportCreate => Ok("/reports".into()),
            Self::ExchangeLoanLendingOverview => Ok("/loans/lending-overview".into()),
            Self::ExchangeDepositAddresses {
                coinbase_account_id,
            }
            | Self::ExchangeDepositAddressCreate {
                coinbase_account_id,
            } => Ok(format!(
                "/coinbase-accounts/{}/addresses",
                segment(coinbase_account_id)
            )),
            Self::ExchangeTransfers { account_id, query } => query_path(
                &format!("/accounts/{}/transfers", segment(account_id)),
                query,
                &[
                    ("limit", "limit"),
                    ("starting_after", "starting_after"),
                    ("ending_before", "ending_before"),
                ],
            ),
            Self::ExchangeProfileTransfers { query } => query_path(
                "/transfers",
                query,
                &[
                    ("transfer_type", "type"),
                    ("profile_id", "profile_id"),
                    ("currency", "currency"),
                    ("starting_after", "starting_after"),
                    ("ending_before", "ending_before"),
                    ("limit", "limit"),
                ],
            ),
            Self::ExchangeProfileTransfer { transfer_id } => {
                Ok(format!("/transfers/{}", segment(transfer_id)))
            }
            Self::ExchangeDeposit => Ok("/deposits/coinbase-account".into()),
            Self::ExchangeWithdraw => Ok("/withdrawals/coinbase-account".into()),
            Self::PortfolioMoveFunds => Ok("/api/v3/brokerage/portfolios/move_funds".into()),
            Self::InternationalAccounts => Ok("/api/v1/portfolios".into()),
            Self::InternationalOrders => Ok("/api/v1/orders".into()),
            Self::InternationalOrdersList => Ok("/api/v1/orders".into()),
            Self::InternationalOrdersFiltered { query } => {
                if query.result_limit.is_some_and(|n| n > 100) {
                    return Err(CoinbaseError::InvalidRequest);
                }
                query_path(
                    "/api/v1/orders",
                    query,
                    &[
                        ("portfolio_id", "portfolio"),
                        ("instrument", "instrument"),
                        ("client_order_id", "client_order_id"),
                        ("result_limit", "result_limit"),
                        ("result_offset", "result_offset"),
                    ],
                )
            }
            Self::InternationalCancelOrder { order_id } => {
                Ok(format!("/api/v1/orders/{}", segment(order_id)))
            }
            Self::InternationalCancelOrderScoped {
                order_id,
                portfolio_id,
            } => query_path(
                &format!("/api/v1/orders/{}", segment(order_id)),
                &CoinbaseQuery {
                    portfolio_id: Some(portfolio_id.clone()),
                    ..Default::default()
                },
                &[("portfolio_id", "portfolio")],
            ),
            Self::InternationalEditOrder { order_id } => {
                Ok(format!("/api/v1/orders/{}", segment(order_id)))
            }
            Self::InternationalOrder { order_id } => {
                Ok(format!("/api/v1/orders/{}", segment(order_id)))
            }
            Self::InternationalOrderScoped {
                order_id,
                portfolio_id,
            } => query_path(
                &format!("/api/v1/orders/{}", segment(order_id)),
                &CoinbaseQuery {
                    portfolio_id: Some(portfolio_id.clone()),
                    ..Default::default()
                },
                &[("portfolio_id", "portfolio")],
            ),
            Self::InternationalFills => Ok("/api/v1/portfolios/fills".into()),
            Self::InternationalPortfolioFills { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/fills",
                segment(portfolio_id)
            )),
            Self::InternationalPortfolioFillsFiltered {
                portfolio_id,
                query,
            } => {
                if query.result_limit.is_some_and(|n| n > 100) {
                    return Err(CoinbaseError::InvalidRequest);
                }
                query_path(
                    &format!("/api/v1/portfolios/{}/fills", segment(portfolio_id)),
                    query,
                    &[
                        ("order_id", "order_id"),
                        ("client_order_id", "client_order_id"),
                        ("ref_datetime", "ref_datetime"),
                        ("result_limit", "result_limit"),
                        ("result_offset", "result_offset"),
                        ("time_from", "time_from"),
                    ],
                )
            }
            Self::InternationalPortfolioTransfer => Ok("/api/v1/portfolios/transfer".into()),
            Self::InternationalPortfolioDetail { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/detail",
                segment(portfolio_id)
            )),
            Self::InternationalPortfolioSummary { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/summary",
                segment(portfolio_id)
            )),
            Self::InternationalPortfolioBalances { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/balances",
                segment(portfolio_id)
            )),
            Self::InternationalPortfolioBalance {
                portfolio_id,
                asset,
            } => Ok(format!(
                "/api/v1/portfolios/{}/balances/{}",
                segment(portfolio_id),
                segment(asset)
            )),
            Self::InternationalPortfolioPositions { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/positions",
                segment(portfolio_id)
            )),
            Self::InternationalPortfolioPosition {
                portfolio_id,
                instrument,
            } => Ok(format!(
                "/api/v1/portfolios/{}/positions/{}",
                segment(portfolio_id),
                segment(instrument)
            )),
            Self::InternationalPortfolioLoans { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/loans",
                segment(portfolio_id)
            )),
            Self::InternationalPortfolioLoan {
                portfolio_id,
                asset,
            } => Ok(format!(
                "/api/v1/portfolios/{}/loans/{}",
                segment(portfolio_id),
                segment(asset)
            )),
            Self::InternationalPortfolioLoanAvailability {
                portfolio_id,
                asset,
            } => Ok(format!(
                "/api/v1/portfolios/{}/loans/{}/availability",
                segment(portfolio_id),
                segment(asset)
            )),
            Self::InternationalPortfolioMarginCallStatus { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/margin-call-status",
                segment(portfolio_id)
            )),
            Self::InternationalPortfolioCreate => Ok("/api/v1/portfolios".into()),
            Self::InternationalPortfolioUpdate { portfolio_id }
            | Self::InternationalPortfolioPatch { portfolio_id } => {
                Ok(format!("/api/v1/portfolios/{}", segment(portfolio_id)))
            }
            Self::InternationalPortfolioMargin => Ok("/api/v1/portfolios/margin".into()),
            Self::InternationalCrossCollateral { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/cross-collateral-enabled",
                segment(portfolio_id)
            )),
            Self::InternationalAutoMargin { portfolio_id } => Ok(format!(
                "/api/v1/portfolios/{}/auto-margin-enabled",
                segment(portfolio_id)
            )),
            Self::InternationalTransfers { query } => query_path(
                "/api/v1/transfers",
                query,
                &[
                    ("transfer_type", "type"),
                    ("time_from", "time_from"),
                    ("time_to", "time_to"),
                    ("result_limit", "result_limit"),
                    ("result_offset", "result_offset"),
                    ("status", "status"),
                    ("portfolios", "portfolios"),
                ],
            ),
            Self::InternationalTransfer { transfer_uuid } => {
                Ok(format!("/api/v1/transfers/{}", segment(transfer_uuid)))
            }
        }?;
        let path_only = path.split('?').next().unwrap_or(&path);
        if path_only
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
        {
            return Err(CoinbaseError::InvalidRequest);
        }
        Ok(path)
    }
    pub fn method(&self) -> &'static str {
        match self {
            Self::NativeRead { .. } => "GET",
            Self::LegacyCurrencies
            | Self::LegacyAccounts { .. }
            | Self::Accounts { .. }
            | Self::Account { .. }
            | Self::Portfolios
            | Self::PortfolioDetail { .. }
            | Self::Products { .. }
            | Self::Product { .. }
            | Self::ProductBook { .. }
            | Self::Order { .. }
            | Self::HistoricalOrders { .. }
            | Self::Fills { .. }
            | Self::Fees
            | Self::DepositAddresses { .. }
            | Self::Transactions { .. }
            | Self::Transaction { .. }
            | Self::ExchangeAccounts
            | Self::ExchangeAccount { .. }
            | Self::ExchangeProfiles
            | Self::ExchangeProfile { .. }
            | Self::ExchangeOrder { .. }
            | Self::ExchangeOrderByClientId { .. }
            | Self::ExchangeOrdersList { .. }
            | Self::ExchangeFills
            | Self::ExchangeFillsFiltered { .. }
            | Self::ExchangeFees
            | Self::ExchangeLoanLendingOverview
            | Self::ExchangeDepositAddresses { .. }
            | Self::ExchangeTransfers { .. }
            | Self::ExchangeProfileTransfers { .. }
            | Self::ExchangeProfileTransfer { .. }
            | Self::ConvertTrade { .. }
            | Self::ConvertTradeExact { .. }
            | Self::InternationalAccounts
            | Self::InternationalOrder { .. }
            | Self::InternationalOrderScoped { .. }
            | Self::InternationalOrdersList
            | Self::InternationalOrdersFiltered { .. }
            | Self::InternationalFills
            | Self::InternationalPortfolioDetail { .. }
            | Self::InternationalPortfolioSummary { .. }
            | Self::InternationalPortfolioBalances { .. }
            | Self::InternationalPortfolioBalance { .. }
            | Self::InternationalPortfolioPositions { .. }
            | Self::InternationalPortfolioPosition { .. }
            | Self::InternationalPortfolioLoans { .. }
            | Self::InternationalPortfolioLoan { .. }
            | Self::InternationalPortfolioLoanAvailability { .. }
            | Self::InternationalPortfolioMarginCallStatus { .. }
            | Self::InternationalTransfers { .. }
            | Self::InternationalTransfer { .. } => "GET",
            Self::InternationalPortfolioFills { .. }
            | Self::InternationalPortfolioFillsFiltered { .. } => "GET",
            Self::KeyPermissions => "GET",
            Self::LegacyCryptoCurrencies => "GET",
            Self::OrderCancel | Self::OrderCancelBatch => "POST",
            Self::OrderEdit => "POST",
            Self::ExchangeCancelOrder { .. }
            | Self::ExchangeCancelOrderScoped { .. }
            | Self::PortfolioDelete { .. }
            | Self::CfmCancelSweeps
            | Self::InternationalCancelOrder { .. } => "DELETE",
            Self::InternationalCancelOrderScoped { .. } => "DELETE",
            Self::InternationalEditOrder { .. }
            | Self::PortfolioUpdate { .. }
            | Self::InternationalPortfolioUpdate { .. } => "PUT",
            Self::InternationalPortfolioPatch { .. } => "PATCH",
            Self::Orders
            | Self::PortfolioCreate
            | Self::OrderPreview
            | Self::OrderEditPreview
            | Self::ConvertQuote
            | Self::ConvertCommit { .. }
            | Self::ExchangeOrders
            | Self::ExchangeReportCreate
            | Self::ExchangeDeposit
            | Self::ExchangeWithdraw
            | Self::InternationalOrders
            | Self::DepositAddressCreate { .. }
            | Self::TransactionSend { .. }
            | Self::ExchangeDepositAddressCreate { .. }
            | Self::PortfolioMoveFunds
            | Self::CfmSweepSchedule
            | Self::CfmMarginSetting
            | Self::InternationalPortfolioTransfer => "POST",
            Self::InternationalPortfolioCreate
            | Self::InternationalPortfolioMargin
            | Self::InternationalCrossCollateral { .. }
            | Self::InternationalAutoMargin { .. } => "POST",
            Self::ExchangeProfileCreate => "POST",
            Self::ExchangeProfileRename { .. } => "PUT",
        }
    }
    pub fn path(&self) -> Result<String, CoinbaseError> {
        self.route_path()
    }
    pub fn mutating(&self) -> bool {
        matches!(
            self,
            Self::Orders
                | Self::PortfolioCreate
                | Self::PortfolioUpdate { .. }
                | Self::PortfolioDelete { .. }
                | Self::CfmSweepSchedule
                | Self::CfmMarginSetting
                | Self::CfmCancelSweeps
                | Self::OrderEdit
                | Self::OrderCancel
                | Self::OrderCancelBatch
                | Self::DepositAddressCreate { .. }
                | Self::TransactionSend { .. }
                | Self::ConvertCommit { .. }
                | Self::ExchangeOrders
                | Self::ExchangeProfileCreate
                | Self::ExchangeProfileRename { .. }
                | Self::ExchangeReportCreate
                | Self::ExchangeCancelOrder { .. }
                | Self::ExchangeCancelOrderScoped { .. }
                | Self::ExchangeDepositAddressCreate { .. }
                | Self::ExchangeDeposit
                | Self::ExchangeWithdraw
                | Self::InternationalOrders
                | Self::InternationalCancelOrder { .. }
                | Self::InternationalCancelOrderScoped { .. }
                | Self::InternationalEditOrder { .. }
                | Self::PortfolioMoveFunds
                | Self::InternationalPortfolioTransfer
                | Self::InternationalPortfolioCreate
                | Self::InternationalPortfolioUpdate { .. }
                | Self::InternationalPortfolioPatch { .. }
                | Self::InternationalPortfolioMargin
                | Self::InternationalCrossCollateral { .. }
                | Self::InternationalAutoMargin { .. }
        )
    }
    fn url(&self) -> Result<String, CoinbaseError> {
        let origin = match self.host() {
            Host::App => "https://api.coinbase.com",
            Host::Exchange => "https://api.exchange.coinbase.com",
            Host::International => "https://api.international.coinbase.com",
        };
        Ok(format!("{origin}{}", self.route_path()?))
    }
    fn authority(&self) -> &'static str {
        match self.host() {
            Host::App => "api.coinbase.com",
            Host::Exchange => "api.exchange.coinbase.com",
            Host::International => "api.international.coinbase.com",
        }
    }
}
fn segment(value: &str) -> String {
    const SEGMENT_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, SEGMENT_ENCODE_SET).to_string()
}
fn capture_pagination(
    headers: &reqwest::header::HeaderMap,
) -> Result<CoinbasePagination, CoinbaseError> {
    fn cursor(
        headers: &reqwest::header::HeaderMap,
        name: &'static str,
    ) -> Result<Option<String>, CoinbaseError> {
        let Some(value) = headers.get(name) else {
            return Ok(None);
        };
        let value = value.to_str().map_err(|_| CoinbaseError::Transport)?;
        if value.is_empty() {
            return Ok(None);
        }
        if value.len() > 1024 || value.chars().any(char::is_control) {
            return Err(CoinbaseError::Transport);
        }
        Ok(Some(value.to_owned()))
    }
    Ok(CoinbasePagination {
        after: cursor(headers, "cb-after")?,
        before: cursor(headers, "cb-before")?,
    })
}
fn query_path(
    base: &str,
    query: &CoinbaseQuery,
    allowed: &[(&str, &str)],
) -> Result<String, CoinbaseError> {
    let limit = query.limit.map(|n| n.to_string());
    let offset = query.offset.map(|n| n.to_string());
    let level = query.level.map(|n| n.to_string());
    let result_limit = query.result_limit.map(|n| n.to_string());
    let result_offset = query.result_offset.map(|n| n.to_string());
    let fields = [
        ("offset", offset.as_deref()),
        ("level", level.as_deref()),
        ("granularity", query.granularity.as_deref()),
        ("start", query.start.as_deref()),
        ("end", query.end.as_deref()),
        ("currency", query.currency.as_deref()),
        ("date", query.date.as_deref()),
        ("cursor", query.cursor.as_deref()),
        ("limit", limit.as_deref()),
        ("product_id", query.product_id.as_deref()),
        ("account_id", query.account_id.as_deref()),
        ("portfolio_id", query.portfolio_id.as_deref()),
        ("profile_id", query.profile_id.as_deref()),
        ("status", query.status.as_deref()),
        ("start_time", query.start_time.as_deref()),
        ("end_time", query.end_time.as_deref()),
        ("starting_after", query.starting_after.as_deref()),
        ("ending_before", query.ending_before.as_deref()),
        ("order_id", query.order_id.as_deref()),
        ("after", query.after.as_deref()),
        ("before", query.before.as_deref()),
        ("product_type", query.product_type.as_deref()),
        ("product_venue", query.product_venue.as_deref()),
        ("transfer_type", query.transfer_type.as_deref()),
        ("time_from", query.time_from.as_deref()),
        ("time_to", query.time_to.as_deref()),
        ("result_limit", result_limit.as_deref()),
        ("result_offset", result_offset.as_deref()),
        ("portfolios", query.portfolios.as_deref()),
        ("instrument", query.instrument.as_deref()),
        ("client_order_id", query.client_order_id.as_deref()),
        ("ref_datetime", query.ref_datetime.as_deref()),
    ];
    if query.limit.is_some_and(|n| n == 0 || n > 1000) {
        return Err(CoinbaseError::InvalidRequest);
    }
    if query.result_limit.is_some_and(|n| n == 0 || n > 1000)
        || query.result_offset.is_some_and(|n| n > 1_000_000)
    {
        return Err(CoinbaseError::InvalidRequest);
    }
    let mut encoded = Vec::new();
    for (field, value) in fields {
        if let Some(value) = value {
            let Some((_, target)) = allowed.iter().find(|(name, _)| *name == field) else {
                return Err(CoinbaseError::InvalidRequest);
            };
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(CoinbaseError::InvalidRequest);
            }
            encoded.push(format!("{}={}", segment(target), segment(value)));
        }
    }
    if !query.order_ids.is_empty() {
        let Some((_, target)) = allowed.iter().find(|(name, _)| *name == "order_ids") else {
            return Err(CoinbaseError::InvalidRequest);
        };
        if query.order_ids.len() > 100
            || query
                .order_ids
                .iter()
                .any(|v| v.is_empty() || v.len() > 128 || v.chars().any(char::is_control))
        {
            return Err(CoinbaseError::InvalidRequest);
        }
        for value in &query.order_ids {
            encoded.push(format!("{}={}", segment(target), segment(value)));
        }
    }
    if !query.product_ids.is_empty() {
        let Some((_, target)) = allowed.iter().find(|(name, _)| *name == "product_ids") else {
            return Err(CoinbaseError::InvalidRequest);
        };
        if query.product_ids.len() > 100
            || query
                .product_ids
                .iter()
                .any(|v| v.is_empty() || v.len() > 128 || v.chars().any(char::is_control))
        {
            return Err(CoinbaseError::InvalidRequest);
        }
        for value in &query.product_ids {
            encoded.push(format!("{}={}", segment(target), segment(value)));
        }
    }
    if encoded.is_empty() {
        Ok(base.to_owned())
    } else {
        Ok(format!("{base}?{}", encoded.join("&")))
    }
}

/// Credential variants keep algorithm selection explicit and material redacted.
#[derive(Zeroize, ZeroizeOnDrop)]
pub enum CoinbaseAuth {
    CdpEd25519 {
        api_key: String,
        private_key_base64: String,
    },
    CdpEcdsa {
        api_key: String,
        private_key_pem: String,
    },
    ExchangeHmac {
        api_key: String,
        secret: String,
        passphrase: String,
    },
    InternationalHmac {
        api_key: String,
        secret: String,
        passphrase: String,
    },
}
impl fmt::Debug for CoinbaseAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CoinbaseAuth(<redacted>)")
    }
}

pub struct CoinbaseRequest {
    pub body: Option<String>,
}
impl CoinbaseRequest {
    pub fn get() -> Self {
        Self { body: None }
    }
    pub fn json(exact_body: String) -> Self {
        Self {
            body: Some(exact_body),
        }
    }
}
#[derive(Default)]
pub struct CoinbaseResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub pagination: CoinbasePagination,
}
/// Only cursor headers needed to finish Exchange paging are retained. All
/// authentication, request, and unrelated response headers are discarded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoinbasePagination {
    pub after: Option<String>,
    pub before: Option<String>,
}
impl CoinbaseResponse {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            pagination: CoinbasePagination::default(),
        }
    }
}
impl fmt::Debug for CoinbaseResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoinbaseResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("after_cursor_present", &self.pagination.after.is_some())
            .field("before_cursor_present", &self.pagination.before.is_some())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoinbaseError {
    #[error("Coinbase request rejected before signing")]
    GuardRejected,
    #[error("Coinbase credential format is invalid")]
    InvalidCredential,
    #[error("Coinbase request route or query is invalid")]
    InvalidRequest,
    #[error("Coinbase request could not be completed")]
    Transport,
}

pub struct CoinbaseTransport {
    client: reqwest::Client,
    auth: Arc<CoinbaseAuth>,
    signer: ccxt_request::Client,
    #[cfg(feature = "test-support")]
    base: Option<String>,
}
#[async_trait::async_trait]
pub trait CoinbaseApi: Send + Sync {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError>;
}
pub type DispatchGuard = Arc<dyn Fn() -> bool + Send + Sync>;
impl CoinbaseTransport {
    pub fn new(auth: CoinbaseAuth) -> Result<Self, CoinbaseError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|_| CoinbaseError::Transport)?;
        let auth = Arc::new(auth);
        let signer = ccxt_request::Client::new(auth.clone())?;
        Ok(Self {
            client,
            auth,
            signer,
            #[cfg(feature = "test-support")]
            base: None,
        })
    }
    /// Guard runs immediately before credential-dependent signing and dispatch.
    async fn execute(
        &self,
        route: CoinbaseRoute,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        let method = route.method();
        route.path()?;
        if matches!(method, "POST" | "PUT" | "PATCH") && request.body.is_none() {
            return Err(CoinbaseError::InvalidRequest);
        }
        let body = request.body.unwrap_or_default();
        let host_matches = matches!(
            (self.auth.as_ref(), route.host()),
            (CoinbaseAuth::CdpEd25519 { .. }, Host::App)
                | (CoinbaseAuth::CdpEcdsa { .. }, Host::App)
                | (CoinbaseAuth::ExchangeHmac { .. }, Host::Exchange)
                | (CoinbaseAuth::InternationalHmac { .. }, Host::International)
        );
        if !host_matches {
            return Err(CoinbaseError::InvalidCredential);
        }
        let wire = self
            .signer
            .request(route.clone(), body, guard.clone())
            .await?;
        let dispatch_url = wire.url.clone();
        #[cfg(feature = "test-support")]
        let dispatch_url = if let Some(base) = &self.base {
            let mut url = reqwest::Url::parse(base).map_err(|_| CoinbaseError::InvalidRequest)?;
            let original =
                reqwest::Url::parse(&dispatch_url).map_err(|_| CoinbaseError::InvalidRequest)?;
            url.set_path(original.path());
            url.set_query(original.query());
            url.to_string()
        } else {
            dispatch_url
        };
        let mut builder = self
            .client
            .request(
                method.parse().map_err(|_| CoinbaseError::Transport)?,
                dispatch_url,
            )
            .headers(wire.headers);
        if !wire.body.is_empty() {
            builder = builder
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(wire.body);
        }
        // Signing can take long enough for the approval window to expire.
        // Recheck after authentication headers are built and immediately before
        // the request is handed to the HTTP client.
        if route.mutating() && !guard.as_ref().is_some_and(|g| g()) {
            return Err(CoinbaseError::GuardRejected);
        }
        let response = builder.send().await.map_err(|_| CoinbaseError::Transport)?;
        let status = response.status().as_u16();
        let pagination = capture_pagination(response.headers())?;
        let mut body_bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| CoinbaseError::Transport)?
        {
            if body_bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(CoinbaseError::Transport);
            }
            body_bytes.extend_from_slice(&chunk);
        }
        Ok(CoinbaseResponse {
            status,
            body: body_bytes,
            pagination,
        })
    }
    #[cfg(feature = "test-support")]
    pub fn loopback(auth: CoinbaseAuth, base: String) -> Result<Self, CoinbaseError> {
        let url = reqwest::Url::parse(&base).map_err(|_| CoinbaseError::InvalidRequest)?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(CoinbaseError::InvalidRequest);
        }
        let mut transport = Self::new(auth)?;
        transport.base = Some(base);
        Ok(transport)
    }
    #[cfg(test)]
    fn headers(
        &self,
        route: &CoinbaseRoute,
        method: &str,
        path: &str,
        body: &str,
        now: u64,
    ) -> Result<Vec<(&'static str, Zeroizing<String>)>, CoinbaseError> {
        auth_headers(&self.auth, route, method, path, body, now)
    }
}
fn auth_headers(
    auth: &CoinbaseAuth,
    route: &CoinbaseRoute,
    method: &str,
    path: &str,
    body: &str,
    now: u64,
) -> Result<Vec<(&'static str, Zeroizing<String>)>, CoinbaseError> {
    match auth {
        CoinbaseAuth::CdpEd25519 {
            api_key,
            private_key_base64,
        } => {
            let token = ed25519_jwt(
                api_key,
                private_key_base64,
                method,
                route.authority(),
                path,
                now,
            )?;
            Ok(vec![(
                "authorization",
                Zeroizing::new(format!("Bearer {}", *token)),
            )])
        }
        CoinbaseAuth::CdpEcdsa {
            api_key,
            private_key_pem,
        } => {
            let token = ecdsa_jwt(
                api_key,
                private_key_pem,
                method,
                route.authority(),
                path,
                now,
            )?;
            Ok(vec![(
                "authorization",
                Zeroizing::new(format!("Bearer {}", *token)),
            )])
        }
        CoinbaseAuth::ExchangeHmac {
            api_key,
            secret,
            passphrase,
        }
        | CoinbaseAuth::InternationalHmac {
            api_key,
            secret,
            passphrase,
        } => {
            let timestamp = now.to_string();
            let message = Zeroizing::new(format!("{timestamp}{method}{path}{body}"));
            let key = Zeroizing::new(
                STANDARD
                    .decode(secret)
                    .map_err(|_| CoinbaseError::InvalidCredential)?,
            );
            let mut mac =
                HmacSha256::new_from_slice(&key).map_err(|_| CoinbaseError::InvalidCredential)?;
            mac.update(message.as_bytes());
            let signature = Zeroizing::new(STANDARD.encode(mac.finalize().into_bytes()));
            let out = vec![
                ("cb-access-key", Zeroizing::new(api_key.clone())),
                ("cb-access-sign", signature),
                ("cb-access-timestamp", Zeroizing::new(timestamp)),
                ("cb-access-passphrase", Zeroizing::new(passphrase.clone())),
            ];
            Ok(out)
        }
    }
}
#[async_trait::async_trait]
impl CoinbaseApi for CoinbaseTransport {
    async fn request(
        &self,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<DispatchGuard>,
    ) -> Result<CoinbaseResponse, CoinbaseError> {
        self.execute(endpoint, request, guard).await
    }
}

#[derive(Serialize)]
struct CdpClaims<'a> {
    iss: &'static str,
    sub: &'a str,
    nbf: u64,
    exp: u64,
    uri: String,
}
fn token_header(alg: &str, kid: &str, nonce: &str) -> Result<Zeroizing<String>, CoinbaseError> {
    serde_json::to_string(&serde_json::json!({"alg":alg,"kid":kid,"nonce":nonce,"typ":"JWT"}))
        .map_err(|_| CoinbaseError::InvalidCredential)
        .map(|s| Zeroizing::new(URL_SAFE_NO_PAD.encode(s)))
}
fn token_payload<T: Serialize>(claims: &T) -> Result<Zeroizing<String>, CoinbaseError> {
    serde_json::to_vec(claims)
        .map_err(|_| CoinbaseError::InvalidCredential)
        .map(|s| Zeroizing::new(URL_SAFE_NO_PAD.encode(s)))
}
fn jwt_header(alg: &str, kid: &str) -> Result<Zeroizing<String>, CoinbaseError> {
    use rand::RngCore;
    let mut nonce_bytes = Zeroizing::new([0u8; 16]);
    rand::thread_rng().fill_bytes(&mut *nonce_bytes);
    let nonce = Zeroizing::new(URL_SAFE_NO_PAD.encode(*nonce_bytes));
    token_header(alg, kid, &nonce)
}
fn ed25519_jwt(
    kid: &str,
    key: &str,
    method: &str,
    host: &str,
    path: &str,
    now: u64,
) -> Result<Zeroizing<String>, CoinbaseError> {
    let raw = Zeroizing::new(
        STANDARD
            .decode(key)
            .map_err(|_| CoinbaseError::InvalidCredential)?,
    );
    if raw.len() != 64 {
        return Err(CoinbaseError::InvalidCredential);
    }
    let signing = EdSigningKey::from_bytes(
        raw[..32]
            .try_into()
            .map_err(|_| CoinbaseError::InvalidCredential)?,
    );
    if signing.verifying_key().as_bytes() != &raw[32..] {
        return Err(CoinbaseError::InvalidCredential);
    }
    let h = jwt_header("EdDSA", kid)?;
    let uri = format!("{method} {host}{}", path.split('?').next().unwrap_or(path));
    let claims = CdpClaims {
        iss: "cdp",
        sub: kid,
        nbf: now,
        exp: now.saturating_add(120),
        uri,
    };
    let p = token_payload(&claims)?;
    let input = Zeroizing::new(format!("{}.{}", *h, *p));
    Ok(Zeroizing::new(format!(
        "{}.{}",
        *input,
        URL_SAFE_NO_PAD.encode(signing.sign(input.as_bytes()).to_bytes())
    )))
}
fn ecdsa_jwt(
    kid: &str,
    pem: &str,
    method: &str,
    host: &str,
    path: &str,
    now: u64,
) -> Result<Zeroizing<String>, CoinbaseError> {
    use p256::pkcs8::DecodePrivateKey;
    let signing = SigningKey::from_pkcs8_pem(pem)
        .or_else(|_| p256::SecretKey::from_sec1_pem(pem).map(SigningKey::from))
        .map_err(|_| CoinbaseError::InvalidCredential)?;
    let h = jwt_header("ES256", kid)?;
    let uri = format!("{method} {host}{}", path.split('?').next().unwrap_or(path));
    let claims = CdpClaims {
        iss: "cdp",
        sub: kid,
        nbf: now,
        exp: now.saturating_add(120),
        uri,
    };
    let p = token_payload(&claims)?;
    let input = Zeroizing::new(format!("{}.{}", *h, *p));
    let sig: Signature = signing.sign(input.as_bytes());
    Ok(Zeroizing::new(format!(
        "{}.{}",
        *input,
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature as EdSignature, Verifier as EdVerifier};
    use p256::ecdsa::SigningKey as EcSigningKey;

    fn jwt_parts(token: &str) -> (String, serde_json::Value, String) {
        let mut pieces = token.split('.');
        let header = URL_SAFE_NO_PAD
            .decode(pieces.next().unwrap_or_default())
            .unwrap_or_default();
        let payload = URL_SAFE_NO_PAD
            .decode(pieces.next().unwrap_or_default())
            .unwrap_or_default();
        let sig = pieces.next().unwrap_or_default().to_owned();
        let header: String = serde_json::from_slice(&header)
            .map(|v: serde_json::Value| v.to_string())
            .unwrap_or_default();
        let payload = serde_json::from_slice(&payload).unwrap_or(serde_json::Value::Null);
        (header, payload, sig)
    }

    #[test]
    fn ed25519_cdp_jwt_is_independently_verifiable_and_binds_exact_request() {
        let seed = [0x51; 32];
        let key = EdSigningKey::from_bytes(&seed);
        let mut bytes = seed.to_vec();
        bytes.extend_from_slice(key.verifying_key().as_bytes());
        let secret = STANDARD.encode(bytes);
        let token = ed25519_jwt(
            "organizations/org/apiKeys/id",
            &secret,
            "GET",
            "api.coinbase.com",
            "/api/v3/brokerage/accounts/550e8400-e29b-41d4-a716-446655440000?limit=20",
            1_800_000_000,
        )
        .unwrap_or_default();
        let (header, claims, sig) = jwt_parts(&token);
        assert!(header.contains("EdDSA"));
        assert!(claims["iss"] == "cdp");
        assert!(
            claims["uri"]
                == "GET api.coinbase.com/api/v3/brokerage/accounts/550e8400-e29b-41d4-a716-446655440000"
        );
        assert!(claims["nbf"] == 1_800_000_000);
        assert!(claims["exp"] == 1_800_000_120);
        let signing = EdSigningKey::from_bytes(&seed);
        let input = token
            .rsplit_once('.')
            .map(|(prefix, _)| prefix)
            .unwrap_or_default();
        let signature = URL_SAFE_NO_PAD.decode(sig).unwrap_or_default();
        let signature = EdSignature::from_slice(&signature)
            .unwrap_or_else(|_| EdSignature::from_bytes(&[0; 64]));
        assert!(
            signing
                .verifying_key()
                .verify(input.as_bytes(), &signature)
                .is_ok()
        );
    }

    #[test]
    fn ecdsa_es256_jwt_is_independently_verifiable() {
        use p256::pkcs8::EncodePrivateKey;
        let signing = EcSigningKey::from_bytes((&[0x42; 32]).into()).expect("fixed P-256 scalar");
        let pem = signing
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .expect("PEM encoding");
        let token = ecdsa_jwt(
            "organizations/org/apiKeys/id",
            &pem,
            "POST",
            "api.coinbase.com",
            "/api/v3/brokerage/orders/preview",
            1_800_000_000,
        )
        .unwrap_or_default();
        let (header, claims, sig) = jwt_parts(&token);
        assert!(header.contains("ES256"));
        assert!(claims["uri"] == "POST api.coinbase.com/api/v3/brokerage/orders/preview");
        let signature_bytes = URL_SAFE_NO_PAD.decode(sig).unwrap_or_default();
        let signature = Signature::from_slice(&signature_bytes).expect("raw ES256 signature");
        let input = token
            .rsplit_once('.')
            .map(|(prefix, _)| prefix)
            .unwrap_or_default();
        signing
            .verifying_key()
            .verify(input.as_bytes(), &signature)
            .expect("independent ES256 verification");
    }

    #[test]
    fn endpoint_routes_and_post_read_classification_are_exact() {
        assert_eq!(
            CoinbaseRoute::LegacyCurrencies.path().unwrap_or_default(),
            "/v2/currencies"
        );
        assert_eq!(
            CoinbaseRoute::LegacyCryptoCurrencies
                .path()
                .unwrap_or_default(),
            "/v2/currencies/crypto"
        );
        assert_eq!(CoinbaseRoute::LegacyCryptoCurrencies.method(), "GET");
        assert_eq!(
            CoinbaseRoute::Account {
                account_id: "550e8400-e29b-41d4-a716-446655440000".into()
            }
            .path()
            .unwrap(),
            "/api/v3/brokerage/accounts/550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            CoinbaseRoute::Product {
                product_id: "BTC-USD_spot.~".into()
            }
            .path()
            .unwrap(),
            "/api/v3/brokerage/market/products/BTC-USD_spot.~"
        );
        for account_id in [".", ".."] {
            assert!(matches!(
                CoinbaseRoute::Account {
                    account_id: account_id.into()
                }
                .path(),
                Err(CoinbaseError::InvalidRequest)
            ));
        }
        assert_eq!(
            CoinbaseRoute::Order {
                order_id: "order-1".into()
            }
            .method(),
            "GET"
        );
        assert_eq!(
            CoinbaseRoute::Order {
                order_id: "order-1".into()
            }
            .path()
            .unwrap_or_default(),
            "/api/v3/brokerage/orders/historical/order-1"
        );
        assert_eq!(
            CoinbaseRoute::ExchangeCancelOrder {
                order_id: "id".into()
            }
            .method(),
            "DELETE"
        );
        assert_eq!(
            CoinbaseRoute::ExchangeDepositAddressCreate {
                coinbase_account_id: "account".into()
            }
            .method(),
            "POST"
        );
        assert!(!CoinbaseRoute::OrderPreview.mutating());
        assert!(!CoinbaseRoute::OrderEditPreview.mutating());
        assert!(!CoinbaseRoute::ConvertQuote.mutating());
        assert!(CoinbaseRoute::Orders.mutating());
        assert_eq!(
            CoinbaseRoute::HistoricalOrders {
                query: CoinbaseQuery {
                    product_ids: vec!["BTC-USD".into(), "ETH-USD".into()],
                    ..Default::default()
                }
            }
            .path()
            .unwrap_or_default(),
            "/api/v3/brokerage/orders/historical/batch?product_ids=BTC-USD&product_ids=ETH-USD"
        );
        let bad = CoinbaseRoute::Transactions {
            account_id: "id".into(),
            query: CoinbaseQuery {
                product_id: Some("BTC-USD".into()),
                ..Default::default()
            },
        };
        assert!(bad.path().is_err());
    }

    #[test]
    fn response_pagination_capture_whitelists_only_bounded_cursor_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "cb-after",
            reqwest::header::HeaderValue::from_static("after-secret-like-token"),
        );
        headers.insert(
            "cb-before",
            reqwest::header::HeaderValue::from_static("before-cursor"),
        );
        headers.insert(
            "cb-access-key",
            reqwest::header::HeaderValue::from_static("must-not-capture"),
        );
        let pagination = capture_pagination(&headers).expect("well-formed cursors");
        assert_eq!(pagination.after.as_deref(), Some("after-secret-like-token"));
        assert_eq!(pagination.before.as_deref(), Some("before-cursor"));
        let response = CoinbaseResponse {
            status: 200,
            body: b"{}".to_vec(),
            pagination,
        };
        let debug = format!("{response:?}");
        assert!(!debug.contains("after-secret-like-token"));
        assert!(!debug.contains("before-cursor"));
        assert!(!debug.contains("must-not-capture"));

        headers.insert(
            "cb-after",
            reqwest::header::HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(matches!(
            capture_pagination(&headers),
            Err(CoinbaseError::Transport)
        ));
    }

    #[test]
    fn institutional_order_and_fill_reads_are_distinct_from_mutations() {
        let exchange_orders = CoinbaseRoute::ExchangeOrdersList {
            query: CoinbaseQuery {
                product_id: Some("BTC-USD".into()),
                status: Some("open".into()),
                after: Some("opaque-cursor".into()),
                limit: Some(50),
                ..Default::default()
            },
        };
        assert_eq!(exchange_orders.method(), "GET");
        assert!(!exchange_orders.mutating());
        let path = exchange_orders.path().expect("typed Exchange query");
        assert!(path.starts_with("/orders?"));
        assert!(path.contains("product_id=BTC-USD"));
        assert!(path.contains("after=opaque-cursor"));
        assert!(path.contains("status=open"));

        let exchange_fills = CoinbaseRoute::ExchangeFillsFiltered {
            query: CoinbaseQuery {
                order_id: Some("order-id".into()),
                limit: Some(100),
                ..Default::default()
            },
        };
        assert_eq!(exchange_fills.method(), "GET");
        assert!(!exchange_fills.mutating());
        assert!(
            exchange_fills
                .path()
                .expect("fills query")
                .contains("order_id=order-id")
        );

        let intl_fills = CoinbaseRoute::InternationalPortfolioFills {
            portfolio_id: "portfolio/one".into(),
        };
        assert_eq!(intl_fills.method(), "GET");
        assert!(!intl_fills.mutating());
        assert_eq!(
            intl_fills.path().expect("encoded portfolio"),
            "/api/v1/portfolios/portfolio%2Fone/fills"
        );

        assert_eq!(CoinbaseRoute::ExchangeOrders.method(), "POST");
        assert!(CoinbaseRoute::ExchangeOrders.mutating());
        assert_eq!(
            CoinbaseRoute::ExchangeOrderByClientId {
                client_oid: "client-1".into()
            }
            .path()
            .unwrap(),
            "/orders/client:client-1"
        );
        let cancel = CoinbaseRoute::ExchangeCancelOrderScoped {
            order_id: "o-1".into(),
            product_id: "BTC-USD".into(),
            profile_id: "profile-1".into(),
        };
        assert_eq!(cancel.method(), "DELETE");
        assert!(cancel.mutating());
        assert_eq!(
            cancel.path().expect("scoped cancel"),
            "/orders/o-1?product_id=BTC-USD&profile_id=profile-1"
        );
        assert_eq!(CoinbaseRoute::InternationalOrders.method(), "POST");
        assert!(CoinbaseRoute::InternationalOrders.mutating());
        assert_eq!(CoinbaseRoute::PortfolioMoveFunds.method(), "POST");
        assert!(CoinbaseRoute::PortfolioMoveFunds.mutating());
        assert_eq!(CoinbaseRoute::PortfolioCreate.method(), "POST");
        assert!(CoinbaseRoute::PortfolioCreate.mutating());
        assert_eq!(
            CoinbaseRoute::PortfolioCreate.path().unwrap(),
            "/api/v3/brokerage/portfolios"
        );
        let portfolio_detail = CoinbaseRoute::PortfolioDetail {
            portfolio_id: "portfolio-1".into(),
        };
        assert_eq!(portfolio_detail.method(), "GET");
        assert!(!portfolio_detail.mutating());
        assert_eq!(
            portfolio_detail.path().unwrap(),
            "/api/v3/brokerage/portfolios/portfolio-1"
        );
        let portfolio_update = CoinbaseRoute::PortfolioUpdate {
            portfolio_id: "portfolio-1".into(),
        };
        assert_eq!(portfolio_update.method(), "PUT");
        assert!(portfolio_update.mutating());
        assert_eq!(
            portfolio_update.path().unwrap(),
            "/api/v3/brokerage/portfolios/portfolio-1"
        );
        let portfolio_delete = CoinbaseRoute::PortfolioDelete {
            portfolio_id: "portfolio-1".into(),
        };
        assert_eq!(portfolio_delete.method(), "DELETE");
        assert!(portfolio_delete.mutating());
        assert_eq!(
            portfolio_delete.path().unwrap(),
            "/api/v3/brokerage/portfolios/portfolio-1"
        );
        for (route, method, path) in [
            (
                CoinbaseRoute::CfmSweepSchedule,
                "POST",
                "/api/v3/brokerage/cfm/sweeps/schedule",
            ),
            (
                CoinbaseRoute::CfmMarginSetting,
                "POST",
                "/api/v3/brokerage/cfm/intraday/margin_setting",
            ),
            (
                CoinbaseRoute::CfmCancelSweeps,
                "DELETE",
                "/api/v3/brokerage/cfm/sweeps",
            ),
        ] {
            assert_eq!(route.method(), method);
            assert!(route.mutating());
            assert_eq!(route.path().unwrap(), path);
        }
        assert_eq!(
            CoinbaseRoute::PortfolioMoveFunds.path().unwrap(),
            "/api/v3/brokerage/portfolios/move_funds"
        );
        assert_eq!(
            CoinbaseRoute::InternationalPortfolioTransfer.method(),
            "POST"
        );
        assert!(CoinbaseRoute::InternationalPortfolioTransfer.mutating());
        assert_eq!(
            CoinbaseRoute::InternationalPortfolioTransfer
                .path()
                .unwrap(),
            "/api/v1/portfolios/transfer"
        );
    }

    #[test]
    fn international_transfer_history_and_balances_are_typed_reads() {
        let history = CoinbaseRoute::InternationalTransfers {
            query: CoinbaseQuery {
                transfer_type: Some("INTERNAL".into()),
                time_from: Some("2026-01-01T00:00:00Z".into()),
                result_limit: Some(100),
                portfolios: Some("portfolio-a,portfolio-b".into()),
                ..Default::default()
            },
        };
        assert_eq!(history.method(), "GET");
        assert!(!history.mutating());
        let path = history.path().expect("closed history query");
        assert!(path.contains("type=INTERNAL"));
        assert!(path.contains("result_limit=100"));
        assert!(path.contains("portfolios=portfolio-a%2Cportfolio-b"));
        let invalid = CoinbaseRoute::InternationalTransfers {
            query: CoinbaseQuery {
                product_id: Some("BTC-USD".into()),
                ..Default::default()
            },
        };
        assert!(invalid.path().is_err());
        assert_eq!(
            CoinbaseRoute::InternationalTransfer {
                transfer_uuid: "uuid/1".into()
            }
            .path()
            .unwrap(),
            "/api/v1/transfers/uuid%2F1"
        );
        assert_eq!(
            CoinbaseRoute::InternationalPortfolioDetail {
                portfolio_id: "portfolio/one".into()
            }
            .path()
            .unwrap(),
            "/api/v1/portfolios/portfolio%2Fone/detail"
        );
    }

    #[test]
    fn international_order_and_fill_reads_bind_portfolio_and_typed_filters() {
        let order = CoinbaseRoute::InternationalOrderScoped {
            order_id: "order/1".into(),
            portfolio_id: "portfolio-1".into(),
        };
        assert_eq!(order.method(), "GET");
        assert_eq!(
            order.path().unwrap(),
            "/api/v1/orders/order%2F1?portfolio=portfolio-1"
        );
        let cancel = CoinbaseRoute::InternationalCancelOrderScoped {
            order_id: "order-1".into(),
            portfolio_id: "portfolio-1".into(),
        };
        assert_eq!(cancel.method(), "DELETE");
        assert!(cancel.mutating());
        assert!(cancel.path().unwrap().ends_with("?portfolio=portfolio-1"));
        let fills = CoinbaseRoute::InternationalPortfolioFillsFiltered {
            portfolio_id: "portfolio-1".into(),
            query: CoinbaseQuery {
                order_id: Some("order-1".into()),
                client_order_id: Some("00000000-0000-4000-8000-000000000001".into()),
                result_limit: Some(100),
                result_offset: Some(200),
                ..Default::default()
            },
        };
        assert_eq!(
            fills.path().unwrap(),
            "/api/v1/portfolios/portfolio-1/fills?order_id=order-1&result_limit=100&result_offset=200&client_order_id=00000000-0000-4000-8000-000000000001"
        );
        let orders = CoinbaseRoute::InternationalOrdersFiltered {
            query: CoinbaseQuery {
                portfolio_id: Some("portfolio-1".into()),
                instrument: Some("BTC-USDC".into()),
                client_order_id: Some("00000000-0000-4000-8000-000000000001".into()),
                ..Default::default()
            },
        };
        assert_eq!(
            orders.path().unwrap(),
            "/api/v1/orders?portfolio=portfolio-1&instrument=BTC-USDC&client_order_id=00000000-0000-4000-8000-000000000001"
        );
        let invalid = CoinbaseRoute::InternationalOrdersFiltered {
            query: CoinbaseQuery {
                account_id: Some("caller-controlled".into()),
                ..Default::default()
            },
        };
        assert!(matches!(invalid.path(), Err(CoinbaseError::InvalidRequest)));
        let too_many = CoinbaseRoute::InternationalOrdersFiltered {
            query: CoinbaseQuery {
                result_limit: Some(101),
                ..Default::default()
            },
        };
        assert!(matches!(
            too_many.path(),
            Err(CoinbaseError::InvalidRequest)
        ));
    }

    #[test]
    fn exchange_hmac_binds_method_query_and_exact_body() {
        let key = [0x33; 32];
        let secret = STANDARD.encode(key);
        let auth = CoinbaseAuth::ExchangeHmac {
            api_key: "key-id".into(),
            secret,
            passphrase: "pass".into(),
        };
        let transport = CoinbaseTransport::new(auth).expect("HTTP client");
        let route = CoinbaseRoute::ExchangeOrders;
        let body = "{\"size\":\"1.25\"}";
        let headers = transport
            .headers(&route, "POST", "/orders?product_id=BTC-USD", body, 123)
            .expect("HMAC headers");
        let signature = headers
            .iter()
            .find(|(name, _)| *name == "cb-access-sign")
            .map(|(_, value)| value.as_str())
            .unwrap_or_default();
        let mut verifier = HmacSha256::new_from_slice(&key).expect("HMAC key");
        verifier.update(format!("123POST/orders?product_id=BTC-USD{body}").as_bytes());
        assert_eq!(signature, STANDARD.encode(verifier.finalize().into_bytes()));
    }

    #[tokio::test]
    async fn rejected_dispatch_guard_stops_before_network_and_auth_debug_is_redacted() {
        let auth = CoinbaseAuth::CdpEd25519 {
            api_key: "private-id-marker".into(),
            private_key_base64: "private-key-marker".into(),
        };
        let display = format!("{auth:?}");
        assert!(!display.contains("private-id-marker"));
        assert!(!display.contains("private-key-marker"));
        let client = CoinbaseTransport::new(auth).expect("HTTP client");
        let result = client
            .request(
                CoinbaseRoute::OrderCancel,
                CoinbaseRequest::json("{}".into()),
                None,
            )
            .await;
        assert!(matches!(result, Err(CoinbaseError::GuardRejected)));
    }

    #[tokio::test]
    async fn revoked_during_signing_is_rejected_before_http_send() {
        let seed = [0x29; 32];
        let key = EdSigningKey::from_bytes(&seed);
        let mut bytes = seed.to_vec();
        bytes.extend_from_slice(key.verifying_key().as_bytes());
        let auth = CoinbaseAuth::CdpEd25519 {
            api_key: "organizations/org/apiKeys/test".into(),
            private_key_base64: STANDARD.encode(bytes),
        };
        let client = CoinbaseTransport::new(auth).expect("HTTP client");
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let guard: DispatchGuard = {
            let checks = checks.clone();
            Arc::new(move || checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0)
        };
        let result = client
            .request(
                CoinbaseRoute::Orders,
                CoinbaseRequest::json("{}".into()),
                Some(guard),
            )
            .await;
        assert!(matches!(result, Err(CoinbaseError::GuardRejected)));
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
