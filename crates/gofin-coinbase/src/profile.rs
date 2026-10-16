use crate::contracts::{self, Product as ContractProduct};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use gofin_domain::{
    AccountingLocation, Amount, Check, CheckStatus, ConnectionMetadata, CostBound, CostTreatment,
    Definition, EffectConstraint, FinancialEffect, FinancialIntent, FinancialState, GoFinError,
    OperationRecord, PreparationContext, PreparedOperation, Quantity, Reconciliation,
    ResourceAmount, Result, SettlementProgress, SnapshotScope, StateSnapshot, Status,
    VerifiedEffects,
};
use gofin_exchange::{
    ExchangeProfile, PlannedSubmission,
    coinbase::{
        CoinbaseApi, CoinbaseEndpoint, CoinbaseError, CoinbaseQuery, CoinbaseRequest,
        CoinbaseResponse, CoinbaseRoute,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[path = "withdrawal.rs"]
mod withdrawal;

pub struct CoinbaseDispatch {
    pub product: CoinbaseProduct,
    pub endpoint: CoinbaseEndpoint,
    pub account: String,
    pub exact_body: String,
    pub submission: gofin_domain::Submission,
    pub candidate: PreparedOperation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoinbaseProduct {
    Advanced,
    Exchange,
    International,
}
impl CoinbaseProduct {
    pub fn provider(self) -> &'static str {
        match self {
            Self::Advanced => "coinbase",
            Self::Exchange => "coinbaseexchange",
            Self::International => "coinbaseinternational",
        }
    }
}

/// Nonsecret product and account identity. Authentication stays inside the
/// injected API transport created by the protected broker.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoinbaseAccount {
    pub name: String,
    pub resource: gofin_domain::ResourceId,
    pub credential_ref: String,
    pub physical_identity: String,
    pub identity_verified: bool,
    #[serde(default)]
    pub portfolio_id: Option<String>,
    pub product: CoinbaseProduct,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CoinbaseOrderBounds {
    #[serde(default)]
    maximum_spend: Option<String>,
    #[serde(default)]
    maximum_quantity: Option<String>,
    #[serde(default)]
    maximum_price: Option<String>,
    #[serde(default)]
    minimum_price: Option<String>,
    maximum_fees: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct CoinbaseProfile {
    accounts: Vec<CoinbaseAccount>,
    transports: BTreeMap<String, Arc<dyn CoinbaseApi>>,
    pub(super) prices: Option<Arc<dyn gofin_pricing::UsdPriceSource>>,
    recipient_verifier: Option<Arc<dyn crate::funding::CoinbaseRecipientVerifier>>,
    execution_state:
        Arc<std::sync::RwLock<Option<Arc<dyn gofin_domain::execution_state::ExecutionState>>>>,
}
impl CoinbaseProfile {
    pub(crate) fn journal_amendment_children(
        &self,
        parent_operation_id: &str,
        operation: &str,
    ) -> Result<Vec<OperationRecord>> {
        let state = self
            .execution_state
            .read()
            .map_err(|_| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Protected execution history cannot be read.",
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "This operation requires the protected Fin journal.",
                )
            })?;
        state.amendment_children(parent_operation_id, operation)
    }

    pub fn new(
        accounts: Vec<CoinbaseAccount>,
        transports: BTreeMap<String, Arc<dyn CoinbaseApi>>,
    ) -> Result<Self> {
        let mut names = BTreeSet::new();
        let mut resources = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for account in &accounts {
            let identity = canonical_identity(&account.physical_identity);
            if account.name.trim().is_empty()
                || account.credential_ref.trim().is_empty()
                || identity.is_empty()
                || !names.insert(account.name.clone())
                || !resources.insert(account.resource.clone())
                || !identities.insert((account.product, identity))
                || !transports.contains_key(&account.credential_ref)
                || (account.product == CoinbaseProduct::Advanced
                    && account.portfolio_id.as_deref().unwrap_or("").is_empty())
            {
                return Err(failure(
                    "COINBASE_ACCOUNT_INVALID",
                    "Coinbase accounts require distinct explicit product identities and a protected transport.",
                ));
            }
        }
        Ok(Self {
            accounts,
            transports,
            prices: None,
            recipient_verifier: None,
            execution_state: Arc::new(std::sync::RwLock::new(None)),
        })
    }
    pub fn with_prices(mut self, prices: Arc<dyn gofin_pricing::UsdPriceSource>) -> Self {
        self.prices = Some(prices);
        self
    }
    pub fn with_recipient_verifier(
        mut self,
        verifier: Arc<dyn crate::funding::CoinbaseRecipientVerifier>,
    ) -> Self {
        self.recipient_verifier = Some(verifier);
        self
    }
    pub fn accounts(&self) -> &[CoinbaseAccount] {
        &self.accounts
    }
    /// Read immutable local execution history for a specific prepared binding.
    /// Institutional cancellation uses this to prove linkage without exposing
    /// the journal handle or letting caller input choose a parent operation.
    pub(crate) fn operations_by_prepared_binding(
        &self,
        operation: &str,
        pointer: &str,
        value: &Value,
    ) -> Result<Vec<OperationRecord>> {
        let state = self
            .execution_state
            .read()
            .map_err(|_| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Protected execution history cannot be read.",
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "This action requires the protected Fin journal.",
                )
            })?;
        let pointer = if pointer.starts_with("/preview/binding") {
            pointer.to_owned()
        } else {
            format!("/preview/binding{pointer}")
        };
        state.operations_by_prepared_binding(operation, &pointer, value)
    }
    /// Reject reuse of a native client order ID once any submission with that
    /// identity has been durably written. Pre-dispatch records without a
    /// submission remain retryable.
    pub(crate) fn assert_unique_client_order_id(
        &self,
        product: CoinbaseProduct,
        client_order_id: &str,
        except_record_id: Option<&str>,
    ) -> Result<()> {
        let state = self
            .execution_state
            .read()
            .map_err(|_| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Protected execution history cannot be read.",
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Native order identity requires the protected Fin journal.",
                )
            })?;
        let bindings = match product {
            CoinbaseProduct::Advanced => {
                vec![("coinbase.create-order", "/preview/binding/clientOrderId")]
            }
            CoinbaseProduct::Exchange => vec![(
                "coinbaseexchange.create-order",
                "/preview/binding/clientOrderId",
            )],
            CoinbaseProduct::International => vec![
                (
                    "coinbaseinternational.create-order",
                    "/preview/binding/clientOrderId",
                ),
                (
                    "coinbaseinternational.edit-order",
                    "/preview/binding/successorClientOrderId",
                ),
            ],
        };
        for (operation, pointer) in bindings {
            for record in
                state.operations_by_prepared_binding(operation, pointer, &json!(client_order_id))?
            {
                if Some(record.id.as_str()) != except_record_id
                    && record.submission.is_some()
                    && !is_terminal_verified_no_dispatch(&record, product, client_order_id)
                {
                    return Err(failure(
                        "COINBASE_CLIENT_ORDER_ID_REUSED",
                        "This Coinbase client order ID already belongs to a durable submission. Reconcile that original operation instead of reusing its ID.",
                    ));
                }
            }
        }
        Ok(())
    }
    fn account(&self, name: &str, product: CoinbaseProduct) -> Result<&CoinbaseAccount> {
        self.accounts
            .iter()
            .find(|a| a.name == name && a.product == product)
            .ok_or_else(|| {
                failure(
                    "COINBASE_ACCOUNT_UNKNOWN",
                    "Select an enrolled account for this Coinbase product.",
                )
            })
    }
    fn configuration_binding_matches(&self, binding: &Value) -> bool {
        let Some(name) = binding["account"].as_str() else {
            return false;
        };
        let Some(account) = self.accounts.iter().find(|account| account.name == name) else {
            return false;
        };
        account.product == CoinbaseProduct::Advanced
            && account.identity_verified
            && serde_json::to_value(&account.resource).ok().as_ref() == binding.get("resource")
            && account.physical_identity == binding["physicalIdentity"].as_str().unwrap_or("")
            && account.portfolio_id.as_deref() == binding["portfolioId"].as_str()
    }
    /// Used by a broker-owned composite operation. The endpoint must remain a
    /// closed route and requests carry no caller-selected URL or headers.
    pub async fn request_for_account(
        &self,
        account_name: &str,
        product: CoinbaseProduct,
        endpoint: CoinbaseEndpoint,
        request: CoinbaseRequest,
        guard: Option<gofin_exchange::coinbase::DispatchGuard>,
    ) -> Result<CoinbaseResponse> {
        let account = self.account(account_name, product)?;
        let transport = self
            .transports
            .get(&account.credential_ref)
            .ok_or_else(|| {
                failure(
                    "COINBASE_CREDENTIAL_UNAVAILABLE",
                    "Protected Coinbase credentials are unavailable.",
                )
            })?;
        transport
            .request(endpoint, request, guard)
            .await
            .map_err(coinbase_request_error)
    }
    async fn verify_advanced_key(
        &self,
        account: &CoinbaseAccount,
        require_trade: bool,
        require_transfer: bool,
    ) -> Result<()> {
        let response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::KeyPermissions,
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_KEY_PERMISSIONS_UNAVAILABLE",
                "Advanced key permissions could not be verified.",
            ));
        }
        let permissions: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid key permission evidence.",
            )
        })?;
        let portfolio = permissions.get("portfolio_uuid").and_then(Value::as_str);
        if portfolio != account.portfolio_id.as_deref()
            || permissions.get("can_view").and_then(Value::as_bool) != Some(true)
            || (require_trade
                && permissions.get("can_trade").and_then(Value::as_bool) != Some(true))
            || (require_transfer
                && permissions.get("can_transfer").and_then(Value::as_bool) != Some(true))
        {
            return Err(failure(
                "COINBASE_KEY_PERMISSION_DENIED",
                "The live Advanced key permissions do not match the enrolled portfolio and required action.",
            ));
        }
        Ok(())
    }
    fn decode<'a>(
        &'a self,
        operation: &str,
        input: &Value,
    ) -> Result<(&'a CoinbaseAccount, &'static contracts::Contract, Value)> {
        let contract = contracts::contract(operation).ok_or_else(|| {
            failure(
                "UNSUPPORTED_OPERATION",
                "This Coinbase operation is not supported.",
            )
        })?;
        let object = input.as_object().ok_or_else(|| {
            failure(
                "INVALID_INPUT",
                "Coinbase input must be an object matching its closed operation schema.",
            )
        })?;
        let validator = jsonschema::validator_for(&(contract.schema)()).map_err(|_| {
            failure(
                "COINBASE_CONTRACT_INVALID",
                "Coinbase operation contract is invalid.",
            )
        })?;
        if !validator.is_valid(input) {
            return Err(failure(
                "INVALID_INPUT",
                "Coinbase input does not match the operation's exact schema.",
            ));
        }
        let product = match contract.product {
            ContractProduct::Advanced => CoinbaseProduct::Advanced,
            ContractProduct::Exchange => CoinbaseProduct::Exchange,
            ContractProduct::International => CoinbaseProduct::International,
        };
        let account = match object.get("account").and_then(Value::as_str) {
            Some(name) => self.account(name, product)?,
            None if matches!(contract.route, "products" | "product" | "product_book") => {
                let mut matching = self.accounts.iter().filter(|a| a.product == product);
                let account = matching.next().ok_or_else(|| {
                    failure(
                        "COINBASE_ACCOUNT_REQUIRED",
                        "No enrolled Coinbase key is available for this public route.",
                    )
                })?;
                if matching.next().is_some() {
                    return Err(failure(
                        "COINBASE_ACCOUNT_REQUIRED",
                        "Select an enrolled key when multiple Coinbase products are configured.",
                    ));
                }
                account
            }
            None => {
                return Err(failure(
                    "COINBASE_ACCOUNT_REQUIRED",
                    "Select an enrolled Coinbase account.",
                ));
            }
        };
        Ok((account, contract, input.clone()))
    }
    async fn read_contract(
        &self,
        account: &CoinbaseAccount,
        contract: &contracts::Contract,
        input: &Value,
    ) -> Result<Value> {
        let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
        let query = CoinbaseQuery {
            cursor: text("cursor"),
            limit: input.get("limit").and_then(Value::as_u64).map(|n| n as u16),
            product_id: if contract.route == "fills" {
                None
            } else {
                text("productId")
            },
            product_ids: if contract.route == "fills" {
                text("productId").into_iter().collect()
            } else {
                vec![]
            },
            order_ids: if contract.route == "fills" {
                text("id").into_iter().collect()
            } else {
                vec![]
            },
            account_id: None,
            portfolio_id: None,
            status: None,
            start_time: None,
            end_time: None,
            starting_after: text("startingAfter"),
            ending_before: text("endingBefore"),
            ..Default::default()
        };
        let endpoint = match contract.route {
            "key_permissions" => CoinbaseRoute::KeyPermissions,
            "accounts" => CoinbaseRoute::Accounts { query },
            "account" => CoinbaseRoute::Account {
                account_id: text("accountId").expect("schema"),
            },
            "portfolios" => CoinbaseRoute::Portfolios,
            "portfolio_accounts" => {
                let id = account.portfolio_id.as_ref().expect("validated");
                if text("portfolioId")
                    .as_deref()
                    .is_some_and(|requested| requested != id)
                {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_MISMATCH",
                        "Requested portfolio differs from the key-bound portfolio.",
                    ));
                }
                return self.read_advanced_portfolio_accounts(account, id).await;
            }
            "products" => CoinbaseRoute::Products { query },
            "product" => CoinbaseRoute::Product {
                product_id: text("id").expect("schema"),
            },
            "product_book" => CoinbaseRoute::ProductBook {
                product_id: text("id").expect("schema"),
            },
            "order" => CoinbaseRoute::Order {
                order_id: text("id").expect("schema"),
            },
            "fills" => CoinbaseRoute::Fills { query },
            "fees" => CoinbaseRoute::Fees,
            "deposit_addresses" => CoinbaseRoute::DepositAddresses {
                account_id: text("accountId").expect("schema"),
            },
            "transactions" => CoinbaseRoute::Transactions {
                account_id: text("accountId").expect("schema"),
                query,
            },
            "transaction" => CoinbaseRoute::Transaction {
                account_id: text("accountId").expect("schema"),
                transaction_id: text("transactionId").expect("schema"),
            },
            "order_preview" => CoinbaseRoute::OrderPreview,
            "exchange_accounts" => CoinbaseRoute::ExchangeAccounts,
            "exchange_account" => CoinbaseRoute::ExchangeAccount {
                account_id: text("accountId").expect("schema"),
            },
            "exchange_order" => CoinbaseRoute::ExchangeOrder {
                order_id: text("id").expect("schema"),
            },
            "exchange_fills" => CoinbaseRoute::ExchangeFills,
            "exchange_fees" => CoinbaseRoute::ExchangeFees,
            "exchange_transfers" => CoinbaseRoute::ExchangeTransfers {
                account_id: text("accountId").expect("schema"),
                query,
            },
            "international_accounts" => CoinbaseRoute::InternationalAccounts,
            "international_order" => CoinbaseRoute::InternationalOrder {
                order_id: text("id").expect("schema"),
            },
            "international_fills" => CoinbaseRoute::InternationalFills,
            _ => {
                return Err(failure(
                    "COINBASE_ROUTE_UNSUPPORTED",
                    "This operation has no reviewed Coinbase route.",
                ));
            }
        };
        let request = if contract.route == "order_preview" {
            CoinbaseRequest::json(gofin_domain::canonical_json(&json!({
                "product_id":text("productId").expect("schema"),"side":text("side").expect("schema"),
                "order_configuration":input["orderConfiguration"]
            }))?)
        } else {
            CoinbaseRequest::get()
        };
        let raw = self
            .request_for_account(&account.name, account.product, endpoint, request, None)
            .await?;
        if !(200..300).contains(&raw.status) {
            let response: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
            let native_code = response
                .get("error")
                .or_else(|| response.get("code"))
                .and_then(Value::as_str)
                .filter(|code| {
                    code.len() <= 64
                        && code
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                });
            return Err(failure(
                "COINBASE_REQUEST_FAILED",
                "Coinbase rejected or could not complete this read.",
            )
            .with_details(json!({"http_status":raw.status,"native_code":native_code})));
        }
        serde_json::from_slice(&raw.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned an invalid response.",
            )
        })
    }
    async fn prepare_order(&self, input: &Value) -> Result<PreparedOperation> {
        self.prepare_order_with_funding_policy(input, false).await
    }

    async fn prepare_order_with_funding_policy(
        &self,
        input: &Value,
        defer_insufficient_funds_for_plan: bool,
    ) -> Result<PreparedOperation> {
        let (account, _contract, _) = self.decode("coinbase.create-order", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Order preparation requires a verified Advanced portfolio identity.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let product_id = input["productId"].as_str().expect("schema");
        let product_response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Product {
                    product_id: product_id.to_owned(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&product_response.status) {
            return Err(failure(
                "COINBASE_PRODUCT_UNAVAILABLE",
                "Coinbase product metadata could not be verified.",
            ));
        }
        let product: Value = serde_json::from_slice(&product_response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid product metadata.",
            )
        })?;
        let product = product.get("product").unwrap_or(&product);
        if product.get("product_type").and_then(Value::as_str) != Some("SPOT")
            || product.get("trading_disabled").and_then(Value::as_bool) == Some(true)
            || product
                .get("base_currency_id")
                .and_then(Value::as_str)
                .is_none()
            || product
                .get("quote_currency_id")
                .and_then(Value::as_str)
                .is_none()
        {
            return Err(failure(
                "COINBASE_PRODUCT_UNSUPPORTED",
                "Only enabled spot products with explicit venue currency identities are currently supported.",
            ));
        }
        let order_id = client_order_id(
            &account.physical_identity,
            input["requestId"].as_str().expect("schema"),
        );
        // Coinbase's native preview endpoint (and pinned CCXT helper) does not
        // accept client_order_id. Keep preview parameters identical, then
        // persist the distinct exact create body with its durable identity.
        let preview_body = gofin_domain::canonical_json(&json!({
            "product_id":product_id,
            "side":input["side"],"order_configuration":input["orderConfiguration"]
        }))?;
        let create_body = gofin_domain::canonical_json(&json!({
            "client_order_id":order_id,"product_id":product_id,
            "side":input["side"],"order_configuration":input["orderConfiguration"]
        }))?;
        let preview = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::OrderPreview,
                CoinbaseRequest::json(preview_body.clone()),
                None,
            )
            .await?;
        if !(200..300).contains(&preview.status) {
            return Err(failure(
                "COINBASE_ORDER_PREVIEW_REJECTED",
                "Coinbase rejected the exact order preview.",
            ));
        }
        let preview_value: Value = serde_json::from_slice(&preview.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid order preview.",
            )
        })?;
        let preview_errors = preview_value
            .get("errs")
            .or_else(|| preview_value.get("errors"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PREVIEW_INCOMPLETE",
                    "Coinbase order preview omitted its native errs array.",
                )
            })?;
        let only_insufficient_funds = preview_errors.len() == 1
            && preview_errors[0].as_str() == Some("PREVIEW_INSUFFICIENT_FUND");
        let deferred_funding_error = defer_insufficient_funds_for_plan && only_insufficient_funds;
        if !preview_errors.is_empty() && !deferred_funding_error {
            return Err(failure(
                "COINBASE_ORDER_PREVIEW_REJECTED",
                "Coinbase order preview contains venue errors or funding blockers.",
            ));
        }
        if deferred_funding_error {
            validate_conclusive_insufficient_funds_preview(&preview_value, input)?;
        }
        let side = input["side"].as_str().expect("schema");
        let spend_cap = input["maximumSpend"].as_str().expect("schema");
        let order_config = input["orderConfiguration"]
            .as_object()
            .and_then(|m| m.iter().next())
            .ok_or_else(|| {
                failure(
                    "INVALID_INPUT",
                    "Exactly one native Advanced order configuration is required.",
                )
            })?;
        let config_name = order_config.0.as_str();
        let config = order_config.1;
        let is_market = config_name.starts_with("market_market_");
        let configured_base = config.get("base_size").and_then(Value::as_str);
        let configured_quote = config.get("quote_size").and_then(Value::as_str);
        let configured_limit = config.get("limit_price").and_then(Value::as_str);
        if config_name.ends_with("_gtd") && config.get("end_time").and_then(Value::as_str).is_none()
        {
            return Err(failure(
                "COINBASE_ORDER_TIME_INVALID",
                "GTD orders require an explicit end_time.",
            ));
        }
        if (config_name.ends_with("_gtd") || config_name.starts_with("twap_"))
            && let (Some(start), Some(end)) = (
                config.get("start_time").and_then(Value::as_str),
                config.get("end_time").and_then(Value::as_str),
            )
        {
            let start = chrono::DateTime::parse_from_rfc3339(start).map_err(|_| {
                failure(
                    "COINBASE_ORDER_TIME_INVALID",
                    "Scheduled order start_time must be RFC3339.",
                )
            })?;
            let end = chrono::DateTime::parse_from_rfc3339(end).map_err(|_| {
                failure(
                    "COINBASE_ORDER_TIME_INVALID",
                    "Scheduled order end_time must be RFC3339.",
                )
            })?;
            if end <= start {
                return Err(failure(
                    "COINBASE_ORDER_TIME_INVALID",
                    "Scheduled order end_time must be later than start_time.",
                ));
            }
        }
        if config_name.starts_with("twap_")
            && (config.get("start_time").and_then(Value::as_str).is_none()
                || config.get("end_time").and_then(Value::as_str).is_none()
                || config
                    .get("number_buckets")
                    .and_then(Value::as_u64)
                    .is_none()
                || config
                    .get("bucket_duration")
                    .and_then(Value::as_str)
                    .is_none())
        {
            return Err(failure(
                "COINBASE_TWAP_CONFIG_INVALID",
                "TWAP requires start/end, a bounded bucket count and exact bucket duration.",
            ));
        }
        for field in ["end_time", "start_time"] {
            if let Some(value) = config.get(field).and_then(Value::as_str) {
                let instant = chrono::DateTime::parse_from_rfc3339(value).map_err(|_| failure("COINBASE_ORDER_TIME_INVALID", "Scheduled order times must be RFC3339 timestamps with an explicit timezone."))?;
                if instant <= Utc::now() {
                    return Err(failure(
                        "COINBASE_ORDER_TIME_EXPIRED",
                        "Scheduled order start and end times must be in the future.",
                    ));
                }
            }
        }
        if config_name.starts_with("stop_limit_")
            && (config.get("stop_price").and_then(Value::as_str).is_none()
                || config
                    .get("stop_direction")
                    .and_then(Value::as_str)
                    .is_none())
        {
            return Err(failure(
                "COINBASE_STOP_CONFIG_INVALID",
                "Stop-limit orders require exact stop_price and stop_direction values.",
            ));
        }
        if config_name.starts_with("trigger_bracket_")
            && config
                .get("stop_trigger_price")
                .and_then(Value::as_str)
                .is_none()
        {
            return Err(failure(
                "COINBASE_BRACKET_CONFIG_INVALID",
                "Trigger-bracket orders require an exact stop_trigger_price.",
            ));
        }
        if config_name.starts_with("trigger_bracket_") {
            return Err(failure(
                "COINBASE_ORDER_CONFIGURATION_UNSUPPORTED",
                "Trigger-bracket child-order effects are not yet reconciled as one finite financial scope.",
            ));
        }
        if configured_base.is_some() && configured_quote.is_some() {
            return Err(failure(
                "COINBASE_ORDER_SIZE_AMBIGUOUS",
                "Specify exactly one base_size or quote_size for this order.",
            ));
        }
        if !is_market && configured_limit.is_none() && !config_name.starts_with("scaled_") {
            return Err(failure(
                "COINBASE_ORDER_PRICE_MISSING",
                "This order configuration requires a native limit_price.",
            ));
        }
        if config_name.starts_with("twap_")
            && configured_base.is_none()
            && configured_quote.is_none()
        {
            return Err(failure(
                "COINBASE_ORDER_SIZE_MISSING",
                "TWAP requires an exact base_size or quote_size.",
            ));
        }
        if is_market
            && ((side == "BUY" && configured_quote.is_none())
                || (side == "SELL" && configured_base.is_none()))
        {
            return Err(failure(
                "COINBASE_MARKET_ORDER_UNBOUNDED",
                "Market BUY requires an exact quote_size cap and market SELL requires an exact base_size cap.",
            ));
        }
        let safe_from_config = if side == "BUY" {
            if is_market {
                configured_quote
                    .is_some_and(|quote_size| decimal_le(quote_size, spend_cap).unwrap_or(false))
            } else if let (Some(base_size), Some(limit_price)) = (configured_base, configured_limit)
            {
                decimal_product_le(base_size, limit_price, spend_cap)?
            } else if let Some(quote_size) = configured_quote {
                decimal_le(quote_size, spend_cap)?
            } else {
                // A market buy expressed in base units has no deterministic
                // upper quote debit without the venue's exact preview total.
                preview_value
                    .get("order_total")
                    .and_then(Value::as_str)
                    .is_some_and(|total| decimal_le(total, spend_cap).unwrap_or(false))
            }
        } else if let Some(base_size) = configured_base {
            decimal_le(base_size, spend_cap)?
        } else {
            false
        };
        if !safe_from_config {
            return Err(failure(
                "COINBASE_ORDER_EXCEEDS_BOUND",
                "Typed order size or Coinbase preview exceeds the authorized maximum spend.",
            ));
        }
        if let Some(commission) = preview_value
            .get("commission_total")
            .and_then(Value::as_str)
        {
            if !decimal_le(commission, input["maximumFee"].as_str().expect("schema"))? {
                return Err(failure(
                    "COINBASE_FEE_EXCEEDS_BOUND",
                    "Coinbase preview commission exceeds the authorized fee cap.",
                ));
            }
        } else {
            return Err(failure(
                "COINBASE_FEE_UNVERIFIED",
                "Coinbase order preview omitted commission_total, so the fee cap cannot be verified.",
            ));
        }
        let base = product["base_currency_id"].as_str().expect("validated");
        let quote = product["quote_currency_id"].as_str().expect("validated");
        let base_increment = product
            .get("base_increment")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PRODUCT_METADATA_INCOMPLETE",
                    "Product metadata omitted the exact base increment.",
                )
            })?;
        let quote_increment = product
            .get("quote_increment")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PRODUCT_METADATA_INCOMPLETE",
                    "Product metadata omitted the exact quote increment.",
                )
            })?;
        let base_min = product
            .get("base_min_size")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PRODUCT_METADATA_INCOMPLETE",
                    "Product metadata omitted base_min_size.",
                )
            })?;
        let quote_min = product
            .get("quote_min_size")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PRODUCT_METADATA_INCOMPLETE",
                    "Product metadata omitted quote_min_size.",
                )
            })?;
        if let Some(size) = configured_base
            && (!decimal_multiple(size, base_increment)? || !decimal_le(base_min, size)?)
        {
            return Err(failure(
                "COINBASE_INCREMENT_INVALID",
                "Base size violates the venue increment or minimum size.",
            ));
        }
        if let Some(size) = configured_quote
            && (!decimal_multiple(size, quote_increment)? || !decimal_le(quote_min, size)?)
        {
            return Err(failure(
                "COINBASE_INCREMENT_INVALID",
                "Quote size violates the venue increment or minimum size.",
            ));
        }
        let price_increment = product
            .get("price_increment")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PRODUCT_METADATA_INCOMPLETE",
                    "Product metadata omitted the exact price increment.",
                )
            })?;
        for field in ["limit_price", "stop_price", "stop_trigger_price"] {
            if let Some(price) = config.get(field).and_then(Value::as_str)
                && !decimal_multiple(price, price_increment)?
            {
                return Err(failure(
                    "COINBASE_INCREMENT_INVALID",
                    "Order price or stop trigger is not an exact multiple of the venue price increment.",
                ));
            }
        }
        let base_asset = product_asset(base);
        let quote_asset = product_asset(quote);
        let (debit_asset, credit_asset) = if side == "BUY" {
            (quote_asset.clone(), base_asset)
        } else {
            (base_asset, quote_asset.clone())
        };
        let debit = exact_atoms(
            input["maximumSpend"].as_str().expect("schema"),
            debit_asset.decimals,
        )?;
        let min_receive = exact_atoms(
            input["minimumReceive"].as_str().expect("schema"),
            credit_asset.decimals,
        )?;
        let fee = exact_atoms(input["maximumFee"].as_str().expect("schema"), 36)?;
        let resource = account.resource.clone();
        let debits = vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: debit_asset.clone(),
                atoms: debit.clone(),
            },
        }];
        let costs = vec![CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: quote_asset.clone(),
                atoms: fee,
            },
            treatment: if side == "SELL" {
                CostTreatment::DeductedFromCredit
            } else {
                CostTreatment::AdditionalDebit
            },
        }];
        let valuation = self.financial_valuation(&debits, &costs).await?;
        let observed_at = gofin_domain::now();
        let expires_at = (Utc::now() + Duration::seconds(45)).to_rfc3339();
        let mut prepared = PreparedOperation {
            operation: "coinbase.create-order".into(),
            input: input.clone(),
            binding: json!({"version":1,"provider":"coinbase","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":account.portfolio_id,"productId":product_id,"clientOrderId":order_id,"requestBodyExact":create_body,"previewRequestBodyExact":preview_body,"preview":preview_value,"baseCurrencyId":base,"quoteCurrencyId":quote,"maximumSpend":input["maximumSpend"],"minimumReceive":input["minimumReceive"],"maximumFee":input["maximumFee"]}),
            summary: format!(
                "Preview {} {} order for {}",
                side,
                product_id,
                input["maximumSpend"].as_str().unwrap_or("")
            ),
            checks: vec![Check {
                name: "venue_order_preview".into(),
                status: CheckStatus::Passed,
                detail: if deferred_funding_error {
                    "Coinbase returned conclusive quote/fee data but marked insufficient current funds; this preview is read-only plan evidence and cannot be dispatched. The execution step must obtain an error-free preview after declared earlier funding has settled.".into()
                } else {
                    "Coinbase accepted the exact typed order preview.".into()
                },
            }],
            warnings: if deferred_funding_error {
                vec![
                "PREVIEW_INSUFFICIENT_FUND was deferred only for read-only future-funded plan feasibility; ordinary preparation, refresh and dispatch still require errs: [].".into(),
            ]
            } else {
                vec![
                    "Coinbase preview does not guarantee execution price, fill, or settlement."
                        .into(),
                ]
            },
            observed_at,
            expires_at,
            valuation: Some(valuation),
            intent: FinancialIntent {
                debits,
                maximum_costs: costs,
                expected: vec![EffectConstraint::Conversion {
                    resource,
                    input: debit_asset,
                    maximum_input: debit,
                    output: credit_asset,
                    minimum_output: min_receive,
                }],
                locks: vec![account.resource.clone()],
            },
            operation_version: 1,
            action_deadline: None,
        };
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    async fn prepare_cancel_order(&self, input: &Value) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.cancel-order", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Cancellation requires a verified Advanced portfolio identity.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let order_id = input["venueOrderId"].as_str().expect("schema");
        let client_id = input["clientOrderId"].as_str().expect("schema");
        let product_id = input["productId"].as_str().expect("schema");
        let response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Order {
                    order_id: order_id.to_owned(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_ORDER_UNAVAILABLE",
                "The exact order cannot be verified for cancellation.",
            ));
        }
        let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid order data.",
            )
        })?;
        let order = doc.get("order").unwrap_or(&doc);
        if order
            .get("order_id")
            .or_else(|| order.get("orderId"))
            .and_then(Value::as_str)
            != Some(order_id)
            || order
                .get("client_order_id")
                .or_else(|| order.get("clientOrderId"))
                .and_then(Value::as_str)
                != Some(client_id)
            || order
                .get("product_id")
                .or_else(|| order.get("productId"))
                .and_then(Value::as_str)
                != Some(product_id)
        {
            return Err(failure(
                "COINBASE_ORDER_IDENTITY_MISMATCH",
                "The venue order does not match the exact persisted order and product identities.",
            ));
        }
        let status = order
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        if !matches!(
            status.as_str(),
            "OPEN" | "PENDING" | "QUEUED" | "PARTIALLY_FILLED"
        ) {
            return Err(failure(
                "COINBASE_ORDER_NOT_OPEN",
                "The order has no verified open remainder; reconcile the original operation.",
            ));
        }
        let original = gofin_domain::Submission {
            kind: "coinbase".into(),
            reference: client_id.to_owned(),
            data: json!({"product":"coinbase","clientOrderId":client_id,"account":account.name,"productId":product_id}),
        };
        let state = self.execution_state.read().map_err(|_| failure("EXECUTION_STATE_UNAVAILABLE", "Protected execution history cannot be read for cancellation."))?.clone().ok_or_else(|| failure("EXECUTION_STATE_UNAVAILABLE", "Cancellation requires the protected Fin journal so its original operation can be attested."))?;
        let records = state.operations_by_prepared_binding(
            "coinbase.create-order",
            "/preview/binding/clientOrderId",
            &json!(client_id),
        )?;
        let matches = records
            .into_iter()
            .filter(|record| {
                record.preview.as_ref().is_some_and(|preview| {
                    preview.operation == "coinbase.create-order"
                        && preview.binding["account"].as_str() == Some(account.name.as_str())
                        && preview.binding["productId"].as_str() == Some(product_id)
                        && preview.binding["clientOrderId"].as_str() == Some(client_id)
                }) && record.submission.as_ref() == Some(&original)
                    && matches!(
                        record.status,
                        Status::Submitted | Status::Pending | Status::Unknown
                    )
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(failure(
                "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                "The protected journal does not identify exactly one unresolved Fin operation for this exact venue order.",
            ));
        }
        let operation_id = matches[0].id.clone();
        let binding = json!({"version":1,"provider":"coinbase","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":account.portfolio_id,"orderId":order_id,"clientOrderId":client_id,"productId":product_id,"operationId":operation_id,"originalSubmission":original,"requestBodyExact":gofin_domain::canonical_json(&json!({"order_ids":[order_id]}))?});
        Ok(PreparedOperation {
            operation: "coinbase.cancel-order".into(), input: input.clone(), binding,
            summary: format!("Cancel open remainder of Coinbase order {order_id}; fills remain with the original operation."),
            checks: vec![Check { name:"original_order_identity".into(), status:CheckStatus::Passed, detail:"Exact venue order, client order ID, product, and permissioned portfolio were verified.".into() }],
            warnings: vec!["Fills can race cancellation; reconcile this cancellation and the original order before acting again.".into()],
            observed_at:gofin_domain::now(), expires_at:(Utc::now()+Duration::seconds(45)).to_rfc3339(), valuation:None,
            intent:FinancialIntent { debits:vec![], maximum_costs:vec![], expected:vec![EffectConstraint::SubmissionCancellation { operation_id, submission:original }], locks:vec![account.resource.clone()] },
            operation_version:1, action_deadline:None,
        })
    }
    async fn prepare_cancel_orders(&self, input: &Value) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.cancel-orders", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Batch cancellation requires the verified Advanced portfolio identity.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let state = self
            .execution_state
            .read()
            .map_err(|_| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Protected execution history cannot be read for batch cancellation.",
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Batch cancellation requires the protected Fin journal.",
                )
            })?;
        let rows = input["orders"].as_array().expect("closed schema");
        let mut ids = BTreeSet::new();
        let mut parent_ids = BTreeSet::new();
        let mut bound = Vec::with_capacity(rows.len());
        let mut effects = Vec::with_capacity(rows.len());
        for row in rows {
            let order_id = row["venueOrderId"].as_str().expect("schema");
            let client_id = row["clientOrderId"].as_str().expect("schema");
            let product_id = row["productId"].as_str().expect("schema");
            let operation_id = row["operationId"].as_str().expect("schema");
            if !ids.insert(order_id.to_owned()) || !parent_ids.insert(operation_id.to_owned()) {
                return Err(failure(
                    "COINBASE_BATCH_DUPLICATE_ORDER",
                    "A batch cannot repeat a venue order or its parent Fin operation.",
                ));
            }
            let parent = state.operation(operation_id)?.ok_or_else(|| {
                failure(
                    "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                    "A listed original Fin operation is absent from the protected journal.",
                )
            })?;
            let original = parent.submission.clone().ok_or_else(|| {
                failure(
                    "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                    "A listed original operation has no persisted submission identity.",
                )
            })?;
            if parent.operation != "coinbase.create-order"
                || original.kind != "coinbase"
                || original.reference != client_id
                || parent.preview.as_ref().is_none_or(|p| {
                    p.binding["account"].as_str() != Some(account.name.as_str())
                        || p.binding["clientOrderId"].as_str() != Some(client_id)
                        || p.binding["productId"].as_str() != Some(product_id)
                })
                || !matches!(
                    parent.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                )
            {
                return Err(failure(
                    "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                    "Every batch member must link to one unresolved original order operation.",
                ));
            }
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Order {
                        order_id: order_id.into(),
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_ORDER_UNAVAILABLE",
                    "Every batch order must be independently readable before cancellation.",
                ));
            }
            let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid order data.",
                )
            })?;
            let order = doc.get("order").unwrap_or(&doc);
            if order["order_id"].as_str() != Some(order_id)
                || order["client_order_id"].as_str() != Some(client_id)
                || order["product_id"].as_str() != Some(product_id)
                || !matches!(
                    order["status"]
                        .as_str()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "OPEN" | "PENDING" | "QUEUED" | "PARTIALLY_FILLED"
                )
            {
                return Err(failure(
                    "COINBASE_ORDER_IDENTITY_MISMATCH",
                    "Every batch member must be the exact journal-linked order with an open remainder.",
                ));
            }
            effects.push(EffectConstraint::SubmissionCancellation {
                operation_id: operation_id.into(),
                submission: original.clone(),
            });
            bound.push(json!({"operationId":operation_id,"orderId":order_id,"clientOrderId":client_id,"productId":product_id,"originalSubmission":original}));
        }
        let body =
            gofin_domain::canonical_json(&json!({"order_ids":ids.iter().collect::<Vec<_>>() }))?;
        Ok(PreparedOperation {
            operation:"coinbase.cancel-orders".into(), input:input.clone(),
            binding:json!({"version":1,"provider":"coinbase","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":account.portfolio_id,"requestId":input["requestId"],"orders":bound,"requestBodyExact":body}),
            summary:format!("Cancel {} individually verified Coinbase Advanced orders.", bound.len()),
            checks:vec![Check{name:"linked_batch_orders".into(),status:CheckStatus::Passed,detail:"Every exact venue order was linked to one unresolved protected Fin operation and verified open.".into()}],
            warnings:vec!["Batch outcomes are reconciled independently. Any missing/failed member keeps the batch unresolved; no member is replayed.".into()],
            observed_at:gofin_domain::now(),expires_at:(Utc::now()+Duration::seconds(45)).to_rfc3339(),valuation:None,
            intent:FinancialIntent{debits:vec![],maximum_costs:vec![],expected:effects,locks:vec![account.resource.clone()]},operation_version:1,action_deadline:None,
        })
    }
    async fn prepare_edit_order(
        &self,
        input: &Value,
        linked: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.edit-order", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Order edits require a verified Advanced portfolio identity.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let original = linked.original.as_ref().ok_or_else(|| failure("COINBASE_ORIGINAL_OPERATION_UNVERIFIED", "Editing requires protected linked context for the original create-order operation."))?;
        if !linked.revisions.is_empty() {
            return Err(failure(
                "COINBASE_EDIT_CHAIN_UNSUPPORTED",
                "A second linked Advanced price amendment is not enabled until predecessor amendment ordering is independently proven.",
            ));
        }
        if original.operation != "coinbase.create-order"
            || original.binding["account"] != account.name
            || original.binding["productId"] != input["productId"]
            || original.binding["clientOrderId"] != input["clientOrderId"]
            || original.binding["portfolioId"].as_str() != account.portfolio_id.as_deref()
        {
            return Err(failure(
                "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                "The protected linked operation does not match the requested account, product, client order, and portfolio.",
            ));
        }
        let config = &original.input["orderConfiguration"]["limit_limit_gtc"];
        if !config.is_object() || config["base_size"].as_str().is_none() {
            return Err(failure(
                "COINBASE_EDIT_UNSUPPORTED",
                "Only exact single-order limit GTC orders can be edited.",
            ));
        }
        let evidence = linked
            .original_evidence
            .as_ref()
            .and_then(|e| e.submission.as_ref())
            .ok_or_else(|| {
                failure(
                    "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                    "The protected journal has no durable original submission identity.",
                )
            })?;
        if evidence.kind != "coinbase"
            || evidence.reference != input["clientOrderId"].as_str().unwrap_or("")
            || evidence.data["clientOrderId"] != input["clientOrderId"]
        {
            return Err(failure(
                "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                "The original durable submission does not match the exact client order ID.",
            ));
        }
        let state = self
            .execution_state
            .read()
            .map_err(|_| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Protected execution history cannot be read for order editing.",
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Order editing requires the protected Fin journal.",
                )
            })?;
        let records = state.operations_by_prepared_binding(
            "coinbase.create-order",
            "/preview/binding/clientOrderId",
            &input["clientOrderId"],
        )?;
        if !records.iter().any(|record| {
            record.id == input["operationId"].as_str().unwrap_or("")
                && matches!(
                    record.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                )
                && record
                    .preview
                    .as_ref()
                    .is_some_and(|p| p.binding == original.binding)
                && record.submission.as_ref() == Some(evidence)
        }) {
            return Err(failure(
                "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                "The requested original operation ID does not match the protected journal.",
            ));
        }
        let order_id = input["venueOrderId"].as_str().expect("schema");
        let response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Order {
                    order_id: order_id.into(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_ORDER_UNAVAILABLE",
                "The original order cannot be verified for editing.",
            ));
        }
        let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid order data.",
            )
        })?;
        let order = doc.get("order").unwrap_or(&doc);
        let status = order["status"].as_str().unwrap_or("").to_ascii_uppercase();
        let expected_price = linked
            .effective_scope()
            .and_then(|scope| scope.binding["effectiveLimitPrice"].as_str())
            .or_else(|| config["limit_price"].as_str());
        if order
            .get("order_id")
            .or_else(|| order.get("orderId"))
            .and_then(Value::as_str)
            != Some(order_id)
            || order
                .get("client_order_id")
                .or_else(|| order.get("clientOrderId"))
                .and_then(Value::as_str)
                != Some(input["clientOrderId"].as_str().unwrap_or(""))
            || order
                .get("product_id")
                .or_else(|| order.get("productId"))
                .and_then(Value::as_str)
                != Some(input["productId"].as_str().unwrap_or(""))
            || !edit_order_configuration_matches(
                order,
                config["base_size"].as_str().unwrap_or(""),
                expected_price.unwrap_or(""),
                original.input["side"].as_str().unwrap_or(""),
                account.portfolio_id.as_deref(),
            )
            || !matches!(status.as_str(), "OPEN" | "PENDING" | "QUEUED")
        {
            return Err(failure(
                "COINBASE_ORDER_IDENTITY_MISMATCH",
                "The exact linked limit GTC order is not open for editing.",
            ));
        }
        let new_price = input["newPrice"].as_str().expect("schema");
        exact_atoms(new_price, 36)?;
        if !decimal_le("0", new_price)? || decimal_le(new_price, "0")? {
            return Err(failure(
                "COINBASE_EDIT_BOUND_INVALID",
                "The new limit price must be positive.",
            ));
        }
        let edit_body =
            gofin_domain::canonical_json(&json!({"order_id":order_id,"price":new_price}))?;
        let preview = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::OrderEditPreview,
                CoinbaseRequest::json(edit_body.clone()),
                None,
            )
            .await?;
        if !(200..300).contains(&preview.status) {
            return Err(failure(
                "COINBASE_EDIT_PREVIEW_REJECTED",
                "Coinbase rejected the exact order edit preview.",
            ));
        }
        let preview_value: Value = serde_json::from_slice(&preview.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid edit preview data.",
            )
        })?;
        let errors = preview_value
            .get("errors")
            .or_else(|| preview_value.get("errs"))
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                failure(
                    "COINBASE_EDIT_PREVIEW_INCONCLUSIVE",
                    "Edit preview omitted its native errors array.",
                )
            })?;
        if !errors.is_empty() {
            return Err(failure(
                "COINBASE_EDIT_PREVIEW_REJECTED",
                "Coinbase returned native edit preview errors.",
            ));
        }
        let commission = preview_value["commission_total"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_EDIT_PREVIEW_INCONCLUSIVE",
                "Edit preview omitted its exact commission.",
            )
        })?;
        let total = preview_value["order_total"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_EDIT_PREVIEW_INCONCLUSIVE",
                "Edit preview omitted its exact order total.",
            )
        })?;
        let base_size = config["base_size"].as_str().unwrap_or("");
        let economics_ok = decimal_le(
            commission,
            original.input["maximumFee"].as_str().unwrap_or("0"),
        )? && if original.input["side"] == "BUY" {
            decimal_product_le(
                base_size,
                new_price,
                original.input["maximumSpend"].as_str().unwrap_or("0"),
            )?
        } else {
            decimal_le(
                original.input["minimumReceive"].as_str().unwrap_or("0"),
                total,
            )? && decimal_product_ge_sum(
                base_size,
                new_price,
                original.input["minimumReceive"].as_str().unwrap_or("0"),
                commission,
            )?
        };
        if !economics_ok {
            return Err(failure(
                "COINBASE_EDIT_BOUND_INVALID",
                "The edited order preview exceeds the original fee or spend ceiling.",
            ));
        }
        let mut revision_binding = original.binding.clone();
        revision_binding["effectiveLimitPrice"] = json!(new_price);
        let mut revision_intent = original.intent.clone();
        revision_intent
            .expected
            .retain(|effect| !matches!(effect, EffectConstraint::SubmissionAmendment { .. }));
        let revision = gofin_domain::amendments::AmendmentScope {
            intent: revision_intent,
            binding: revision_binding,
            valuation: original.valuation.clone(),
        };
        let intent = FinancialIntent {
            debits: vec![],
            maximum_costs: vec![],
            expected: vec![EffectConstraint::SubmissionAmendment {
                operation_id: input["operationId"].as_str().unwrap().into(),
                submission: evidence.clone(),
                revision: Box::new(revision),
            }],
            locks: original.intent.locks.clone(),
        };
        Ok(PreparedOperation {
            operation:"coinbase.edit-order".into(), input:input.clone(),
            binding:json!({"version":1,"provider":"coinbase","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":account.portfolio_id,"productId":input["productId"],"orderId":order_id,"clientOrderId":input["clientOrderId"],"operationId":input["operationId"],"originalSubmission":evidence,"originalBinding":original.binding,"originalInput":original.input,"predecessorPrice":expected_price,"newPrice":new_price,"requestBodyExact":edit_body,"preview":preview_value}),
            summary:format!("Edit the price of linked Coinbase limit GTC order {order_id}"),
            checks:vec![Check{name:"linked_order_identity".into(),status:CheckStatus::Passed,detail:"Original Fin journal operation, durable client ID, portfolio, product, and open venue order were verified.".into()},Check{name:"venue_edit_preview".into(),status:CheckStatus::Passed,detail:"Native edit preview returned no errors and remained within original spend and fee ceilings.".into()}],
            warnings:vec!["An edit acknowledgment does not settle fills; the original operation retains all monetary fill accounting.".into()],
            observed_at:gofin_domain::now(), expires_at:(Utc::now()+Duration::seconds(45)).to_rfc3339(), valuation:original.valuation.clone(), intent, operation_version:1, action_deadline:None,
        })
    }
    async fn refresh_edit_order(&self, approved: &PreparedOperation) -> Result<PreparedOperation> {
        let account = self.account(
            approved.binding["account"].as_str().unwrap_or(""),
            CoinbaseProduct::Advanced,
        )?;
        self.verify_advanced_key(account, true, false).await?;
        let order_id = approved.binding["orderId"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Edited order identity is missing.",
            )
        })?;
        let response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Order {
                    order_id: order_id.into(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_ORDER_UNAVAILABLE",
                "The linked order cannot be revalidated.",
            ));
        }
        let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid order data.",
            )
        })?;
        let order = doc.get("order").unwrap_or(&doc);
        let status = order["status"].as_str().unwrap_or("").to_ascii_uppercase();
        let original_config =
            &approved.binding["originalInput"]["orderConfiguration"]["limit_limit_gtc"];
        if order["order_id"].as_str() != Some(order_id)
            || order["client_order_id"].as_str() != approved.binding["clientOrderId"].as_str()
            || order["product_id"].as_str() != approved.binding["productId"].as_str()
            || !edit_order_configuration_matches(
                order,
                original_config["base_size"].as_str().unwrap_or(""),
                approved.binding["predecessorPrice"].as_str().unwrap_or(""),
                approved.binding["originalInput"]["side"]
                    .as_str()
                    .unwrap_or(""),
                account.portfolio_id.as_deref(),
            )
            || !matches!(status.as_str(), "OPEN" | "PENDING" | "QUEUED")
        {
            return Err(failure(
                "COINBASE_ORDER_IDENTITY_MISMATCH",
                "The exact linked order is no longer editable.",
            ));
        }
        let submission: gofin_domain::Submission = serde_json::from_value(
            approved.binding["originalSubmission"].clone(),
        )
        .map_err(|_| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Original submission identity is invalid.",
            )
        })?;
        let state = self
            .execution_state
            .read()
            .map_err(|_| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Protected execution history cannot be read.",
                )
            })?
            .clone()
            .ok_or_else(|| {
                failure(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "Order editing requires the protected Fin journal.",
                )
            })?;
        let records = state.operations_by_prepared_binding(
            "coinbase.create-order",
            "/preview/binding/clientOrderId",
            &approved.binding["clientOrderId"],
        )?;
        if !records.iter().any(|record| {
            record.id == approved.binding["operationId"].as_str().unwrap_or("")
                && record.submission.as_ref() == Some(&submission)
                && matches!(
                    record.status,
                    Status::Submitted | Status::Pending | Status::Unknown
                )
        }) {
            return Err(failure(
                "COINBASE_ORIGINAL_OPERATION_UNVERIFIED",
                "The linked parent operation no longer matches protected journal evidence.",
            ));
        }
        let body = approved.binding["requestBodyExact"]
            .as_str()
            .ok_or_else(|| failure("COINBASE_BINDING_INVALID", "Exact edit body is missing."))?;
        let preview_response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::OrderEditPreview,
                CoinbaseRequest::json(body.into()),
                None,
            )
            .await?;
        if !(200..300).contains(&preview_response.status) {
            return Err(failure(
                "COINBASE_EDIT_PREVIEW_REJECTED",
                "Coinbase rejected the refreshed edit preview.",
            ));
        }
        let preview: Value = serde_json::from_slice(&preview_response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid edit preview data.",
            )
        })?;
        let errors = preview
            .get("errors")
            .or_else(|| preview.get("errs"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                failure(
                    "COINBASE_EDIT_PREVIEW_INCONCLUSIVE",
                    "Refreshed edit preview omitted its native errors array.",
                )
            })?;
        if !errors.is_empty() {
            return Err(failure(
                "COINBASE_EDIT_PREVIEW_REJECTED",
                "Coinbase returned native edit preview errors.",
            ));
        }
        let fee = preview["commission_total"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_EDIT_PREVIEW_INCONCLUSIVE",
                "Refreshed edit preview omitted exact commission.",
            )
        })?;
        let size =
            approved.binding["originalInput"]["orderConfiguration"]["limit_limit_gtc"]["base_size"]
                .as_str()
                .ok_or_else(|| {
                    failure("COINBASE_BINDING_INVALID", "Original GTC size is missing.")
                })?;
        let side = approved.binding["originalInput"]["side"]
            .as_str()
            .unwrap_or("");
        let price = approved.binding["newPrice"].as_str().unwrap_or("");
        let original = &approved.binding["originalInput"];
        let economics_ok = if side == "BUY" {
            decimal_le(fee, original["maximumFee"].as_str().unwrap_or("0"))?
                && decimal_product_le(
                    size,
                    price,
                    original["maximumSpend"].as_str().unwrap_or("0"),
                )?
        } else if side == "SELL" {
            let proceeds = preview["order_total"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_EDIT_PREVIEW_INCONCLUSIVE",
                    "Refreshed sell preview omitted exact net order total.",
                )
            })?;
            decimal_le(fee, original["maximumFee"].as_str().unwrap_or("0"))?
                && decimal_le(original["minimumReceive"].as_str().unwrap_or("0"), proceeds)?
                && decimal_product_ge_sum(
                    size,
                    price,
                    original["minimumReceive"].as_str().unwrap_or("0"),
                    fee,
                )?
        } else {
            false
        };
        if !economics_ok {
            return Err(failure(
                "COINBASE_EDIT_BOUND_INVALID",
                "Refreshed edit exceeds the original price, spend, receipt, or fee ceiling.",
            ));
        }
        let mut fresh = approved.clone();
        fresh.binding["preview"] = preview;
        fresh.observed_at = gofin_domain::now();
        fresh.expires_at = (Utc::now() + Duration::seconds(45)).to_rfc3339();
        Ok(fresh)
    }
    async fn refresh_conversion(&self, approved: &PreparedOperation) -> Result<PreparedOperation> {
        let account_name = approved.binding["account"]
            .as_str()
            .ok_or_else(|| failure("COINBASE_BINDING_INVALID", "Conversion account is missing."))?;
        let trade_id = approved.binding["tradeId"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Conversion quote identity is missing.",
            )
        })?;
        let from = approved.binding["fromCurrency"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Conversion source currency is missing.",
            )
        })?;
        let to = approved.binding["toCurrency"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Conversion destination currency is missing.",
            )
        })?;
        let account = self.account(account_name, CoinbaseProduct::Advanced)?;
        self.verify_advanced_key(account, true, false).await?;
        let response = self
            .request_for_account(
                account_name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::ConvertTradeExact {
                    trade_id: trade_id.into(),
                    from_account: from.into(),
                    to_account: to.into(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_CONVERSION_QUOTE_UNAVAILABLE",
                "The exact approved conversion quote cannot be revalidated; it will not be replaced with a new trade ID.",
            ));
        }
        let document: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid conversion quote data.",
            )
        })?;
        let quote = document.get("trade").unwrap_or(&document);
        let approved_quote = &approved.binding["quote"];
        let exact_quote_fields = [
            "/id",
            "/user_entered_amount/value",
            "/user_entered_amount/currency",
            "/amount/value",
            "/amount/currency",
            "/subtotal/value",
            "/subtotal/currency",
            "/total/value",
            "/total/currency",
            "/total_fee/amount/value",
            "/total_fee/amount/currency",
            "/source/coinbase_account/account_id",
            "/target/coinbase_account/account_id",
        ];
        if quote["id"].as_str() != Some(trade_id)
            || quote["status"].as_str() != Some("TRADE_STATUS_CREATED")
            || exact_quote_fields
                .iter()
                .any(|path| quote.pointer(path) != approved_quote.pointer(path))
        {
            return Err(failure(
                "COINBASE_CONVERSION_QUOTE_CHANGED",
                "The original conversion quote is no longer in its exact approved state; a new trade ID is never substituted during refresh.",
            ));
        }
        let mut refreshed = approved.clone();
        refreshed.checks.push(Check {
            name: "refreshed_conversion_quote".into(),
            status: CheckStatus::Passed,
            detail: "The original Coinbase trade ID, source, destination, principal, output, and fee remain exact and commit-ready.".into(),
        });
        Ok(refreshed)
    }
    async fn prepare_deposit_address_configuration(
        &self,
        input: &Value,
    ) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.create-deposit-address", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Address creation requires a verified Advanced portfolio identity.",
            ));
        }
        // Key permissions currently expose view/trade/transfer but not Receive.
        // Never substitute Transfer for Receive; the native create route is the
        // authority for receive access and rejection remains visible.
        self.verify_advanced_key(account, false, false).await?;
        let account_id = input["accountId"].as_str().expect("closed schema");
        let account_state = self
            .advanced_currency_account_details(account, account_id)
            .await?;
        let currency = account_state
            .get("currency")
            .and_then(|value| {
                value
                    .get("code")
                    .or_else(|| value.get("id"))
                    .or(Some(value))
            })
            .and_then(Value::as_str)
            .or_else(|| account_state.get("currency_id").and_then(Value::as_str))
            .ok_or_else(|| {
                failure(
                    "COINBASE_CURRENCY_UNAVAILABLE",
                    "Currency account omitted its exact currency identity.",
                )
            })?;
        Ok(PreparedOperation {
            operation: "coinbase.create-deposit-address".into(),
            input: input.clone(),
            binding: json!({"version":1,"provider":"coinbase","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":account.portfolio_id,"currencyAccountId":account_id,"currency":currency,"requestBodyExact":""}),
            summary: format!("Create a new Coinbase receive address for {currency}."),
            checks: vec![Check { name:"currency_account_identity".into(), status:CheckStatus::Passed, detail:"The exact active and ready currency account and key-bound portfolio were verified; the permissions endpoint does not attest Receive scope.".into() }],
            warnings: vec!["Receive permission is not reported by the key-permissions endpoint; Coinbase enforces it on this create request. Coinbase provides no request idempotency key, so an uncertain response is never replayed.".into()],
            observed_at:gofin_domain::now(), expires_at:(Utc::now()+Duration::seconds(45)).to_rfc3339(), valuation:None,
            intent:FinancialIntent { debits:vec![], maximum_costs:vec![], expected:vec![], locks:vec![account.resource.clone()] },
            operation_version:1, action_deadline:None,
        })
    }
    async fn prepare_portfolio_configuration(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode(operation, input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Portfolio configuration requires the verified Advanced key-bound portfolio.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let create = operation == "coinbase.create-portfolio";
        let desired_name = if create {
            input["name"].as_str().expect("closed schema")
        } else {
            input["newName"].as_str().expect("closed schema")
        };
        if desired_name.trim() != desired_name || desired_name.chars().any(char::is_control) {
            return Err(failure(
                "COINBASE_PORTFOLIO_NAME_INVALID",
                "Portfolio names must be nonempty, trimmed text without control characters.",
            ));
        }
        let mut binding = json!({
            "version":1,"provider":"coinbase","account":account.name,
            "resource":account.resource,"physicalIdentity":account.physical_identity,
            "portfolioId":account.portfolio_id,"requestId":input["requestId"],
            "newName":desired_name
        });
        let route = if create {
            binding["requestBodyExact"] =
                json!(gofin_domain::canonical_json(&json!({"name":desired_name}))?);
            "portfolios_create"
        } else {
            let current = account.portfolio_id.as_deref().ok_or_else(|| {
                failure(
                    "COINBASE_PORTFOLIO_ID_UNAVAILABLE",
                    "The CDP key is not bound to an exact portfolio identity.",
                )
            })?;
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Portfolios,
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_PORTFOLIOS_UNAVAILABLE",
                    "Portfolio identity cannot be verified for rename.",
                ));
            }
            let document: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned malformed portfolio data.",
                )
            })?;
            let rows = document
                .get("portfolios")
                .or_else(|| document.get("data"))
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Portfolio listing omitted its native portfolio array.",
                    )
                })?;
            let matching = rows
                .iter()
                .filter(|row| {
                    row.get("uuid")
                        .or_else(|| row.get("portfolio_uuid"))
                        .and_then(Value::as_str)
                        == Some(current)
                })
                .collect::<Vec<_>>();
            if matching.len() != 1 || matching[0]["deleted"] == true {
                return Err(failure(
                    "COINBASE_PORTFOLIO_IDENTITY_MISMATCH",
                    "The enrolled key-bound portfolio is absent, deleted, or ambiguous.",
                ));
            }
            binding["previousName"] = matching[0].get("name").cloned().ok_or_else(|| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "The current portfolio name is missing.",
                )
            })?;
            binding["requestBodyExact"] =
                json!(gofin_domain::canonical_json(&json!({"name":desired_name}))?);
            "portfolios_edit"
        };
        let mut prepared = PreparedOperation {
            operation:operation.into(), input:input.clone(), binding,
            summary:if create { format!("Create Coinbase Advanced portfolio {desired_name}; a separate portfolio-scoped CDP key is required before use.") } else { format!("Rename the enrolled Coinbase Advanced portfolio to {desired_name}.") },
            checks:vec![Check { name:"key_bound_configuration_scope".into(), status:CheckStatus::Passed,
                detail:"Authenticated key trade permission and immutable enrolled portfolio identity were verified.".into() }],
            warnings:vec![if create { "Coinbase does not make the existing API key permissioned for the new portfolio; enroll a dedicated CDP key before using it. A lost create response is ambiguous and will never be replayed.".into() } else { "The rename is limited to the portfolio bound to this key. A lost response is reconciled by exact portfolio ID and resulting name; no write is replayed.".into() }],
            observed_at:gofin_domain::now(), expires_at:(Utc::now()+Duration::seconds(45)).to_rfc3339(), valuation:None,
            intent:FinancialIntent { debits:vec![], maximum_costs:vec![], expected:vec![], locks:vec![account.resource.clone()] },
            operation_version:1, action_deadline:None,
        };
        prepared.binding["configurationRoute"] = json!(route);
        Ok(prepared)
    }
    async fn read_advanced_portfolio_accounts(
        &self,
        account: &CoinbaseAccount,
        portfolio_id: &str,
    ) -> Result<Value> {
        if account.portfolio_id.as_deref() != Some(portfolio_id) {
            return Err(failure(
                "COINBASE_PORTFOLIO_MISMATCH",
                "Requested portfolio differs from this CDP key's enrolled portfolio.",
            ));
        }
        self.verify_advanced_key(account, false, false).await?;
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut rows = Vec::new();
        let mut terminated = false;
        for page_number in 0..1000 {
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Accounts {
                        query: CoinbaseQuery {
                            portfolio_id: Some(portfolio_id.to_owned()),
                            limit: Some(250),
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_PORTFOLIO_ACCOUNTS_UNAVAILABLE",
                    "The key-scoped native accounts list could not be read.",
                ));
            }
            let page: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Native account data is malformed.",
                )
            })?;
            let page_rows = page
                .get("accounts")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_PORTFOLIO_ACCOUNTS_INCOMPLETE",
                        "Native account response omitted its accounts array.",
                    )
                })?;
            for row in page_rows {
                let id = row
                    .get("uuid")
                    .or_else(|| row.get("id"))
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty());
                let currency = row
                    .get("currency")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty());
                let row_portfolio = ["portfolio_id", "portfolio_uuid", "retail_portfolio_id"]
                    .iter()
                    .find_map(|field| row.get(*field).and_then(Value::as_str));
                if id.is_none()
                    || currency.is_none()
                    || row_portfolio != Some(portfolio_id)
                    || row.get("ready").and_then(Value::as_bool) != Some(true)
                    || row.get("active").and_then(Value::as_bool) != Some(true)
                {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_ACCOUNTS_INCOMPLETE",
                        "A native account row omitted or mismatched its UUID, currency, portfolio, or ready/active identity.",
                    ));
                }
                let available = row.get("available_balance");
                let hold = row.get("hold");
                if available
                    .and_then(|v| v.get("currency"))
                    .and_then(Value::as_str)
                    != currency
                    || hold.and_then(|v| v.get("currency")).and_then(Value::as_str) != currency
                    || available
                        .and_then(|v| v.get("value"))
                        .and_then(Value::as_str)
                        .is_none()
                    || hold
                        .and_then(|v| v.get("value"))
                        .and_then(Value::as_str)
                        .is_none()
                {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_ACCOUNTS_INCOMPLETE",
                        "A native account row omitted exact available/hold amounts or their currency denomination.",
                    ));
                }
                rows.push(row.clone());
            }
            match page.get("has_next").and_then(Value::as_bool) {
                Some(false) => {
                    terminated = true;
                    break;
                }
                Some(true) => {
                    let next = page
                        .get("cursor")
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| {
                            failure(
                                "COINBASE_PAGINATION_INVALID",
                                "Native account page declared more data without a cursor.",
                            )
                        })?;
                    if !seen.insert(next.to_owned()) || page_number == 999 {
                        return Err(failure(
                            "COINBASE_PAGINATION_INVALID",
                            "Native account pagination repeated a cursor or exceeded its bound.",
                        ));
                    }
                    cursor = Some(next.to_owned());
                }
                None => {
                    return Err(failure(
                        "COINBASE_PAGINATION_INVALID",
                        "Native account page omitted an explicit has_next boolean.",
                    ));
                }
            }
        }
        if !terminated {
            return Err(failure(
                "COINBASE_PAGINATION_LIMIT",
                "Native account listing exceeded the bounded pagination limit.",
            ));
        }
        Ok(json!({"accounts":rows,"has_next":false}))
    }

    async fn verify_portfolio_empty(
        &self,
        account_name: &str,
        portfolio_id: &str,
    ) -> Result<(Value, usize, usize)> {
        let enrolled = self.account(account_name, CoinbaseProduct::Advanced)?;
        if enrolled.portfolio_id.as_deref() != Some(portfolio_id) {
            return Err(failure(
                "COINBASE_PORTFOLIO_SCOPE_MISMATCH",
                "Portfolio deletion is limited to the exact portfolio bound to this enrolled key.",
            ));
        }
        let detail = self
            .request_for_account(
                account_name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::PortfolioDetail {
                    portfolio_id: portfolio_id.into(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&detail.status) {
            return Err(failure(
                "COINBASE_PORTFOLIO_UNAVAILABLE",
                "The exact portfolio cannot be read before deletion.",
            ));
        }
        let detail_doc: Value = serde_json::from_slice(&detail.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Portfolio detail is malformed.",
            )
        })?;
        let portfolio = detail_doc.get("portfolio").unwrap_or(&detail_doc);
        if portfolio.get("uuid").and_then(Value::as_str) != Some(portfolio_id)
            || portfolio.get("deleted").and_then(Value::as_bool) != Some(false)
        {
            return Err(failure(
                "COINBASE_PORTFOLIO_IDENTITY_MISMATCH",
                "The portfolio ID is absent, already deleted, or ambiguous.",
            ));
        }
        match portfolio.get("type").and_then(Value::as_str) {
            Some("DEFAULT") => {
                return Err(failure(
                    "COINBASE_DEFAULT_PORTFOLIO_PROTECTED",
                    "Coinbase does not permit deleting the key-bound default portfolio.",
                ));
            }
            Some("CONSUMER") => {}
            Some(_) => {
                return Err(failure(
                    "COINBASE_PORTFOLIO_TYPE_UNSUPPORTED",
                    "Only a native consumer portfolio can be deleted through this operation.",
                ));
            }
            None => {
                return Err(failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Native portfolio detail omitted its type, so default protection cannot be verified.",
                ));
            }
        }
        let name = portfolio.get("name").cloned().ok_or_else(|| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "The exact portfolio name is missing from native detail.",
            )
        })?;
        let account_data = self
            .read_advanced_portfolio_accounts(enrolled, portfolio_id)
            .await?;
        let rows = account_data
            .get("accounts")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PORTFOLIO_ACCOUNTS_INCOMPLETE",
                    "The native key-scoped accounts response omitted its account rows.",
                )
            })?;
        let mut account_ids = BTreeSet::new();
        for row in rows {
            let currency = row
                .get("currency")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let account_id = row
                .get("uuid")
                .or_else(|| row.get("id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let row_portfolio = row
                .get("portfolio_id")
                .or_else(|| row.get("portfolio_uuid"))
                .or_else(|| row.get("retail_portfolio_id"))
                .and_then(Value::as_str);
            if currency.is_none()
                || account_id.is_none()
                || !account_ids.insert(account_id.unwrap_or_default().to_owned())
                || row.get("ready").and_then(Value::as_bool) != Some(true)
                || row.get("active").and_then(Value::as_bool) != Some(true)
                || row_portfolio.is_some_and(|value| value != portfolio_id)
            {
                return Err(failure(
                    "COINBASE_PORTFOLIO_ACCOUNTS_INCOMPLETE",
                    "A portfolio account has missing or mismatched currency-account identity/readiness evidence.",
                ));
            }
            let available = row
                .get("available_balance")
                .and_then(|value| value.get("value"))
                .and_then(Value::as_str);
            let available_currency = row
                .get("available_balance")
                .and_then(|value| value.get("currency"))
                .and_then(Value::as_str);
            let hold = row
                .get("hold")
                .and_then(|value| value.get("value"))
                .and_then(Value::as_str);
            let hold_currency = row
                .get("hold")
                .and_then(|value| value.get("currency"))
                .and_then(Value::as_str);
            if available.is_none()
                || hold.is_none()
                || available_currency != currency
                || hold_currency != currency
                || !exact_atoms(available.unwrap_or_default(), 36)?.is_zero()
                || !exact_atoms(hold.unwrap_or_default(), 36)?.is_zero()
            {
                return Err(failure(
                    "COINBASE_PORTFOLIO_NOT_EMPTY",
                    "A portfolio account has a nonzero or inconclusive available balance or hold.",
                ));
            }
        }
        for status in [
            "OPEN",
            "PENDING",
            "QUEUED",
            "PARTIALLY_FILLED",
            "PENDING_CANCEL",
        ] {
            let mut cursor = None;
            let mut seen = BTreeSet::new();
            for page_num in 0..1000 {
                let response = self
                    .request_for_account(
                        account_name,
                        CoinbaseProduct::Advanced,
                        CoinbaseRoute::HistoricalOrders {
                            query: CoinbaseQuery {
                                status: Some(status.into()),
                                cursor: cursor.clone(),
                                limit: Some(250),
                                ..Default::default()
                            },
                        },
                        CoinbaseRequest::get(),
                        None,
                    )
                    .await?;
                if !(200..300).contains(&response.status) {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_ORDERS_UNAVAILABLE",
                        "Open orders cannot be checked before portfolio deletion.",
                    ));
                }
                let page: Value = serde_json::from_slice(&response.body).map_err(|_| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Order history page is malformed.",
                    )
                })?;
                let orders = page
                    .get("orders")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_PORTFOLIO_ORDERS_INCOMPLETE",
                            "Open-order response omitted its native orders array.",
                        )
                    })?;
                if orders.iter().any(|order| {
                    order
                        .get("retail_portfolio_id")
                        .or_else(|| order.get("portfolio_uuid"))
                        .and_then(Value::as_str)
                        .is_some_and(|row_id| row_id != portfolio_id)
                }) {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_ORDERS_INCOMPLETE",
                        "A key-scoped open-order row reported a different portfolio identity.",
                    ));
                }
                if !orders.is_empty() {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_NOT_EMPTY",
                        "The key-bound portfolio has active orders and cannot be deleted.",
                    ));
                }
                match page.get("has_next").and_then(Value::as_bool) {
                    Some(false) => break,
                    Some(true) => {
                        let next = page
                            .get("cursor")
                            .or_else(|| page.get("next_cursor"))
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                failure(
                                    "COINBASE_PAGINATION_INVALID",
                                    "Open-order page omitted its continuation cursor.",
                                )
                            })?
                            .to_owned();
                        if !seen.insert(next.clone()) || page_num == 999 {
                            return Err(failure(
                                "COINBASE_PAGINATION_INVALID",
                                "Open-order scan did not advance safely.",
                            ));
                        }
                        cursor = Some(next);
                    }
                    None => {
                        return Err(failure(
                            "COINBASE_PORTFOLIO_ORDERS_INCOMPLETE",
                            "Open-order page omitted its explicit terminal flag.",
                        ));
                    }
                }
            }
        }
        Ok((name, rows.len(), 0))
    }
    fn ensure_no_unresolved_portfolio_operations(
        &self,
        resource: &gofin_domain::ResourceId,
        portfolio_id: &str,
    ) -> Result<()> {
        let state = self.execution_state.read().map_err(|_| {
            failure("EXECUTION_STATE_UNAVAILABLE", "Protected execution history cannot be read.")
        })?.clone().ok_or_else(|| failure(
            "EXECUTION_STATE_UNAVAILABLE",
            "Portfolio deletion requires the protected Fin journal to check for unresolved operations.",
        ))?;
        let resource = serde_json::to_value(resource).map_err(|_| {
            failure(
                "COINBASE_BINDING_INVALID",
                "The enrolled resource identity cannot be checked in the journal.",
            )
        })?;
        let portfolio_id = Value::String(portfolio_id.to_owned());
        for contract in contracts::CONTRACTS.iter().filter(|contract| {
            contract.product == ContractProduct::Advanced
                && contract.action != contracts::Action::Read
        }) {
            for (pointer, value) in [
                ("/preview/binding/resource", &resource),
                ("/preview/binding/targetResource", &resource),
                ("/preview/binding/portfolioId", &portfolio_id),
                ("/preview/binding/targetPortfolioId", &portfolio_id),
            ] {
                for record in
                    state.operations_by_prepared_binding(contract.operation, pointer, value)?
                {
                    if !record.status.terminal() {
                        return Err(failure(
                            "COINBASE_PORTFOLIO_FIN_ACTIVITY_UNRESOLVED",
                            "The protected Fin journal contains an unresolved operation bound to this portfolio resource.",
                        ));
                    }
                }
            }
        }
        for record in state.operations_by_prepared_binding(
            "coinbase.deposit",
            "/preview/binding/resource",
            &resource,
        )? {
            if !record.status.terminal() {
                return Err(failure(
                    "COINBASE_PORTFOLIO_FIN_ACTIVITY_UNRESOLVED",
                    "The protected Fin journal contains an unresolved Base deposit bound to this Coinbase resource.",
                ));
            }
        }
        Ok(())
    }
    async fn prepare_portfolio_delete(&self, input: &Value) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.delete-portfolio", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Portfolio deletion requires a verified Advanced key-bound identity.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let portfolio_id = input["portfolioId"].as_str().expect("closed schema");
        self.ensure_no_unresolved_portfolio_operations(&account.resource, portfolio_id)?;
        let (previous_name, checked_accounts, checked_orders) = self
            .verify_portfolio_empty(&account.name, portfolio_id)
            .await?;
        let body = String::new();
        Ok(PreparedOperation {
            operation: "coinbase.delete-portfolio".into(),
            input: input.clone(),
            binding: json!({
                "version":1,
                "provider":"coinbase",
                "account":account.name,
                "resource":account.resource,
                "physicalIdentity":account.physical_identity,
                "portfolioId":portfolio_id,
                "requestId":input["requestId"],
                "previousName":previous_name,
                "requestBodyExact":body,
                "configurationRoute":"portfolios_delete",
                "checkedAccountCount":checked_accounts,
                "checkedOpenOrderCount":checked_orders
            }),
            summary: format!("Delete empty Coinbase Advanced portfolio {portfolio_id}."),
            checks: vec![Check {
                name: "empty_enrolled_nondefault_portfolio".into(),
                status: CheckStatus::Passed,
                detail: format!(
                    "The exact enrolled nondefault portfolio is the target; {checked_accounts} account rows were checked for zero available balance and holds, and no active orders were found."
                ),
            }],
            warnings: vec![
                "Deletes the enrolled nondefault portfolio; Coinbase access tied to it may stop. Fin retains local credentials and journal history. A lost response is never replayed; completion requires a native deleted=true readback.".into(),
            ],
            observed_at: gofin_domain::now(),
            expires_at: (Utc::now() + Duration::seconds(45)).to_rfc3339(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![],
                maximum_costs: vec![],
                expected: vec![],
                locks: vec![account.resource.clone()],
            },
            operation_version: 1,
            action_deadline: None,
        })
    }
    async fn prepare_cfm_margin_setting(&self, input: &Value) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.set-cfm-margin-setting", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "CFM configuration requires the verified Advanced account.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let requested = input["setting"].as_str().expect("closed schema");
        let previous = self
            .read(
                "coinbase.read-v3-private-brokerage-cfm-intraday-margin-setting",
                &json!({"account":account.name}),
            )
            .await?;
        let previous = native_cfm_setting(&previous).filter(|s| matches!(*s,"INTRADAY_MARGIN_SETTING_STANDARD"|"INTRADAY_MARGIN_SETTING_INTRADAY"|"INTRADAY_MARGIN_SETTING_UNSPECIFIED")).ok_or_else(|| failure("COINBASE_CFM_SETTING_UNAVAILABLE", "The native current CFM margin setting is not conclusive; configuration is not prepared."))?;
        let body = gofin_domain::canonical_json(&json!({"setting":requested}))?;
        Ok(PreparedOperation {
            operation:"coinbase.set-cfm-margin-setting".into(),input:input.clone(),
            binding:json!({"version":1,"provider":"coinbase","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":account.portfolio_id,"requestId":input["requestId"],"previousSetting":previous,"setting":requested,"requestBodyExact":body}),
            summary:format!("Set Coinbase Futures margin mode to {requested}."),
            checks:vec![Check{name:"key_bound_cfm_configuration".into(),status:CheckStatus::Passed,detail:"Trade permission, exact enrolled account scope, and the current native setting were verified.".into()}],
            warnings:vec!["Margin mode changes affect derivative exposure and require separate configuration approval. A lost response is never replayed; exact native setting readback is required.".into()],
            observed_at:gofin_domain::now(),expires_at:(Utc::now()+Duration::seconds(45)).to_rfc3339(),valuation:None,
            intent:FinancialIntent{debits:vec![],maximum_costs:vec![],expected:vec![],locks:vec![account.resource.clone()]},operation_version:1,action_deadline:None,
        })
    }
    async fn prepare_convert(&self, input: &Value) -> Result<PreparedOperation> {
        let (account, _, input) = self.decode("coinbase.convert", input)?;
        if account.product != CoinbaseProduct::Advanced || !account.identity_verified {
            return Err(failure(
                "COINBASE_IDENTITY_UNVERIFIED",
                "Conversion requires a verified Advanced portfolio identity.",
            ));
        }
        self.verify_advanced_key(account, true, false).await?;
        let from_currency = input["fromCurrency"].as_str().expect("schema");
        let to_currency = input["toCurrency"].as_str().expect("schema");
        let from_id = input["fromAccountId"].as_str().expect("schema");
        let to_id = input["toAccountId"].as_str().expect("schema");
        if from_currency == to_currency || from_id == to_id {
            return Err(failure(
                "COINBASE_CONVERSION_IDENTITY_INVALID",
                "Conversion must bind two distinct currency accounts.",
            ));
        }
        let amount_text = input["amount"].as_str().expect("schema");
        let spend_cap = input["maximumSpend"].as_str().expect("schema");
        let fee_cap = input["maximumFee"].as_str().expect("schema");
        let output_floor = input["minimumReceive"].as_str().expect("schema");
        for value in [amount_text, spend_cap, fee_cap, output_floor] {
            exact_atoms(value, 36)?;
        }
        if !decimal_le(amount_text, spend_cap)? {
            return Err(failure(
                "COINBASE_CONVERSION_BOUND_INVALID",
                "Conversion principal exceeds maximumSpend.",
            ));
        }
        for (account_id, currency) in [(from_id, from_currency), (to_id, to_currency)] {
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Account {
                        account_id: account_id.to_owned(),
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_CURRENCY_ACCOUNT_UNAVAILABLE",
                    "Conversion currency account could not be verified.",
                ));
            }
            let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid currency account data.",
                )
            })?;
            let row = doc.get("account").unwrap_or(&doc);
            // Advanced v3 GET /accounts/{account_id} identifies the account
            // in `uuid`; `id` is used by other Coinbase resource families and
            // a fixture using it can mask a wrong native response shape.
            let returned_id = row.get("uuid").and_then(Value::as_str);
            let returned_currency = row
                .get("currency")
                .and_then(|v| v.get("code").or_else(|| v.get("id")).or(Some(v)))
                .and_then(Value::as_str)
                .or_else(|| row.get("currency_id").and_then(Value::as_str));
            if returned_id != Some(account_id) || returned_currency != Some(currency) {
                return Err(failure(
                    "COINBASE_CURRENCY_ACCOUNT_MISMATCH",
                    "The venue account ID does not match the requested currency identity.",
                ));
            }
            // Account IDs are only meaningful inside the key-bound portfolio.
            // If the native account record reports that scope, require an exact
            // match; likewise never treat an inactive or unready currency
            // account as an available conversion source/target.
            let configured_portfolio = account.portfolio_id.as_deref().ok_or_else(|| {
                failure(
                    "COINBASE_IDENTITY_UNVERIFIED",
                    "Conversion requires a key-bound portfolio UUID.",
                )
            })?;
            for field in ["portfolio_id", "portfolio_uuid", "retail_portfolio_id"] {
                if row
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != configured_portfolio)
                {
                    return Err(failure(
                        "COINBASE_CURRENCY_ACCOUNT_MISMATCH",
                        "The currency account belongs to a different Advanced portfolio.",
                    ));
                }
            }
            if row.get("ready").and_then(Value::as_bool) != Some(true)
                || row.get("active").and_then(Value::as_bool) != Some(true)
            {
                return Err(failure(
                    "COINBASE_CURRENCY_ACCOUNT_UNAVAILABLE",
                    "The source or destination currency account is not ready and active.",
                ));
            }
        }
        let quote_body = gofin_domain::canonical_json(&json!({
            "from_account":from_currency,"to_account":to_currency,"amount":amount_text
        }))?;
        let quote_response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::ConvertQuote,
                CoinbaseRequest::json(quote_body.clone()),
                None,
            )
            .await?;
        if !(200..300).contains(&quote_response.status) {
            return Err(failure(
                "COINBASE_CONVERSION_QUOTE_REJECTED",
                "Coinbase rejected the exact conversion quote request.",
            ));
        }
        let quote_doc: Value = serde_json::from_slice(&quote_response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid conversion quote data.",
            )
        })?;
        let quote = quote_doc.get("trade").unwrap_or(&quote_doc);
        let trade_id = quote
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                failure(
                    "COINBASE_CONVERSION_QUOTE_INCOMPLETE",
                    "Conversion quote omitted its durable trade ID.",
                )
            })?;
        let principal = quote
            .get("user_entered_amount")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_CONVERSION_QUOTE_INCOMPLETE",
                    "Quote omitted source amount.",
                )
            })?;
        let principal_currency = quote["user_entered_amount"]["currency"].as_str();
        let output = quote
            .get("amount")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_CONVERSION_QUOTE_INCOMPLETE",
                    "Quote omitted destination amount.",
                )
            })?;
        let output_currency = quote["amount"]["currency"].as_str();
        let fee = quote
            .get("total_fee")
            .and_then(|v| v.get("amount"))
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_CONVERSION_FEE_UNAVAILABLE",
                    "Quote omitted an exact total fee.",
                )
            })?;
        let fee_currency = quote["total_fee"]["amount"]["currency"].as_str();
        let subtotal = quote
            .get("subtotal")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str);
        let subtotal_currency = quote
            .get("subtotal")
            .and_then(|v| v.get("currency"))
            .and_then(Value::as_str);
        let total = quote
            .get("total")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str);
        let total_currency = quote
            .get("total")
            .and_then(|v| v.get("currency"))
            .and_then(Value::as_str);
        let source_account_id = quote
            .get("source")
            .and_then(|v| v.get("coinbase_account"))
            .and_then(|v| v.get("account_id"))
            .and_then(Value::as_str);
        let target_account_id = quote
            .get("target")
            .and_then(|v| v.get("coinbase_account"))
            .and_then(|v| v.get("account_id"))
            .and_then(Value::as_str);
        let subtotal_matches = match subtotal {
            Some(value) => decimal_le(value, principal)? && decimal_le(principal, value)?,
            None => false,
        };
        let total_matches = match total {
            Some(value) => decimal_sum_equals(principal, fee, value)?,
            None => false,
        };
        if principal_currency != Some(from_currency)
            || output_currency != Some(to_currency)
            || fee_currency != Some(from_currency)
            || subtotal_currency != Some(from_currency)
            || total_currency != Some(from_currency)
            || source_account_id != Some(from_id)
            || target_account_id != Some(to_id)
            || !subtotal_matches
            || !total_matches
            || !decimal_le(principal, amount_text)?
            || !decimal_le(amount_text, principal)?
            || !decimal_le(principal, spend_cap)?
            || !decimal_le(fee, fee_cap)?
            || !decimal_le(output_floor, output)?
        {
            return Err(failure(
                "COINBASE_CONVERSION_BOUND_EXCEEDED",
                "Quote currencies or exact source, output, or fee amounts do not fit the approved bounds.",
            ));
        }
        let expires_at = quote
            .get("expiration_time")
            .and_then(Value::as_str)
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|time| {
                time.with_timezone(&Utc)
                    .min(Utc::now() + Duration::seconds(45))
                    .to_rfc3339()
            })
            .unwrap_or_else(|| (Utc::now() + Duration::seconds(45)).to_rfc3339());
        let from_asset = product_asset(from_currency);
        let to_asset = product_asset(to_currency);
        let amount = exact_atoms(principal, 36)?;
        let fee_amount = exact_atoms(fee, 36)?;
        let principal_cap = exact_atoms(spend_cap, 36)?;
        let output_cap = exact_atoms(output_floor, 36)?;
        let fee_cap_amount = exact_atoms(fee_cap, 36)?;
        let commit_body = gofin_domain::canonical_json(&json!({
            "from_account":from_currency,"to_account":to_currency
        }))?;
        let mut prepared = PreparedOperation {
            operation: "coinbase.convert".into(),
            input: input.clone(),
            binding: json!({"version":1,"provider":"coinbase","account":account.name,"resource":account.resource,"physicalIdentity":account.physical_identity,"portfolioId":account.portfolio_id,"requestId":input["requestId"],"tradeId":trade_id,"fromAccountId":from_id,"fromCurrency":from_currency,"toAccountId":to_id,"toCurrency":to_currency,"amount":principal,"output":output,"fee":fee,"quote":quote,"quoteRequestBodyExact":quote_body,"requestBodyExact":commit_body}),
            summary: format!("Convert {principal} {from_currency} to at least {output_floor} {to_currency}"),
            checks: vec![Check {name:"venue_conversion_quote".into(),status:CheckStatus::Passed,detail:"A live quote attests exact source, destination, output, fee and trade identity.".into()}],
            warnings: vec!["Quote acceptance is not settlement; status and exact account movements are reconciled separately.".into()],
            observed_at:gofin_domain::now(), expires_at,
            valuation:None,
            intent:FinancialIntent {
                debits:vec![ResourceAmount {resource:account.resource.clone(),amount:Amount {asset:from_asset.clone(),atoms:principal_cap.clone()}}],
                maximum_costs:vec![CostBound {location:AccountingLocation::Cash {resource:account.resource.clone()},amount:Amount {asset:from_asset.clone(),atoms:fee_cap_amount},treatment:CostTreatment::AdditionalDebit}],
                expected:vec![EffectConstraint::Conversion {resource:account.resource.clone(),input:from_asset,maximum_input:principal_cap,output:to_asset,minimum_output:output_cap}],
                locks:vec![account.resource.clone()],
            },
            operation_version:1, action_deadline:None,
        };
        // Retain quote fee as an explicit valuation debit if value bounds are
        // introduced by the caller; every amount remains exact decimal text.
        let _ = (amount, fee_amount);
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    async fn advanced_portfolio_contains(
        &self,
        account: &CoinbaseAccount,
        portfolio_id: &str,
    ) -> Result<bool> {
        let response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Portfolios,
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_PORTFOLIOS_UNAVAILABLE",
                "Advanced portfolio membership could not be verified.",
            ));
        }
        let document: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid portfolio data.",
            )
        })?;
        let rows = document
            .get("portfolios")
            .or_else(|| document.get("data"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                failure(
                    "COINBASE_PORTFOLIOS_UNAVAILABLE",
                    "Coinbase portfolio response omitted its portfolio array.",
                )
            })?;
        Ok(rows.iter().any(|row| {
            row.get("uuid")
                .or_else(|| row.get("portfolio_uuid"))
                .or_else(|| row.get("id"))
                .and_then(Value::as_str)
                == Some(portfolio_id)
        }))
    }
    async fn advanced_currency_account_balance(
        &self,
        account: &CoinbaseAccount,
        account_id: &str,
        currency: &str,
    ) -> Result<String> {
        let response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Account {
                    account_id: account_id.to_owned(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_CURRENCY_ACCOUNT_UNAVAILABLE",
                "Advanced currency account could not be verified.",
            ));
        }
        let document: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid currency account data.",
            )
        })?;
        let row = document.get("account").unwrap_or(&document);
        let returned_id = row
            .get("uuid")
            .or_else(|| row.get("id"))
            .and_then(Value::as_str);
        let returned_currency = row
            .get("currency")
            .and_then(|value| {
                value
                    .get("code")
                    .or_else(|| value.get("id"))
                    .or(Some(value))
            })
            .and_then(Value::as_str)
            .or_else(|| row.get("currency_id").and_then(Value::as_str));
        let portfolio_matches = ["portfolio_id", "portfolio_uuid", "retail_portfolio_id"]
            .into_iter()
            .all(|key| {
                row.get(key)
                    .and_then(Value::as_str)
                    .is_none_or(|value| Some(value) == account.portfolio_id.as_deref())
            });
        if returned_id != Some(account_id)
            || returned_currency != Some(currency)
            || !portfolio_matches
            || row.get("ready").and_then(Value::as_bool) != Some(true)
            || row.get("active").and_then(Value::as_bool) != Some(true)
        {
            return Err(failure(
                "COINBASE_CURRENCY_ACCOUNT_MISMATCH",
                "Currency account identity, portfolio, or readiness does not match its enrolled profile.",
            ));
        }
        let available = row
            .get("available_balance")
            .and_then(|value| value.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_BALANCE_UNAVAILABLE",
                    "Currency account omitted exact available balance.",
                )
            })?;
        if row["available_balance"]
            .get("currency")
            .and_then(Value::as_str)
            != Some(currency)
        {
            return Err(failure(
                "COINBASE_BALANCE_IDENTITY_MISMATCH",
                "Available balance denomination does not match the currency account.",
            ));
        }
        exact_atoms(available, 36)?;
        Ok(available.to_owned())
    }
    async fn advanced_currency_account_details(
        &self,
        account: &CoinbaseAccount,
        account_id: &str,
    ) -> Result<Value> {
        let response = self
            .request_for_account(
                &account.name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Account {
                    account_id: account_id.to_owned(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&response.status) {
            return Err(failure(
                "COINBASE_CURRENCY_ACCOUNT_UNAVAILABLE",
                "Advanced currency account could not be verified.",
            ));
        }
        let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid currency account data.",
            )
        })?;
        let row = doc.get("account").unwrap_or(&doc);
        let id = row
            .get("uuid")
            .or_else(|| row.get("id"))
            .and_then(Value::as_str);
        let portfolio_matches = ["portfolio_id", "portfolio_uuid", "retail_portfolio_id"]
            .into_iter()
            .all(|key| {
                row.get(key)
                    .and_then(Value::as_str)
                    .is_none_or(|value| Some(value) == account.portfolio_id.as_deref())
            });
        if id != Some(account_id)
            || !portfolio_matches
            || row.get("ready").and_then(Value::as_bool) != Some(true)
            || row.get("active").and_then(Value::as_bool) != Some(true)
        {
            return Err(failure(
                "COINBASE_CURRENCY_ACCOUNT_MISMATCH",
                "Currency account identity, portfolio, or readiness does not match its enrolled profile.",
            ));
        }
        Ok(row.clone())
    }
    /// App v2 and Advanced v3 expose separate account APIs. A v2 row is only
    /// accepted for an Advanced profile when its exact account UUID intersects
    /// the current key-scoped v3 portfolio account list with the same currency.
    /// Currency-symbol or balance matching alone is never used as a join.
    async fn verify_v2_account_scope(
        &self,
        account: &CoinbaseAccount,
        account_id: &str,
    ) -> Result<()> {
        let portfolio_id = account.portfolio_id.as_deref().ok_or_else(|| {
            failure(
                "COINBASE_PORTFOLIO_MISSING",
                "App account reads require the exact enrolled Advanced portfolio UUID.",
            )
        })?;
        // The legacy account list is only trusted after the current CDP key's
        // view permission still attests this exact Advanced portfolio.
        self.verify_advanced_key(account, false, false).await?;
        let mut cursor: Option<String> = None;
        let mut seen_cursors = BTreeSet::new();
        let mut legacy_matches = Vec::new();
        let mut terminated = false;
        for _ in 0..64 {
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::LegacyAccounts {
                        query: CoinbaseQuery {
                            starting_after: cursor.clone(),
                            limit: Some(100),
                            ..Default::default()
                        },
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_APP_ACCOUNT_UNAVAILABLE",
                    "The key-scoped App v2 account list could not validate this account UUID.",
                ));
            }
            let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid App v2 account data.",
                )
            })?;
            let rows = doc.get("data").and_then(Value::as_array).ok_or_else(|| {
                failure(
                    "COINBASE_APP_ACCOUNT_UNAVAILABLE",
                    "App v2 account response omitted its data array.",
                )
            })?;
            for row in rows {
                if row.get("id").and_then(Value::as_str) == Some(account_id) {
                    legacy_matches.push(row.clone());
                }
            }
            let pagination = doc
                .get("pagination")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_PAGINATION_INVALID",
                        "App v2 account response omitted pagination evidence.",
                    )
                })?;
            match pagination.get("next_starting_after") {
                Some(Value::Null) => {
                    terminated = true;
                    break;
                }
                Some(Value::String(next)) if next.is_empty() => {
                    terminated = true;
                    break;
                }
                Some(Value::String(next)) if next != &cursor.clone().unwrap_or_default() => {
                    if !seen_cursors.insert(next.clone()) {
                        return Err(failure(
                            "COINBASE_PAGINATION_INVALID",
                            "App v2 account pagination repeated a cursor.",
                        ));
                    }
                    cursor = Some(next.clone());
                }
                _ => {
                    return Err(failure(
                        "COINBASE_PAGINATION_INVALID",
                        "App v2 account pagination omitted a valid next cursor or terminal null.",
                    ));
                }
            }
        }
        if !terminated {
            return Err(failure(
                "COINBASE_PAGINATION_LIMIT",
                "App v2 account pagination exceeded the bounded scope-validation limit.",
            ));
        }
        if legacy_matches.len() != 1 {
            return Err(failure(
                "COINBASE_APP_ACCOUNT_IDENTITY_MISMATCH",
                "The exact App v2 account UUID is absent or ambiguous under this key.",
            ));
        }
        let currency = legacy_matches[0]
            .get("currency")
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_APP_ACCOUNT_IDENTITY_MISMATCH",
                    "App v2 account omitted its exact currency code.",
                )
            })?;

        let mut cursor: Option<String> = None;
        let mut seen_cursors = BTreeSet::new();
        let mut matches = Vec::new();
        let mut terminated = false;
        for _ in 0..64 {
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Accounts {
                        query: CoinbaseQuery {
                            portfolio_id: Some(portfolio_id.to_owned()),
                            limit: Some(250),
                            cursor: cursor.clone(),
                            ..Default::default()
                        },
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_ACCOUNTS_UNAVAILABLE",
                    "The key-scoped Advanced account list could not validate this App account.",
                ));
            }
            let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid Advanced account data.",
                )
            })?;
            let rows = doc
                .get("accounts")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_ACCOUNTS_UNAVAILABLE",
                        "Advanced account response omitted its accounts array.",
                    )
                })?;
            for row in rows {
                let id = row
                    .get("uuid")
                    .or_else(|| row.get("id"))
                    .and_then(Value::as_str);
                if id == Some(account_id) {
                    matches.push(row.clone());
                }
            }
            match doc.get("has_next").and_then(Value::as_bool) {
                Some(false) => {
                    terminated = true;
                    break;
                }
                Some(true) => {
                    let next = doc
                        .get("cursor")
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| {
                            failure(
                                "COINBASE_PAGINATION_INVALID",
                                "Advanced account page declared a next page without a cursor.",
                            )
                        })?;
                    if !seen_cursors.insert(next.to_owned()) {
                        return Err(failure(
                            "COINBASE_PAGINATION_INVALID",
                            "Advanced account pagination repeated a cursor.",
                        ));
                    }
                    cursor = Some(next.to_owned());
                }
                None => {
                    return Err(failure(
                        "COINBASE_PAGINATION_INVALID",
                        "Advanced account page omitted an explicit has_next boolean.",
                    ));
                }
            }
        }
        if !terminated {
            return Err(failure(
                "COINBASE_PAGINATION_LIMIT",
                "Advanced account pagination exceeded the bounded scope-validation limit.",
            ));
        }
        if matches.len() != 1
            || matches[0].get("currency").and_then(Value::as_str) != Some(currency)
            || matches[0].get("active").and_then(Value::as_bool) != Some(true)
            || matches[0].get("ready").and_then(Value::as_bool) != Some(true)
            || ["portfolio_id", "portfolio_uuid", "retail_portfolio_id"]
                .into_iter()
                .any(|key| {
                    matches[0]
                        .get(key)
                        .and_then(Value::as_str)
                        .is_some_and(|id| id != portfolio_id)
                })
        {
            return Err(failure(
                "COINBASE_APP_ACCOUNT_SCOPE_MISMATCH",
                "App v2 account UUID does not exactly intersect one active, ready account in the enrolled Advanced portfolio.",
            ));
        }
        Ok(())
    }
    async fn prepare_portfolio_move(&self, input: &Value) -> Result<PreparedOperation> {
        let (source, _, input) = self.decode("coinbase.move-portfolio-funds", input)?;
        let target_name = input["targetAccount"].as_str().expect("schema");
        let target = self.account(target_name, CoinbaseProduct::Advanced)?;
        if !source.identity_verified
            || !target.identity_verified
            || source.name == target.name
            || source.physical_identity == target.physical_identity
        {
            return Err(failure(
                "COINBASE_PORTFOLIO_IDENTITY_INVALID",
                "Portfolio movement requires two distinct verified Advanced portfolio identities.",
            ));
        }
        let source_portfolio = source.portfolio_id.as_deref().ok_or_else(|| {
            failure(
                "COINBASE_PORTFOLIO_MISSING",
                "Source portfolio is missing its permissioned UUID.",
            )
        })?;
        let target_portfolio = target.portfolio_id.as_deref().ok_or_else(|| {
            failure(
                "COINBASE_PORTFOLIO_MISSING",
                "Target portfolio is missing its enrolled UUID.",
            )
        })?;
        if source_portfolio == target_portfolio {
            return Err(failure(
                "COINBASE_PORTFOLIO_IDENTITY_INVALID",
                "Source and destination portfolios must be different.",
            ));
        }
        self.verify_advanced_key(source, false, true).await?;
        self.verify_advanced_key(target, false, false).await?;
        if !self
            .advanced_portfolio_contains(source, source_portfolio)
            .await?
            || !self
                .advanced_portfolio_contains(source, target_portfolio)
                .await?
        {
            return Err(failure(
                "COINBASE_PORTFOLIO_NOT_VISIBLE",
                "The source key cannot verify both enrolled portfolios as owned by this Coinbase user.",
            ));
        }
        let currency = input["currency"].as_str().expect("schema");
        let source_account_id = input["sourceCurrencyAccountId"].as_str().expect("schema");
        let target_account_id = input["targetCurrencyAccountId"].as_str().expect("schema");
        let source_balance = self
            .advanced_currency_account_balance(source, source_account_id, currency)
            .await?;
        let target_balance = self
            .advanced_currency_account_balance(target, target_account_id, currency)
            .await?;
        let amount = input["amount"].as_str().expect("schema");
        let minimum = input["minimumAmount"].as_str().expect("schema");
        let maximum = input["maximumAmount"].as_str().expect("schema");
        for value in [amount, minimum, maximum] {
            exact_atoms(value, 36)?;
        }
        if !decimal_le(minimum, amount)?
            || !decimal_le(amount, maximum)?
            || !decimal_le(amount, &source_balance)?
        {
            return Err(failure(
                "COINBASE_TRANSFER_BOUND_INVALID",
                "Transfer amount must fit its exact min/max bounds and the verified available source balance.",
            ));
        }
        let asset = product_asset(currency);
        let source_resource = source.resource.clone();
        let target_resource = target.resource.clone();
        let body = gofin_domain::canonical_json(&json!({
            "funds":{"value":amount,"currency":currency},
            "source_portfolio_uuid":source_portfolio,
            "target_portfolio_uuid":target_portfolio
        }))?;
        let mut prepared = PreparedOperation {
            operation: "coinbase.move-portfolio-funds".into(),
            input: input.clone(),
            binding: json!({
                "version":1,"provider":"coinbase","account":source.name,"targetAccount":target.name,
                "resource":source.resource,"targetResource":target.resource,
                "physicalIdentity":source.physical_identity,"targetPhysicalIdentity":target.physical_identity,
                "portfolioId":source_portfolio,"targetPortfolioId":target_portfolio,
                "sourceCurrencyAccountId":source_account_id,"targetCurrencyAccountId":target_account_id,
                "currency":currency,"sourceBalanceBefore":source_balance,"targetBalanceBefore":target_balance,
                "requestId":input["requestId"],"requestBodyExact":body
            }),
            summary: format!("Move {amount} {currency} from {source_portfolio} to {target_portfolio}"),
            checks: vec![Check {name:"portfolio_move_scope".into(),status:CheckStatus::Passed,detail:"Source key has transfer permission; both enrolled portfolio identities and both currency accounts were verified.".into()}],
            warnings: vec!["Coinbase returns no idempotency key for portfolio moves. Uncertain dispatches are reconciled from the captured receipt and exact source/destination balance deltas; they are never replayed.".into()],
            observed_at:gofin_domain::now(),expires_at:(Utc::now()+Duration::seconds(45)).to_rfc3339(),valuation:None,
            intent:FinancialIntent {
                debits:vec![ResourceAmount {resource:source_resource.clone(),amount:Amount {asset:asset.clone(),atoms:exact_atoms(maximum,36)?}}],
                maximum_costs:vec![],
                expected:vec![EffectConstraint::Transfer {
                    from:source_resource.clone(),to:target_resource.clone(),debit_asset:asset.clone(),credit_asset:asset.clone(),
                    maximum_debit:exact_atoms(maximum,36)?,minimum_credit:exact_atoms(minimum,36)?
                }],
                locks:vec![source_resource,target_resource],
            },
            operation_version:1,action_deadline:None,
        };
        gofin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    async fn advanced_cash_snapshot(
        &self,
        account: &CoinbaseAccount,
        scope: &SnapshotScope,
    ) -> Result<StateSnapshot> {
        self.verify_advanced_key(account, false, false).await?;
        let mut currency_ids = BTreeSet::<String>::new();
        for (endpoint, label) in [
            (CoinbaseRoute::LegacyCurrencies, "fiat"),
            (CoinbaseRoute::LegacyCryptoCurrencies, "crypto"),
        ] {
            let currencies = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    endpoint,
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&currencies.status) {
                return Err(failure(
                    "COINBASE_CURRENCIES_UNAVAILABLE",
                    "Fiat and crypto currency identity metadata must both be available.",
                ));
            }
            let currency_doc: Value = serde_json::from_slice(&currencies.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid currency metadata.",
                )
            })?;
            let currency_rows = currency_doc
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_CURRENCIES_UNAVAILABLE",
                        "Coinbase App currency response omitted its data array.",
                    )
                })?;
            for row in currency_rows {
                let Some(code) = row
                    .get("id")
                    .or_else(|| row.get("code"))
                    .and_then(Value::as_str)
                else {
                    return Err(failure(
                        "COINBASE_CURRENCY_IDENTITY_MISSING",
                        &format!(
                            "Coinbase {label} currency metadata contains a row without an exact ID."
                        ),
                    ));
                };
                currency_ids.insert(code.to_owned());
            }
        }
        let (mut cursor, mut seen) = (None, BTreeSet::new());
        let (mut holdings, mut account_ids) = (Vec::new(), Vec::new());
        let mut observed_currency_codes = BTreeSet::new();
        let mut held_balances = Vec::new();
        let mut in_flight = Vec::new();
        for page_number in 0..1000 {
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Accounts {
                        query: CoinbaseQuery {
                            cursor: cursor.clone(),
                            limit: Some(250),
                            portfolio_id: Some(account.portfolio_id.clone().ok_or_else(|| {
                                failure(
                                    "COINBASE_PORTFOLIO_MISSING",
                                    "Advanced cash observation requires the key-bound portfolio UUID.",
                                )
                            })?),
                            ..Default::default()
                        },
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_ACCOUNTS_UNAVAILABLE",
                    "Advanced balances could not be observed.",
                ));
            }
            let page: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid account balances.",
                )
            })?;
            let rows = page
                .get("accounts")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Advanced account page omitted accounts.",
                    )
                })?;
            for row in rows {
                let configured_portfolio = account.portfolio_id.as_deref().ok_or_else(|| {
                    failure(
                        "COINBASE_PORTFOLIO_MISSING",
                        "Advanced cash observation requires the key-bound portfolio UUID.",
                    )
                })?;
                // Account responses do not always echo portfolio scope. If they do,
                // require it to agree with the enrolled key-bound identity. The legacy
                // retail_portfolio_id query is only a narrowing filter; it cannot
                // establish or override the key's effective portfolio.
                for key in ["portfolio_id", "portfolio_uuid", "retail_portfolio_id"] {
                    if row
                        .get(key)
                        .and_then(Value::as_str)
                        .is_some_and(|returned| returned != configured_portfolio)
                    {
                        return Err(failure(
                            "COINBASE_PORTFOLIO_MISMATCH",
                            "Advanced account belongs to a different portfolio than the enrolled key identity.",
                        ));
                    }
                }
                let code = row.get("currency").and_then(Value::as_str).ok_or_else(|| {
                    failure(
                        "COINBASE_ACCOUNT_IDENTITY_MISSING",
                        "Advanced account omitted its currency identity.",
                    )
                })?;
                observed_currency_codes.insert(code.to_owned());
                let asset = gofin_domain::AssetId {
                    network: "coinbase".into(),
                    reference: code.into(),
                    decimals: 36,
                };
                // Portfolio account pagination can include assets outside the
                // caller's exact requested cash scope (for example staked or
                // legacy assets). They must not block a scoped observation.
                if !scope.matches_asset(&asset) {
                    continue;
                }
                let id = row
                    .get("uuid")
                    .or_else(|| row.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_ACCOUNT_IDENTITY_MISSING",
                            "Advanced account omitted its currency-account UUID.",
                        )
                    })?;
                if row.get("ready").and_then(Value::as_bool) != Some(true)
                    || row.get("active").and_then(Value::as_bool) != Some(true)
                {
                    return Err(failure(
                        "COINBASE_ACCOUNT_NOT_READY",
                        "Coinbase currency account is not explicitly active and ready.",
                    ));
                }
                if !currency_ids.contains(code) {
                    return Err(failure(
                        "COINBASE_CURRENCY_UNKNOWN",
                        "Account currency is absent from Coinbase currency metadata.",
                    ));
                }
                let available_balance = row.get("available_balance").ok_or_else(|| {
                    failure(
                        "COINBASE_BALANCE_INVALID",
                        "Advanced account omitted its available balance object.",
                    )
                })?;
                if available_balance.get("currency").and_then(Value::as_str) != Some(code) {
                    return Err(failure(
                        "COINBASE_BALANCE_IDENTITY_MISMATCH",
                        "Available balance denomination differs from the account currency.",
                    ));
                }
                let available = row
                    .get("available_balance")
                    .and_then(|v| v.get("value"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_BALANCE_INVALID",
                            "Advanced account omitted its exact available balance.",
                        )
                    })?;
                let amount = exact_atoms(available, 36)?;
                if let Some(hold_object) = row.get("hold") {
                    // Holds are not spendable cash; preserve their independent
                    // observation in coverage while using available_balance only.
                    if hold_object.get("currency").and_then(Value::as_str) != Some(code) {
                        return Err(failure(
                            "COINBASE_BALANCE_IDENTITY_MISMATCH",
                            "Hold denomination differs from the account currency.",
                        ));
                    }
                    let hold = hold_object
                        .get("value")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            failure(
                                "COINBASE_BALANCE_INVALID",
                                "Coinbase hold object omitted its exact value.",
                            )
                        })?;
                    let hold = exact_atoms(hold, 36)?;
                    held_balances.push(format!("coinbase:currency-account:{id}:hold:{hold}"));
                }
                // State presence distinguishes proven zero from unobserved.
                holdings.push(ResourceAmount {
                    resource: account.resource.clone(),
                    amount: Amount {
                        asset,
                        atoms: amount,
                    },
                });
                account_ids.push(id.to_owned());
            }
            match page.get("has_next").and_then(Value::as_bool) {
                Some(false) => break,
                Some(true) => {}
                None => {
                    return Err(failure(
                        "COINBASE_PAGINATION_INVALID",
                        "Advanced account page omitted an explicit has_next boolean.",
                    ));
                }
            }
            let next = page
                .get("cursor")
                .or_else(|| page.get("next_cursor"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_PAGINATION_INVALID",
                        "Advanced accounts omitted a continuation cursor.",
                    )
                })?
                .to_owned();
            if !seen.insert(next.clone()) {
                return Err(failure(
                    "COINBASE_PAGINATION_INVALID",
                    "Advanced accounts returned a repeated cursor.",
                ));
            }
            if page_number == 999 {
                return Err(failure(
                    "COINBASE_PAGINATION_LIMIT",
                    "Advanced account page limit was reached before pagination completed.",
                ));
            }
            cursor = Some(next);
        }
        account_ids.sort();
        account_ids.dedup();
        let mut absent_scoped_assets = Vec::new();
        for asset in &scope.assets {
            if asset.network != "coinbase" || asset.decimals != 36 {
                continue;
            }
            if !currency_ids.contains(&asset.reference) {
                return Err(failure(
                    "COINBASE_CURRENCY_UNKNOWN",
                    "A requested cash asset is absent from Coinbase fiat and crypto currency metadata.",
                ));
            }
            if !observed_currency_codes.contains(&asset.reference) {
                absent_scoped_assets.push(asset.reference.clone());
                holdings.push(ResourceAmount {
                    resource: account.resource.clone(),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: Quantity::zero(),
                    },
                });
            }
        }
        let (mut order_cursor, mut order_seen) = (None, BTreeSet::new());
        for page_number in 0..1000 {
            let response = self
                .request_for_account(
                    &account.name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::HistoricalOrders {
                        query: CoinbaseQuery {
                            cursor: order_cursor.clone(),
                            limit: Some(250),
                            status: Some("OPEN".into()),
                            ..Default::default()
                        },
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_ORDERS_UNAVAILABLE",
                    "Open Advanced orders could not be observed.",
                ));
            }
            let page: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid open orders.",
                )
            })?;
            let rows = page
                .get("orders")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Open-order page omitted orders.",
                    )
                })?;
            for row in rows {
                let id = row
                    .get("order_id")
                    .or_else(|| row.get("orderId"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_ORDER_IDENTITY_MISSING",
                            "Open order omitted its venue identity.",
                        )
                    })?;
                in_flight.push(format!("coinbase:order:{id}"));
            }
            match page.get("has_next").and_then(Value::as_bool) {
                Some(false) => break,
                Some(true) => {}
                None => {
                    return Err(failure(
                        "COINBASE_PAGINATION_INVALID",
                        "Open-order page omitted an explicit has_next boolean.",
                    ));
                }
            }
            let next = page
                .get("cursor")
                .or_else(|| page.get("next_cursor"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    failure(
                        "COINBASE_PAGINATION_INVALID",
                        "Open orders omitted a continuation cursor.",
                    )
                })?
                .to_owned();
            if !order_seen.insert(next.clone()) {
                return Err(failure(
                    "COINBASE_PAGINATION_INVALID",
                    "Open orders returned a repeated cursor.",
                ));
            }
            if page_number == 999 {
                return Err(failure(
                    "COINBASE_PAGINATION_LIMIT",
                    "Open-order page limit was reached before pagination completed.",
                ));
            }
            order_cursor = Some(next);
        }
        let mut coverage=vec![account.resource.0.clone(), "Advanced account pages fully traversed; spendable holdings use available_balance at a fixed 36-decimal accounting scale.".into()];
        coverage.extend(held_balances);
        coverage.extend(
            account_ids
                .into_iter()
                .map(|id| format!("coinbase:currency-account:{id}")),
        );
        coverage.extend(absent_scoped_assets.into_iter().map(|currency| {
            format!("coinbase:currency-account-absent:{currency}:zero-available-after-complete-enumeration")
        }));
        let mut pending = Vec::new();
        let scoped_to_cash = !scope.is_full()
            && (!scope.networks.is_empty() || !scope.assets.is_empty())
            && scope.networks.iter().all(|network| network == "coinbase")
            && scope.assets.iter().all(|asset| asset.network == "coinbase");
        let complete = scoped_to_cash;
        if !complete {
            pending.push("Snapshot covers Advanced cash only; Exchange and International balances are not included.".into());
        }
        Ok(StateSnapshot {
            state: FinancialState {
                holdings,
                positions: vec![],
                authorities: vec![],
                in_flight,
            },
            observed_at: gofin_domain::now(),
            complete,
            coverage,
            pending,
        })
    }
    async fn reconcile_portfolio_move(
        &self,
        record: &OperationRecord,
        submission: &gofin_domain::Submission,
    ) -> Result<Reconciliation> {
        let unknown = |reason: &str| Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":reason,"replay":"forbidden_without_conclusive_reconciliation"}),
            verified: None,
        };
        let binding = &submission.data["binding"];
        let Some(prepared) = record
            .preview
            .as_ref()
            .filter(|p| p.operation == "coinbase.move-portfolio-funds" && p.binding == *binding)
        else {
            return Ok(unknown(
                "persisted portfolio move binding is missing or differs",
            ));
        };
        if record.submission.as_ref() != Some(submission)
            || submission.kind != "coinbase"
            || submission.reference
                != client_order_id(
                    binding["physicalIdentity"].as_str().unwrap_or(""),
                    &format!(
                        "portfolio-move:{}",
                        prepared.input["requestId"].as_str().unwrap_or("")
                    ),
                )
        {
            return Ok(unknown(
                "durable submission identity differs from the prepared move",
            ));
        }
        let (
            Some(source_name),
            Some(target_name),
            Some(currency),
            Some(source_portfolio),
            Some(target_portfolio),
            Some(source_id),
            Some(target_id),
            Some(amount),
            Some(before_source),
            Some(before_target),
        ) = (
            binding["account"].as_str(),
            binding["targetAccount"].as_str(),
            binding["currency"].as_str(),
            binding["portfolioId"].as_str(),
            binding["targetPortfolioId"].as_str(),
            binding["sourceCurrencyAccountId"].as_str(),
            binding["targetCurrencyAccountId"].as_str(),
            prepared.input["amount"].as_str(),
            binding["sourceBalanceBefore"].as_str(),
            binding["targetBalanceBefore"].as_str(),
        )
        else {
            return Ok(unknown(
                "prepared source/destination identity or balance baseline is incomplete",
            ));
        };
        // An authoritative dispatch receipt is required. Balance deltas alone
        // cannot attribute concurrent transfers to this request.
        let Some(dispatch) = record.dispatch_evidence.as_ref() else {
            return Ok(unknown(
                "no captured native response; portfolio move was not replayed",
            ));
        };
        let (Some(encoded), Some(expected_hash), Some(http_status)) = (
            dispatch["bodyHex"].as_str(),
            dispatch["bodySha256"].as_str(),
            dispatch["httpStatus"].as_u64(),
        ) else {
            return Ok(unknown("captured portfolio move response is incomplete"));
        };
        let bytes = hex::decode(encoded).map_err(|_| {
            failure(
                "COINBASE_EVIDENCE_INVALID",
                "Persisted portfolio move evidence is malformed.",
            )
        })?;
        if hex::encode(Sha256::digest(&bytes)) != expected_hash {
            return Ok(unknown("captured response digest does not match"));
        }
        if !(200..300).contains(&(http_status as u16)) {
            return Ok(unknown(
                "native response did not acknowledge the portfolio move",
            ));
        }
        let response: Value = serde_json::from_slice(&bytes).map_err(|_| {
            failure(
                "COINBASE_EVIDENCE_INVALID",
                "Persisted portfolio move receipt is invalid JSON.",
            )
        })?;
        if response["source_portfolio_uuid"].as_str() != Some(source_portfolio)
            || response["target_portfolio_uuid"].as_str() != Some(target_portfolio)
        {
            return Ok(unknown(
                "native response does not attest the bound source and target portfolios",
            ));
        }
        let source_cfg = match self.account(source_name, CoinbaseProduct::Advanced) {
            Ok(a) => a,
            Err(_) => return Ok(unknown("source account is no longer enrolled")),
        };
        let target_cfg = match self.account(target_name, CoinbaseProduct::Advanced) {
            Ok(a) => a,
            Err(_) => return Ok(unknown("target account is no longer enrolled")),
        };
        if serde_json::to_value(&source_cfg.resource).ok().as_ref() != Some(&binding["resource"])
            || serde_json::to_value(&target_cfg.resource).ok().as_ref()
                != Some(&binding["targetResource"])
            || source_cfg.physical_identity != binding["physicalIdentity"].as_str().unwrap_or("")
            || target_cfg.physical_identity
                != binding["targetPhysicalIdentity"].as_str().unwrap_or("")
            || source_cfg.portfolio_id.as_deref() != Some(source_portfolio)
            || target_cfg.portfolio_id.as_deref() != Some(target_portfolio)
        {
            return Ok(unknown(
                "current enrolled account identities differ from the persisted portfolio bindings",
            ));
        }
        if self
            .verify_advanced_key(source_cfg, false, true)
            .await
            .is_err()
            || self
                .verify_advanced_key(target_cfg, false, false)
                .await
                .is_err()
        {
            return Ok(unknown(
                "current key permissions no longer attest both enrolled portfolios",
            ));
        }
        let after_source = match self
            .advanced_currency_account_balance(source_cfg, source_id, currency)
            .await
        {
            Ok(v) => v,
            Err(_) => return Ok(unknown("source currency account is unavailable")),
        };
        let after_target = match self
            .advanced_currency_account_balance(target_cfg, target_id, currency)
            .await
        {
            Ok(v) => v,
            Err(_) => return Ok(unknown("destination currency account is unavailable")),
        };
        let exact_source = decimal_delta_equals(before_source, &after_source, amount, true)?;
        let exact_target = decimal_delta_equals(before_target, &after_target, amount, false)?;
        if !(exact_source && exact_target) {
            return Ok(unknown(
                "both exact source debit and destination credit deltas are not proven",
            ));
        }
        let asset = product_asset(currency);
        let debit = Amount {
            asset: asset.clone(),
            atoms: exact_atoms(amount, 36)?,
        };
        let credit = Amount {
            asset,
            atoms: exact_atoms(amount, 36)?,
        };
        let evidence = json!({"sourcePortfolio":source_portfolio,"targetPortfolio":target_portfolio,"currency":currency,"amount":amount,"sourceBalanceBefore":before_source,"sourceBalanceAfter":after_source,"targetBalanceBefore":before_target,"targetBalanceAfter":after_target,"nativeReceipt":response});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: vec![FinancialEffect::Transfer {
                from: source_cfg.resource.clone(),
                to: target_cfg.resource.clone(),
                debit,
                credit,
            }],
            evidence: evidence.clone(),
            observed_at: gofin_domain::now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence,
            verified: Some(verified),
        })
    }
    async fn reconcile_conversion(
        &self,
        record: &OperationRecord,
        submission: &gofin_domain::Submission,
    ) -> Result<Reconciliation> {
        let binding = &submission.data["binding"];
        let Some(prepared) = record
            .preview
            .as_ref()
            .filter(|p| p.operation == "coinbase.convert" && p.binding == *binding)
        else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"conversion journal differs from the persisted quote binding"}),
                verified: None,
            });
        };
        let (Some(account), Some(trade_id), Some(from_currency), Some(to_currency)) = (
            binding["account"].as_str(),
            binding["tradeId"].as_str(),
            binding["fromCurrency"].as_str(),
            binding["toCurrency"].as_str(),
        ) else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"conversion submission identity is incomplete"}),
                verified: None,
            });
        };
        if submission.kind != "coinbase" || submission.reference != trade_id {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"conversion submission does not match its immutable trade ID"}),
                verified: None,
            });
        }
        let Some(account_config) = self
            .accounts
            .iter()
            .find(|a| a.name == account && a.product == CoinbaseProduct::Advanced)
        else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"persisted Advanced account is no longer enrolled"}),
                verified: None,
            });
        };
        if self
            .verify_advanced_key(account_config, false, false)
            .await
            .is_err()
        {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"current key permissions no longer attest the persisted portfolio"}),
                verified: None,
            });
        }
        let response = self
            .request_for_account(
                account,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::ConvertTradeExact {
                    trade_id: trade_id.to_owned(),
                    from_account: from_currency.to_owned(),
                    to_account: to_currency.to_owned(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await;
        let Ok(response) = response else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"tradeId":trade_id,"reason":"conversion status is unavailable; do not replay commit"}),
                verified: None,
            });
        };
        if !(200..300).contains(&response.status) {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"tradeId":trade_id,"reason":"conversion status is unavailable; do not replay commit"}),
                verified: None,
            });
        }
        let document: Value = serde_json::from_slice(&response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid conversion reconciliation data.",
            )
        })?;
        let trade = document.get("trade").unwrap_or(&document);
        if trade.get("id").and_then(Value::as_str) != Some(trade_id) {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"tradeId":trade_id,"reason":"conversion status returned a different trade identity"}),
                verified: None,
            });
        }
        let status_text = trade
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(
            status_text.as_str(),
            "TRADE_STATUS_PENDING"
                | "PENDING"
                | "TRADE_STATUS_CREATED"
                | "CREATED"
                | "TRADE_STATUS_PROCESSING"
                | "PROCESSING"
        ) {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"tradeId":trade_id,"venueStatus":status_text}),
                verified: None,
            });
        }
        if !matches!(status_text.as_str(), "TRADE_STATUS_COMPLETED" | "COMPLETED") {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"tradeId":trade_id,"venueStatus":status_text,"reason":"conversion status does not prove terminal zero effect; retain reservation and do not replay"}),
                verified: None,
            });
        }
        let input_amount = trade
            .get("user_entered_amount")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_CONVERSION_INCOMPLETE",
                    "Completed conversion omitted exact source amount.",
                )
            })?;
        let input_currency = trade["user_entered_amount"]["currency"].as_str();
        let output_amount = trade
            .get("amount")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_CONVERSION_INCOMPLETE",
                    "Completed conversion omitted exact output amount.",
                )
            })?;
        let output_currency = trade["amount"]["currency"].as_str();
        let fee = trade
            .get("total_fee")
            .and_then(|v| v.get("amount"))
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "COINBASE_CONVERSION_INCOMPLETE",
                    "Completed conversion omitted exact total fee.",
                )
            })?;
        let fee_currency = trade["total_fee"]["amount"]["currency"].as_str();
        let source_account = trade
            .get("source")
            .and_then(|v| v.get("coinbase_account"))
            .and_then(|v| v.get("account_id"))
            .and_then(Value::as_str);
        let target_account = trade
            .get("target")
            .and_then(|v| v.get("coinbase_account"))
            .and_then(|v| v.get("account_id"))
            .and_then(Value::as_str);
        let subtotal = trade
            .get("subtotal")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str);
        let subtotal_currency = trade
            .get("subtotal")
            .and_then(|v| v.get("currency"))
            .and_then(Value::as_str);
        let total = trade
            .get("total")
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str);
        let total_currency = trade
            .get("total")
            .and_then(|v| v.get("currency"))
            .and_then(Value::as_str);
        let subtotal_matches = match subtotal {
            Some(value) => decimal_le(value, input_amount)? && decimal_le(input_amount, value)?,
            None => false,
        };
        let total_matches = match total {
            Some(value) => decimal_sum_equals(input_amount, fee, value)?,
            None => false,
        };
        if input_currency != Some(from_currency)
            || output_currency != Some(to_currency)
            || fee_currency != Some(from_currency)
            || source_account != binding["fromAccountId"].as_str()
            || target_account != binding["toAccountId"].as_str()
            || subtotal_currency != Some(from_currency)
            || total_currency != Some(from_currency)
            || !decimal_le(
                input_amount,
                prepared.binding["amount"].as_str().unwrap_or("0"),
            )?
            || !decimal_le(
                prepared.binding["amount"].as_str().unwrap_or("0"),
                input_amount,
            )?
            || !subtotal_matches
            || !total_matches
        {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"tradeId":trade_id,"reason":"conversion currencies differ from bound account identities","trade":trade}),
                verified: None,
            });
        }
        let spent = exact_atoms(input_amount, 36)?;
        let received = exact_atoms(output_amount, 36)?;
        let fee_amount = exact_atoms(fee, 36)?;
        let resource = account_config.resource.clone();
        let mut effects = vec![FinancialEffect::Conversion {
            resource: resource.clone(),
            spent: Amount {
                asset: product_asset(from_currency),
                atoms: spent.clone(),
            },
            received: Amount {
                asset: product_asset(to_currency),
                atoms: received.clone(),
            },
        }];
        if !fee_amount.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash { resource },
                amount: Amount {
                    asset: product_asset(from_currency),
                    atoms: fee_amount.clone(),
                },
                category: "coinbase-conversion-fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        let cap_spend = exact_atoms(prepared.input["maximumSpend"].as_str().unwrap_or("0"), 36)?;
        let cap_fee = exact_atoms(prepared.input["maximumFee"].as_str().unwrap_or("0"), 36)?;
        let floor = exact_atoms(prepared.input["minimumReceive"].as_str().unwrap_or("0"), 36)?;
        let within = spent <= cap_spend && fee_amount <= cap_fee && received >= floor;
        let status = if within {
            Status::Completed
        } else {
            Status::Failed
        };
        let evidence = json!({"tradeId":trade_id,"venueStatus":status_text,"trade":trade,"realizedSpend":spent,"realizedFee":fee_amount,"realizedOutput":received,"withinAuthorizedBounds":within});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects,
            evidence: evidence.clone(),
            observed_at: gofin_domain::now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status,
            evidence,
            verified: Some(verified),
        })
    }
    fn connection(account: &CoinbaseAccount) -> ConnectionMetadata {
        ConnectionMetadata {
            name: account.name.clone(),
            provider: account.product.provider().into(),
            resource: account.resource.clone(),
            role: Some("coinbase_account".into()),
            parent: None,
            network: Some(account.product.provider().into()),
            address: None,
            credential_ref: Some(account.credential_ref.clone()),
            signing_enabled: false,
            details: json!({"physical_identity":account.physical_identity,"identity_verified":account.identity_verified,"portfolio_id":account.portfolio_id,"product":account.product}),
        }
    }
}
fn canonical_identity(value: &str) -> String {
    uuid::Uuid::parse_str(value)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| value.trim().to_owned())
}
fn product_asset(currency_id: &str) -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: "coinbase".into(),
        reference: currency_id.to_owned(),
        decimals: 36,
    }
}
pub(super) fn exact_atoms(value: &str, decimals: u8) -> Result<Quantity> {
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fractional.bytes().all(|b| b.is_ascii_digit())
        || fractional.len() > decimals as usize
    {
        return Err(failure(
            "COINBASE_DECIMAL_INVALID",
            "Financial amount exceeds the exact currency precision.",
        ));
    }
    Quantity::new(format!(
        "{}{}{}",
        whole,
        fractional,
        "0".repeat(decimals as usize - fractional.len())
    ))
}
pub(super) fn decimal_product_atoms_exact(
    left: &str,
    right: &str,
    decimals: u8,
) -> Result<Quantity> {
    let left_scale = left.split_once('.').map_or(0, |(_, f)| f.len());
    let right_scale = right.split_once('.').map_or(0, |(_, f)| f.len());
    let product = decimal_scaled(left, left_scale)? * decimal_scaled(right, right_scale)?;
    let product_scale = left_scale + right_scale;
    let target_scale = decimals as usize;
    let atoms = if product_scale > target_scale {
        let divisor = num_bigint::BigUint::from(10_u8).pow((product_scale - target_scale) as u32);
        if &product % &divisor != num_bigint::BigUint::from(0u8) {
            return Err(failure(
                "COINBASE_AMOUNT_NOT_EXACT",
                "Fill quote value cannot be represented exactly at the shared accounting scale.",
            ));
        }
        product / divisor
    } else {
        product * num_bigint::BigUint::from(10_u8).pow((target_scale - product_scale) as u32)
    };
    Quantity::new(atoms.to_str_radix(10))
}
fn decimal_scaled(value: &str, scale: usize) -> Result<num_bigint::BigUint> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > scale
        || scale > 80
    {
        return Err(failure(
            "COINBASE_DECIMAL_INVALID",
            "Coinbase returned an invalid exact decimal.",
        ));
    }
    num_bigint::BigUint::parse_bytes(
        format!(
            "{}{}{}",
            whole,
            fraction,
            "0".repeat(scale - fraction.len())
        )
        .as_bytes(),
        10,
    )
    .ok_or_else(|| {
        failure(
            "COINBASE_DECIMAL_INVALID",
            "Coinbase returned an invalid exact decimal.",
        )
    })
}
pub(super) fn decimal_le(left: &str, right: &str) -> Result<bool> {
    let scale = left
        .split_once('.')
        .map_or(0, |(_, f)| f.len())
        .max(right.split_once('.').map_or(0, |(_, f)| f.len()));
    Ok(decimal_scaled(left, scale)? <= decimal_scaled(right, scale)?)
}
pub(super) fn decimal_product_le(left: &str, right: &str, cap: &str) -> Result<bool> {
    let left_scale = left.split_once('.').map_or(0, |(_, f)| f.len());
    let right_scale = right.split_once('.').map_or(0, |(_, f)| f.len());
    let cap_scale = cap.split_once('.').map_or(0, |(_, f)| f.len());
    let product = decimal_scaled(left, left_scale)? * decimal_scaled(right, right_scale)?;
    let product_scale = left_scale + right_scale;
    let scale = product_scale.max(cap_scale);
    Ok(
        product * num_bigint::BigUint::from(10_u8).pow((scale - product_scale) as u32)
            <= decimal_scaled(cap, scale)?,
    )
}
pub(super) fn decimal_product_ge(left: &str, right: &str, floor: &str) -> Result<bool> {
    let left_scale = left.split_once('.').map_or(0, |(_, f)| f.len());
    let right_scale = right.split_once('.').map_or(0, |(_, f)| f.len());
    let floor_scale = floor.split_once('.').map_or(0, |(_, f)| f.len());
    let product = decimal_scaled(left, left_scale)? * decimal_scaled(right, right_scale)?;
    let product_scale = left_scale + right_scale;
    let scale = product_scale.max(floor_scale);
    Ok(
        product * num_bigint::BigUint::from(10_u8).pow((scale - product_scale) as u32)
            >= decimal_scaled(floor, scale)?,
    )
}
pub(super) fn decimal_product_ge_sum(
    left: &str,
    right: &str,
    first: &str,
    second: &str,
) -> Result<bool> {
    let left_scale = left.split_once('.').map_or(0, |(_, f)| f.len());
    let right_scale = right.split_once('.').map_or(0, |(_, f)| f.len());
    let first_scale = first.split_once('.').map_or(0, |(_, f)| f.len());
    let second_scale = second.split_once('.').map_or(0, |(_, f)| f.len());
    let product_scale = left_scale + right_scale;
    let scale = product_scale.max(first_scale).max(second_scale);
    let product = decimal_scaled(left, left_scale)?
        * decimal_scaled(right, right_scale)?
        * num_bigint::BigUint::from(10_u8).pow((scale - product_scale) as u32);
    let sum = decimal_scaled(first, scale)? + decimal_scaled(second, scale)?;
    Ok(product >= sum)
}
pub(super) fn edit_order_configuration_matches(
    order: &Value,
    base_size: &str,
    predecessor_price: &str,
    side: &str,
    portfolio_id: Option<&str>,
) -> bool {
    let configuration = &order["order_configuration"]["limit_limit_gtc"];
    let portfolio_matches = ["retail_portfolio_id", "portfolio_uuid"]
        .iter()
        .filter_map(|key| order.get(*key))
        .filter_map(Value::as_str)
        .all(|portfolio| Some(portfolio) == portfolio_id);
    configuration["base_size"].as_str() == Some(base_size)
        && configuration["limit_price"].as_str() == Some(predecessor_price)
        && order.get("side").and_then(Value::as_str) == Some(side)
        && portfolio_matches
}
fn candidate_receipt_within_price(
    prepared: &PreparedOperation,
    bounds: &CoinbaseOrderBounds,
) -> Result<bool> {
    let side = prepared.input["side"].as_str().unwrap_or("");
    let minimum_receive = prepared.input["minimumReceive"].as_str().ok_or_else(|| {
        failure(
            "COINBASE_BINDING_INVALID",
            "Prepared minimumReceive is missing.",
        )
    })?;
    let config_name = [
        "limit_limit_gtc",
        "limit_limit_gtd",
        "limit_limit_fok",
        "sor_limit_ioc",
        "stop_limit_stop_limit_gtc",
        "stop_limit_stop_limit_gtd",
        "market_market_ioc",
    ]
    .into_iter()
    .find(|name| prepared.input["orderConfiguration"].get(*name).is_some());
    let order = config_name
        .and_then(|name| prepared.input["orderConfiguration"].get(name))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            failure(
                "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
                "Boundary receipt checks require market IOC, limit GTC, or limit FOK configuration.",
            )
        })?;
    if side == "BUY" {
        if let Some(quote_size) = order.get("quote_size").and_then(Value::as_str) {
            let Some(maximum_price) = bounds.maximum_price.as_deref() else {
                return Ok(false);
            };
            decimal_product_ge(minimum_receive, maximum_price, quote_size)
        } else if let Some(base_size) = order.get("base_size").and_then(Value::as_str) {
            decimal_le(base_size, minimum_receive)
        } else {
            Ok(false)
        }
    } else if side == "SELL" {
        let (Some(base_size), Some(minimum_price)) = (
            order.get("base_size").and_then(Value::as_str),
            bounds.minimum_price.as_deref(),
        ) else {
            return Ok(false);
        };
        let quote = prepared.binding["quoteCurrencyId"].as_str().unwrap_or("");
        let fee_cap = bounds
            .maximum_fees
            .get(quote)
            .map(String::as_str)
            .unwrap_or("0");
        decimal_product_ge_sum(base_size, minimum_price, minimum_receive, fee_cap)
    } else {
        Ok(false)
    }
}
fn decimal_multiple(value: &str, increment: &str) -> Result<bool> {
    let scale = value
        .split_once('.')
        .map_or(0, |(_, f)| f.len())
        .max(increment.split_once('.').map_or(0, |(_, f)| f.len()));
    let step = decimal_scaled(increment, scale)?;
    if step == num_bigint::BigUint::from(0u8) {
        return Err(failure(
            "COINBASE_PRODUCT_METADATA_INCOMPLETE",
            "Venue increment must be positive.",
        ));
    }
    Ok(decimal_scaled(value, scale)? % step == num_bigint::BigUint::from(0u8))
}
fn client_order_id(identity: &str, request_id: &str) -> String {
    let digest =
        Sha256::digest(format!("fin-coinbase-order-v1:{identity}:{request_id}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // A deterministic RFC 4122-shaped UUID lets preview and submit use the
    // same native client identity without persisting or exposing secrets.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}
pub(super) fn coinbase_order_boundary(
    prepared: &PreparedOperation,
    raw: &Value,
) -> Result<gofin_domain::boundaries::BoundarySpec> {
    if prepared.operation != "coinbase.create-order" {
        return Err(failure(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "Boundary authorization is implemented only for Advanced spot order creation.",
        ));
    }
    let side = prepared.input["side"]
        .as_str()
        .ok_or_else(|| failure("COINBASE_BINDING_INVALID", "Prepared side is missing."))?;
    let mut bounds: CoinbaseOrderBounds = serde_json::from_value(raw.clone()).map_err(|_| {
        failure(
            "INVALID_BOUNDARIES",
            "Coinbase order boundaries must match the closed side-specific schema.",
        )
    })?;
    for value in [
        &mut bounds.maximum_spend,
        &mut bounds.maximum_quantity,
        &mut bounds.maximum_price,
        &mut bounds.minimum_price,
    ]
    .into_iter()
    .flatten()
    {
        exact_atoms(value, 36)?;
    }
    let buy = side == "BUY";
    let bounded_configuration = [
        "limit_limit_gtc",
        "limit_limit_gtd",
        "limit_limit_fok",
        "sor_limit_ioc",
        "stop_limit_stop_limit_gtc",
        "stop_limit_stop_limit_gtd",
    ]
    .into_iter()
    .find(|name| !prepared.input["orderConfiguration"][*name].is_null());
    let market_order = !prepared.input["orderConfiguration"]["market_market_ioc"].is_null();
    if bounded_configuration.is_none() && !market_order {
        return Err(failure(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "Reusable boundaries support single-order spot IOC, limit GTC/GTD/FOK, and stop-limit GTC/GTD configurations.",
        ));
    }
    let limit_order = bounded_configuration.is_some();
    if (buy
        && (bounds.maximum_spend.is_none()
            || bounds.maximum_price.is_none()
            || bounds.maximum_quantity.is_some()
            || bounds.minimum_price.is_some()))
        || (!buy
            && (bounds.maximum_quantity.is_none()
                || bounds.minimum_price.is_none()
                || bounds.maximum_spend.is_some()
                || bounds.maximum_price.is_some()))
    {
        return Err(failure(
            "INVALID_BOUNDARIES",
            "BUY needs maximumSpend and maximumPrice; SELL needs maximumQuantity and minimumPrice.",
        ));
    }
    if bounds.maximum_fees.len() != 1
        || !bounds
            .maximum_fees
            .contains_key(prepared.binding["quoteCurrencyId"].as_str().unwrap_or(""))
    {
        return Err(failure(
            "INVALID_BOUNDARIES",
            "Provide exactly one cumulative maximumFees entry in the product quote currency.",
        ));
    }
    for fee in bounds.maximum_fees.values() {
        exact_atoms(fee, 36)?;
    }
    if let Some(cap) = &bounds.maximum_spend
        && !decimal_le(prepared.input["maximumSpend"].as_str().unwrap_or("0"), cap)?
    {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Initial BUY spend exceeds its approved boundary.",
        ));
    }
    if let Some(cap) = &bounds.maximum_quantity
        && !decimal_le(prepared.input["maximumSpend"].as_str().unwrap_or("0"), cap)?
    {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Initial SELL size exceeds its approved boundary.",
        ));
    }
    if limit_order {
        if let Some(cap) = &bounds.maximum_price {
            let price = prepared.input["orderConfiguration"]
                [bounded_configuration.unwrap_or("limit_limit_gtc")]["limit_price"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "INVALID_BOUNDARIES",
                        "Boundary orders require a native limit-price configuration.",
                    )
                })?;
            if !decimal_le(price, cap)? {
                return Err(failure(
                    "BOUNDARY_VIOLATION",
                    "Initial BUY limit price exceeds its approved boundary.",
                ));
            }
        }
        if let Some(floor) = &bounds.minimum_price {
            let price = prepared.input["orderConfiguration"]
                [bounded_configuration.unwrap_or("limit_limit_gtc")]["limit_price"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "INVALID_BOUNDARIES",
                        "Boundary orders require a native limit-price configuration.",
                    )
                })?;
            if !decimal_le(floor, price)? {
                return Err(failure(
                    "BOUNDARY_VIOLATION",
                    "Initial SELL limit price is below its approved boundary.",
                ));
            }
        }
    }
    if !candidate_receipt_within_price(prepared, &bounds)? {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Initial minimumReceive is below the price-derived receipt floor for this order size.",
        ));
    }
    let resource: gofin_domain::ResourceId =
        serde_json::from_value(prepared.binding["resource"].clone()).unwrap_or_else(|_| {
            // Prepared order bindings intentionally use the enrolled account name;
            // resolve its resource from the typed financial intent instead.
            prepared
                .intent
                .locks
                .first()
                .cloned()
                .unwrap_or(gofin_domain::ResourceId(String::new()))
        });
    let (spent, received, cap) = if buy {
        (
            product_asset(prepared.binding["quoteCurrencyId"].as_str().unwrap_or("")),
            product_asset(prepared.binding["baseCurrencyId"].as_str().unwrap_or("")),
            bounds.maximum_spend.as_deref().unwrap_or("0"),
        )
    } else {
        (
            product_asset(prepared.binding["baseCurrencyId"].as_str().unwrap_or("")),
            product_asset(prepared.binding["quoteCurrencyId"].as_str().unwrap_or("")),
            bounds.maximum_quantity.as_deref().unwrap_or("0"),
        )
    };
    let fee_cap = bounds
        .maximum_fees
        .get(&received.reference)
        .or_else(|| {
            bounds
                .maximum_fees
                .get(prepared.binding["quoteCurrencyId"].as_str().unwrap_or(""))
        })
        .expect("validated fee currency");
    let maximum_costs = vec![CostBound {
        location: AccountingLocation::Cash {
            resource: resource.clone(),
        },
        amount: Amount {
            asset: product_asset(prepared.binding["quoteCurrencyId"].as_str().unwrap_or("")),
            atoms: exact_atoms(fee_cap, 36)?,
        },
        treatment: if buy {
            CostTreatment::AdditionalDebit
        } else {
            CostTreatment::DeductedFromCredit
        },
    }];
    // The receipt floor is part of the original authority. A candidate may
    // refresh fee/preview observations, but it cannot reduce what the user
    // approved to receive.
    let config_name = bounded_configuration.unwrap_or("market_market_ioc");
    let config = &prepared.input["orderConfiguration"][config_name];
    let controls = config
        .as_object()
        .map(|fields| {
            fields
                .iter()
                .filter(|(key, _)| {
                    !["base_size", "quote_size", "limit_price"].contains(&key.as_str())
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    let fixed = json!({"operation":prepared.operation,"account":prepared.binding["account"],"resource":resource,"physicalIdentity":prepared.binding["physicalIdentity"],"portfolioId":prepared.binding["portfolioId"],"productId":prepared.binding["productId"],"side":side,"baseCurrencyId":prepared.binding["baseCurrencyId"],"quoteCurrencyId":prepared.binding["quoteCurrencyId"],"configuration":config_name,"executionControls":controls,"minimumReceive":prepared.input["minimumReceive"]});
    Ok(gofin_domain::boundaries::BoundarySpec {
        fixed,
        parameters: serde_json::to_value(&bounds).map_err(|_| {
            failure(
                "INVALID_BOUNDARIES",
                "Coinbase boundaries could not be encoded.",
            )
        })?,
        maximum_debits: vec![ResourceAmount {
            resource,
            amount: Amount {
                asset: spent,
                atoms: exact_atoms(cap, 36)?,
            },
        }],
        maximum_costs,
        summary: format!(
            "{} {} spot orders for {} within the approved size, price where applicable, and cumulative quote-fee bounds.",
            side, prepared.binding["productId"], prepared.binding["account"]
        ),
    })
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CoinbaseConversionBounds {
    maximum_spend: String,
    minimum_receive: String,
    maximum_fee: String,
}
fn coinbase_conversion_boundary(
    prepared: &PreparedOperation,
    raw: &Value,
) -> Result<gofin_domain::boundaries::BoundarySpec> {
    if prepared.operation != "coinbase.convert" {
        return Err(failure(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "Expected a typed Coinbase conversion.",
        ));
    }
    let bounds: CoinbaseConversionBounds = serde_json::from_value(raw.clone()).map_err(|_| {
        failure(
            "INVALID_BOUNDARIES",
            "Conversion bounds must be a closed maximumSpend/minimumReceive/maximumFee object.",
        )
    })?;
    for value in [
        &bounds.maximum_spend,
        &bounds.minimum_receive,
        &bounds.maximum_fee,
    ] {
        exact_atoms(value, 36)?;
    }
    let input = &prepared.input;
    if !decimal_le(
        input["amount"].as_str().unwrap_or("0"),
        &bounds.maximum_spend,
    )? || !decimal_le(
        &bounds.maximum_fee,
        input["maximumFee"].as_str().unwrap_or("0"),
    )? || !decimal_le(
        input["minimumReceive"].as_str().unwrap_or("0"),
        &bounds.minimum_receive,
    )? {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Conversion exceeds its approved spend, receipt, or fee bounds.",
        ));
    }
    let resource: gofin_domain::ResourceId =
        serde_json::from_value(prepared.binding["resource"].clone()).map_err(|_| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Conversion resource binding is missing.",
            )
        })?;
    let source = product_asset(input["fromCurrency"].as_str().unwrap_or(""));
    let fixed = json!({"operation":prepared.operation,"account":prepared.binding["account"],"resource":resource,"physicalIdentity":prepared.binding["physicalIdentity"],"portfolioId":prepared.binding["portfolioId"],"requestId":prepared.binding["requestId"],"fromCurrency":input["fromCurrency"],"fromAccountId":input["fromAccountId"],"toCurrency":input["toCurrency"],"toAccountId":input["toAccountId"],"amount":input["amount"]});
    Ok(gofin_domain::boundaries::BoundarySpec {
        fixed,
        parameters: serde_json::to_value(&bounds).map_err(|_| {
            failure(
                "INVALID_BOUNDARIES",
                "Conversion bounds could not be encoded.",
            )
        })?,
        maximum_debits: vec![ResourceAmount {
            resource: resource.clone(),
            amount: Amount {
                asset: source.clone(),
                atoms: exact_atoms(&bounds.maximum_spend, 36)?,
            },
        }],
        maximum_costs: vec![CostBound {
            location: AccountingLocation::Cash { resource },
            amount: Amount {
                asset: source,
                atoms: exact_atoms(&bounds.maximum_fee, 36)?,
            },
            treatment: CostTreatment::AdditionalDebit,
        }],
        summary: format!(
            "Convert {} to {} within the approved Coinbase spend, receipt, and fee bounds.",
            input["fromCurrency"], input["toCurrency"]
        ),
    })
}
fn validate_coinbase_conversion_boundary(
    spec: &gofin_domain::boundaries::BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    let rebuilt = coinbase_conversion_boundary(prepared, &spec.parameters)?;
    if rebuilt.fixed != spec.fixed {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Conversion accounts, currencies, request identity, or amount changed.",
        ));
    }
    Ok(())
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CoinbasePortfolioMoveBounds {
    maximum_amount: String,
    minimum_amount: String,
}
fn coinbase_portfolio_move_boundary(
    prepared: &PreparedOperation,
    raw: &Value,
) -> Result<gofin_domain::boundaries::BoundarySpec> {
    if prepared.operation != "coinbase.move-portfolio-funds" {
        return Err(failure(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "Expected a typed Coinbase Advanced portfolio funds move.",
        ));
    }
    let bounds: CoinbasePortfolioMoveBounds =
        serde_json::from_value(raw.clone()).map_err(|_| {
            failure(
                "INVALID_BOUNDARIES",
                "Portfolio move bounds must be a closed maximumAmount/minimumAmount object.",
            )
        })?;
    exact_atoms(&bounds.maximum_amount, 36)?;
    exact_atoms(&bounds.minimum_amount, 36)?;
    let amount = prepared.input["amount"].as_str().unwrap_or("0");
    if !decimal_le(&bounds.minimum_amount, amount)?
        || !decimal_le(amount, &bounds.maximum_amount)?
        || !decimal_le(
            prepared.input["minimumAmount"].as_str().unwrap_or("0"),
            &bounds.minimum_amount,
        )?
        || !decimal_le(
            &bounds.maximum_amount,
            prepared.input["maximumAmount"].as_str().unwrap_or("0"),
        )?
    {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Portfolio move amount exceeds its approved debit or minimum-credit bounds.",
        ));
    }
    let source_resource: gofin_domain::ResourceId =
        serde_json::from_value(prepared.binding["resource"].clone()).map_err(|_| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Source portfolio resource binding is missing.",
            )
        })?;
    let target_resource: gofin_domain::ResourceId =
        serde_json::from_value(prepared.binding["targetResource"].clone()).map_err(|_| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Target portfolio resource binding is missing.",
            )
        })?;
    let asset = product_asset(prepared.input["currency"].as_str().unwrap_or(""));
    let fixed = json!({
        "operation":prepared.operation,"account":prepared.binding["account"],"targetAccount":prepared.binding["targetAccount"],
        "resource":source_resource,"targetResource":target_resource,
        "physicalIdentity":prepared.binding["physicalIdentity"],"targetPhysicalIdentity":prepared.binding["targetPhysicalIdentity"],
        "portfolioId":prepared.binding["portfolioId"],"targetPortfolioId":prepared.binding["targetPortfolioId"],
        "sourceCurrencyAccountId":prepared.binding["sourceCurrencyAccountId"],"targetCurrencyAccountId":prepared.binding["targetCurrencyAccountId"],
        "currency":prepared.binding["currency"],"requestId":prepared.binding["requestId"]
    });
    Ok(gofin_domain::boundaries::BoundarySpec {
        fixed,
        parameters: serde_json::to_value(&bounds).map_err(|_| {
            failure(
                "INVALID_BOUNDARIES",
                "Portfolio move bounds could not be encoded.",
            )
        })?,
        maximum_debits: vec![ResourceAmount {
            resource: source_resource.clone(),
            amount: Amount {
                asset: asset.clone(),
                atoms: exact_atoms(&bounds.maximum_amount, 36)?,
            },
        }],
        maximum_costs: vec![],
        summary: format!(
            "Move {} between the exact enrolled Coinbase Advanced portfolios.",
            asset.reference
        ),
    })
}
fn validate_coinbase_portfolio_move_boundary(
    spec: &gofin_domain::boundaries::BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    let rebuilt = coinbase_portfolio_move_boundary(prepared, &spec.parameters)?;
    if rebuilt.fixed != spec.fixed {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Portfolio move source, destination, currency-account or request identity changed.",
        ));
    }
    Ok(())
}
fn decimal_delta_equals(before: &str, after: &str, amount: &str, decrease: bool) -> Result<bool> {
    let scale = [before, after, amount]
        .iter()
        .map(|value| {
            value
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len())
        })
        .max()
        .unwrap_or(0);
    let before_value = decimal_scaled(before, scale)?;
    let after_value = decimal_scaled(after, scale)?;
    let amount_value = decimal_scaled(amount, scale)?;
    if decrease {
        Ok(before_value >= after_value && before_value - after_value == amount_value)
    } else {
        Ok(after_value >= before_value && after_value - before_value == amount_value)
    }
}
fn validate_conclusive_insufficient_funds_preview(preview: &Value, input: &Value) -> Result<()> {
    let (Some(base), Some(quote), Some(total), Some(commission)) = (
        preview.get("base_size").and_then(Value::as_str),
        preview.get("quote_size").and_then(Value::as_str),
        preview.get("order_total").and_then(Value::as_str),
        preview.get("commission_total").and_then(Value::as_str),
    ) else {
        return Err(failure(
            "COINBASE_PREVIEW_INCOMPLETE",
            "Insufficient-funds preview lacks exact base, quote, total, or commission amounts; it cannot be deferred for a future-funded step.",
        ));
    };
    for value in [base, quote, total, commission] {
        exact_atoms(value, 36)?;
    }
    if !decimal_le("0.000000000000000000000000000000000001", base)?
        || !decimal_le("0.000000000000000000000000000000000001", quote)?
        || !decimal_le("0.000000000000000000000000000000000001", total)?
    {
        return Err(failure(
            "COINBASE_PREVIEW_INCOMPLETE",
            "Insufficient-funds preview returned nonpositive trade quantities or total.",
        ));
    }
    let side = input["side"].as_str().unwrap_or("");
    let totals_match = match side {
        "BUY" => decimal_sum_equals(quote, commission, total)?,
        "SELL" => {
            decimal_le(commission, quote)? && decimal_difference_equals(quote, commission, total)?
        }
        _ => false,
    };
    if !totals_match {
        return Err(failure(
            "COINBASE_PREVIEW_INCOMPLETE",
            "Insufficient-funds preview principal, order total, and commission are not internally consistent.",
        ));
    }
    let config = input["orderConfiguration"]
        .as_object()
        .and_then(|configs| configs.values().next())
        .ok_or_else(|| {
            failure(
                "COINBASE_PREVIEW_INCOMPLETE",
                "Prepared order configuration is missing.",
            )
        })?;
    for (field, actual) in [("base_size", base), ("quote_size", quote)] {
        if let Some(requested) = config.get(field).and_then(Value::as_str)
            && (!decimal_le(requested, actual)? || !decimal_le(actual, requested)?)
        {
            return Err(failure(
                "COINBASE_PREVIEW_INCOMPLETE",
                "Coinbase preview sizes differ from the exact requested order configuration.",
            ));
        }
    }
    Ok(())
}
fn decimal_sum_equals(left: &str, right: &str, expected: &str) -> Result<bool> {
    let scale = [left, right, expected]
        .iter()
        .map(|value| {
            value
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len())
        })
        .max()
        .unwrap_or(0);
    Ok(decimal_scaled(left, scale)? + decimal_scaled(right, scale)?
        == decimal_scaled(expected, scale)?)
}
fn decimal_difference_equals(left: &str, right: &str, expected: &str) -> Result<bool> {
    let scale = [left, right, expected]
        .iter()
        .map(|value| {
            value
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len())
        })
        .max()
        .unwrap_or(0);
    let lhs = decimal_scaled(left, scale)?;
    let rhs = decimal_scaled(right, scale)?;
    Ok(lhs >= rhs && lhs - rhs == decimal_scaled(expected, scale)?)
}
pub(super) fn validate_coinbase_boundary(
    spec: &gofin_domain::boundaries::BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<()> {
    let rebuilt = coinbase_order_boundary(prepared, &spec.parameters)?;
    if rebuilt.fixed != spec.fixed {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Coinbase order account, product, currency, side or order type changed.",
        ));
    }
    let side = prepared.input["side"].as_str().unwrap_or("");
    let (order, market) = if let Some(order) = [
        "limit_limit_gtc",
        "limit_limit_gtd",
        "limit_limit_fok",
        "sor_limit_ioc",
        "stop_limit_stop_limit_gtc",
        "stop_limit_stop_limit_gtd",
    ]
    .into_iter()
    .find_map(|name| prepared.input["orderConfiguration"][name].as_object())
    {
        (order, false)
    } else if let Some(order) =
        prepared.input["orderConfiguration"]["market_market_ioc"].as_object()
    {
        (order, true)
    } else {
        return Err(failure(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "Boundary candidate must remain a typed single-order spot configuration with a finite price/size bound.",
        ));
    };
    let bounds: CoinbaseOrderBounds =
        serde_json::from_value(spec.parameters.clone()).map_err(|_| {
            failure(
                "INVALID_BOUNDARIES",
                "Stored Coinbase order boundaries are invalid.",
            )
        })?;
    if !candidate_receipt_within_price(prepared, &bounds)? {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Candidate minimumReceive is below the approved price-derived floor.",
        ));
    }
    let (spend, price_ok) = if market {
        if side == "BUY" {
            let quote_size = order
                .get("quote_size")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    failure("INVALID_INPUT", "Market BUY boundary requires quote_size.")
                })?;
            (
                decimal_le(quote_size, bounds.maximum_spend.as_deref().unwrap_or("0"))?,
                true,
            )
        } else {
            let base_size = order
                .get("base_size")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    failure("INVALID_INPUT", "Market SELL boundary requires base_size.")
                })?;
            (
                decimal_le(base_size, bounds.maximum_quantity.as_deref().unwrap_or("0"))?,
                true,
            )
        }
    } else {
        let price = order
            .get("limit_price")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    "INVALID_INPUT",
                    "Limit order boundary requires limit_price.",
                )
            })?;
        let spend = if side == "BUY" {
            if let Some(quote_size) = order.get("quote_size").and_then(Value::as_str) {
                decimal_le(quote_size, bounds.maximum_spend.as_deref().unwrap_or("0"))?
            } else if let Some(base_size) = order.get("base_size").and_then(Value::as_str) {
                decimal_product_le(
                    base_size,
                    price,
                    bounds.maximum_spend.as_deref().unwrap_or("0"),
                )?
            } else {
                false
            }
        } else {
            order
                .get("base_size")
                .and_then(Value::as_str)
                .is_some_and(|size| {
                    decimal_le(size, bounds.maximum_quantity.as_deref().unwrap_or("0"))
                        .unwrap_or(false)
                })
        };
        let price_ok = if side == "BUY" {
            decimal_le(price, bounds.maximum_price.as_deref().unwrap_or("0"))?
        } else {
            decimal_le(bounds.minimum_price.as_deref().unwrap_or("0"), price)?
        };
        (spend, price_ok)
    };
    let fee_ok = decimal_le(
        prepared.input["maximumFee"].as_str().unwrap_or("0"),
        bounds
            .maximum_fees
            .get(prepared.binding["quoteCurrencyId"].as_str().unwrap_or(""))
            .map(String::as_str)
            .unwrap_or("0"),
    )?;
    if !spend || !price_ok || !fee_ok {
        return Err(failure(
            "BOUNDARY_VIOLATION",
            "Coinbase limit order exceeds its approved size, price or fee boundary.",
        ));
    }
    Ok(())
}
pub(super) fn failure(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "coinbase",
        message,
        "Inspect the typed Coinbase operation and reconcile any persisted submission before retrying.",
    )
}
pub(super) fn coinbase_request_error(error: CoinbaseError) -> GoFinError {
    let (code, message) = match error {
        CoinbaseError::GuardRejected => (
            "COINBASE_DISPATCH_GUARD_REJECTED",
            "The prepared candidate or authorization changed before Coinbase dispatch; no request was sent.",
        ),
        CoinbaseError::InvalidCredential => (
            "COINBASE_CREDENTIAL_INVALID",
            "Protected Coinbase authentication material could not be used; no request was sent.",
        ),
        CoinbaseError::InvalidRequest => (
            "COINBASE_ROUTE_INVALID",
            "The closed Coinbase route or query failed validation; no request was sent.",
        ),
        CoinbaseError::Transport => (
            "COINBASE_TRANSPORT_UNCERTAIN",
            "Coinbase returned no response. Reconcile any persisted submission before retrying.",
        ),
    };
    failure(code, message)
}
fn persist_no_dispatch(
    request: &CoinbaseDispatch,
    journal: &dyn gofin_domain::SubmissionJournal,
    reason: &str,
    guard_diagnostic: Option<&Value>,
) -> Result<()> {
    let binding = no_dispatch_binding_digest(&request.candidate)?;
    let submission_value = serde_json::to_value(&request.submission).map_err(|_| {
        failure(
            "COINBASE_SUBMISSION_INVALID",
            "The durable Coinbase submission cannot be encoded for local evidence.",
        )
    })?;
    let submission = gofin_domain::canonical_json(&submission_value)?;
    let mut evidence = json!({
        "finNoDispatch": {
            "version": 1,
            "reason": reason,
            "operation": request.candidate.operation,
            "bindingSha256": binding,
            "submissionSha256": hex::encode(Sha256::digest(submission.as_bytes())),
            "requestBodySha256": hex::encode(Sha256::digest(request.exact_body.as_bytes()))
        }
    });
    if let Some(diagnostic) = guard_diagnostic {
        evidence["finNoDispatch"]["guardDiagnostic"] = diagnostic.clone();
    }
    journal.persist_dispatch_evidence(&request.submission, &evidence)
}
fn no_dispatch_binding_digest(prepared: &PreparedOperation) -> Result<String> {
    let mut binding = prepared.binding.clone();
    if let Some(binding) = binding.as_object_mut() {
        binding.remove(gofin_domain::financial_plans::PLAN_METADATA_KEY);
    }
    let binding = gofin_domain::canonical_json(&binding)?;
    Ok(hex::encode(Sha256::digest(binding.as_bytes())))
}
fn preserve_guard_journal_error(mut error: GoFinError, diagnostic: &Value) -> GoFinError {
    let mut details = error
        .details
        .take()
        .map(|value| *value)
        .unwrap_or_else(|| json!({}));
    if !details.is_object() {
        details = json!({"journalDetails":details});
    }
    details["coinbaseDispatch"] = json!({"sent":false,"guardDiagnostic":diagnostic});
    error.details = Some(Box::new(details));
    error
}
fn is_definitive_predispatch_error(code: &str) -> bool {
    matches!(
        code,
        "COINBASE_DISPATCH_GUARD_REJECTED"
            | "COINBASE_CREDENTIAL_INVALID"
            | "COINBASE_ROUTE_INVALID"
    )
}
fn validated_no_dispatch_certificate(
    record: &OperationRecord,
    submission: &gofin_domain::Submission,
) -> Option<Value> {
    if record.submission.as_ref() != Some(submission) {
        return None;
    }
    let prepared = record.preview.as_ref()?;
    if prepared.operation != record.operation {
        return None;
    }
    let certificate = record.dispatch_evidence.as_ref()?.get("finNoDispatch")?;
    let reason = certificate.get("reason")?.as_str()?;
    if !matches!(
        reason,
        "authorization_gate"
            | "transport_guard_rejected"
            | "credential_rejected_before_send"
            | "closed_route_rejected_before_send"
            | "pre_dispatch_rejected"
    ) || certificate.get("version")?.as_u64() != Some(1)
        || certificate.get("operation")?.as_str()? != record.operation
    {
        return None;
    }
    let submission_value =
        gofin_domain::canonical_json(&serde_json::to_value(submission).ok()?).ok()?;
    let request_body = if prepared.operation == "coinbaseexchange.create-balance-report" {
        prepared.binding.get("bodyExact")?.as_str()?
    } else {
        prepared.binding.get("requestBodyExact")?.as_str()?
    };
    let expected = [
        ("bindingSha256", no_dispatch_binding_digest(prepared).ok()?),
        (
            "submissionSha256",
            hex::encode(Sha256::digest(submission_value.as_bytes())),
        ),
        (
            "requestBodySha256",
            hex::encode(Sha256::digest(request_body.as_bytes())),
        ),
    ];
    if expected
        .iter()
        .any(|(field, digest)| certificate.get(*field).and_then(Value::as_str) != Some(digest))
    {
        return None;
    }
    Some(certificate.clone())
}
fn is_terminal_verified_no_dispatch(
    record: &OperationRecord,
    product: CoinbaseProduct,
    client_order_id: &str,
) -> bool {
    if record.status != Status::Failed {
        return false;
    }
    let Some(submission) = record.submission.as_ref() else {
        return false;
    };
    let Some(prepared) = record.preview.as_ref() else {
        return false;
    };
    let (operation, pointer) = match product {
        CoinbaseProduct::Advanced => ("coinbase.create-order", "/clientOrderId"),
        CoinbaseProduct::Exchange => ("coinbaseexchange.create-order", "/clientOrderId"),
        CoinbaseProduct::International if record.operation.ends_with("edit-order") => (
            "coinbaseinternational.edit-order",
            "/successorClientOrderId",
        ),
        CoinbaseProduct::International => ("coinbaseinternational.create-order", "/clientOrderId"),
    };
    if record.operation != operation
        || prepared.binding.pointer(pointer).and_then(Value::as_str) != Some(client_order_id)
        || validated_no_dispatch_certificate(record, submission).is_none()
    {
        return false;
    }
    let Some(verified) = record.effects.as_ref() else {
        return false;
    };
    verified.submission == *submission
        && verified.effects.is_empty()
        && gofin_domain::verified_effect_digest(verified)
            .is_ok_and(|digest| digest == verified.proof_digest)
        && verified.evidence["noDispatch"]["requestBodySha256"]
            == record.dispatch_evidence.as_ref().unwrap()["finNoDispatch"]["requestBodySha256"]
}
fn coinbase_unknown(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason}),
        verified: None,
    }
}

fn captured_configuration_response(dispatch: &Value) -> Option<(u16, Value)> {
    let status = u16::try_from(dispatch.get("httpStatus")?.as_u64()?).ok()?;
    let encoded = dispatch.get("bodyHex")?.as_str()?;
    let expected = dispatch.get("bodySha256")?.as_str()?;
    let bytes = hex::decode(encoded).ok()?;
    if hex::encode(Sha256::digest(&bytes)) != expected {
        return None;
    }
    let body = serde_json::from_slice(&bytes).ok()?;
    Some((status, body))
}
fn coinbase_unknown_request(reason: &str, error: &GoFinError) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason,"requestErrorCode":error.code}),
        verified: None,
    }
}
fn is_conclusive_create_order_rejection(status: u16, receipt: &Value) -> bool {
    if !(200..300).contains(&status) && !(400..500).contains(&status) {
        return false;
    }
    if receipt.get("success").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if (200..300).contains(&status) {
        if receipt.get("success").and_then(Value::as_bool) != Some(false) {
            return false;
        }
        return receipt
            .get("error_response")
            .and_then(Value::as_object)
            .and_then(|error| error.get("error").and_then(Value::as_str))
            .is_some_and(is_definitive_create_rejection_code);
    }
    // A non-2xx status alone is not enough: require a native machine-readable
    // error body so malformed proxies and intermediary responses stay Unknown.
    receipt
        .get("error_response")
        .and_then(Value::as_object)
        .and_then(|error| error.get("error").and_then(Value::as_str))
        .is_some_and(is_definitive_create_rejection_code)
        || receipt
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(is_definitive_create_rejection_code)
}
fn is_definitive_create_rejection_code(code: &str) -> bool {
    matches!(
        code,
        "INSUFFICIENT_FUND"
            | "INSUFFICIENT_FUNDS"
            | "INVALID_ARGUMENT"
            | "INVALID_PRODUCT_ID"
            | "INVALID_ORDER_CONFIGURATION"
            | "INVALID_ORDER_TYPE"
            | "INVALID_SIDE"
            | "INVALID_BASE_SIZE"
            | "INVALID_QUOTE_SIZE"
            | "INVALID_LIMIT_PRICE"
            | "INVALID_TIME_IN_FORCE"
            | "INVALID_POST_ONLY"
    )
}
fn native_cfm_setting(value: &Value) -> Option<&str> {
    value.get("setting").and_then(Value::as_str)
}

#[async_trait]
impl ExchangeProfile for CoinbaseProfile {
    type Request = CoinbaseDispatch;
    fn attach_execution_state(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<()> {
        *self.execution_state.write().map_err(|_| {
            failure(
                "EXECUTION_STATE_UNAVAILABLE",
                "Protected execution history cannot be attached.",
            )
        })? = Some(state);
        Ok(())
    }
    fn definitions(&self) -> Vec<Definition> {
        contracts::definitions()
    }
    fn submission_kind(&self) -> &str {
        "coinbase"
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.accounts.iter().map(Self::connection).collect()
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        if let Some(value) = crate::institutional::read(self, operation, input).await? {
            return Ok(value);
        }
        if let Some(descriptor) = crate::reads::descriptor(operation) {
            let validator =
                jsonschema::validator_for(&crate::reads::schema(descriptor)).map_err(|_| {
                    failure(
                        "COINBASE_CONTRACT_INVALID",
                        "Native read schema is invalid.",
                    )
                })?;
            if !validator.is_valid(input) {
                return Err(failure(
                    "INVALID_INPUT",
                    "Input does not match the closed native read schema.",
                ));
            }
            let product = match descriptor.product {
                "coinbaseexchange" => CoinbaseProduct::Exchange,
                "coinbaseinternational" => CoinbaseProduct::International,
                _ => CoinbaseProduct::Advanced,
            };
            let account = self.account(input["account"].as_str().expect("schema"), product)?;
            for key in ["portfolio", "portfolio_uuid"] {
                let expected = account.portfolio_id.as_deref().or_else(|| {
                    (product == CoinbaseProduct::Advanced)
                        .then_some(account.physical_identity.as_str())
                });
                if input[key].as_str().is_some_and(|id| Some(id) != expected) {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_MISMATCH",
                        "Requested portfolio differs from the enrolled portfolio.",
                    ));
                }
                if input[key].as_str().is_some() && expected.is_none() {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_UNAVAILABLE",
                        "This product has no exact enrolled portfolio ID for the requested native read.",
                    ));
                }
            }
            if let Some(profile_id) = input
                .get("query")
                .and_then(|query| query.get("profile_id"))
                .and_then(Value::as_str)
                && (product != CoinbaseProduct::Exchange || profile_id != account.physical_identity)
            {
                return Err(failure(
                    "COINBASE_PROFILE_MISMATCH",
                    "Requested Exchange profile differs from the enrolled API identity.",
                ));
            }
            for key in ["portfolio_id", "portfolio_uuid", "retail_portfolio_id"] {
                if let Some(value) = input
                    .get("query")
                    .and_then(|query| query.get(key))
                    .and_then(Value::as_str)
                {
                    let expected = account.portfolio_id.as_deref().ok_or_else(|| failure(
                        "COINBASE_PORTFOLIO_UNAVAILABLE",
                        "This product has no exact enrolled portfolio identity for the requested query filter.",
                    ))?;
                    if value != expected {
                        return Err(failure(
                            "COINBASE_PORTFOLIO_MISMATCH",
                            "A native portfolio query filter differs from the enrolled portfolio.",
                        ));
                    }
                }
            }
            if let Some(portfolios) = input.get("query").and_then(|query| query.get("portfolios")) {
                let expected = account.portfolio_id.as_deref().ok_or_else(|| {
                    failure(
                        "COINBASE_PORTFOLIO_UNAVAILABLE",
                        "This product has no exact enrolled portfolio identity for the requested filter.",
                    )
                })?;
                if portfolios
                    .as_array()
                    .is_none_or(|ids| ids.len() != 1 || ids[0].as_str() != Some(expected))
                {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_MISMATCH",
                        "A native portfolio filter must name only the enrolled portfolio.",
                    ));
                }
            }
            if product == CoinbaseProduct::Advanced
                && descriptor
                    .path
                    .starts_with("/api/v3/brokerage/accounts/{account_id}")
            {
                let currency_account_id = input["account_id"].as_str().expect("schema");
                let _ = self
                    .advanced_currency_account_details(account, currency_account_id)
                    .await?;
            }
            if product == CoinbaseProduct::Advanced
                && descriptor.path.starts_with("/v2/accounts/{account_id}")
            {
                self.verify_v2_account_scope(
                    account,
                    input["account_id"].as_str().expect("schema"),
                )
                .await?;
            }
            let response = self
                .request_for_account(
                    &account.name,
                    product,
                    crate::reads::endpoint(descriptor, input),
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(failure(
                    "COINBASE_REQUEST_FAILED",
                    "The server rejected this native read; check access and route availability.",
                ));
            }
            if product == CoinbaseProduct::International && descriptor.path == "/api/v1/portfolios"
            {
                let portfolio_id = account.portfolio_id.as_deref().ok_or_else(|| failure(
                    "COINBASE_PORTFOLIO_UNAVAILABLE",
                    "International portfolio list reads require an exact enrolled portfolio ID.",
                ))?;
                let mut data: Value = serde_json::from_slice(&response.body).map_err(|_| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Coinbase returned invalid portfolio-list JSON.",
                    )
                })?;
                let rows = data
                    .get_mut("portfolios")
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_PORTFOLIO_UNAVAILABLE",
                            "International portfolio list omitted its portfolios array.",
                        )
                    })?;
                let matching = rows
                    .iter()
                    .filter(|row| {
                        row.get("portfolio_id").and_then(Value::as_str) == Some(portfolio_id)
                    })
                    .count();
                if matching != 1 {
                    return Err(failure(
                        "COINBASE_PORTFOLIO_MISMATCH",
                        "International portfolio listing did not identify exactly one enrolled portfolio.",
                    ));
                }
                rows.retain(|row| {
                    row.get("portfolio_id").and_then(Value::as_str) == Some(portfolio_id)
                });
                return Ok(data);
            }
            return crate::reads::response_value(&response).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid JSON.",
                )
            });
        }
        let (account, contract, input) = self.decode(operation, input)?;
        self.read_contract(account, contract, &input).await
    }
    fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<gofin_domain::boundaries::BoundarySpec> {
        if crate::institutional_authority::supported(&prepared.operation) {
            return crate::institutional_authority::prepare_boundaries(prepared, bounds);
        }
        if prepared.operation == "coinbase.move-portfolio-funds" {
            coinbase_portfolio_move_boundary(prepared, bounds)
        } else if prepared.operation == "coinbase.convert" {
            coinbase_conversion_boundary(prepared, bounds)
        } else if prepared.operation == "coinbase.edit-order" {
            let mut original = prepared.clone();
            original.operation = "coinbase.create-order".into();
            original.input = prepared.binding["originalInput"].clone();
            original.binding = prepared.binding["originalBinding"].clone();
            original.input["orderConfiguration"]["limit_limit_gtc"]["limit_price"] =
                prepared.binding["newPrice"].clone();
            coinbase_order_boundary(&original, bounds)
        } else {
            coinbase_order_boundary(prepared, bounds)
        }
    }
    fn validate_boundary_candidate(
        &self,
        spec: &gofin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<()> {
        if crate::institutional_authority::supported(&prepared.operation) {
            return crate::institutional_authority::validate_candidate(spec, prepared);
        }
        if prepared.operation == "coinbase.move-portfolio-funds" {
            validate_coinbase_portfolio_move_boundary(spec, prepared)
        } else if prepared.operation == "coinbase.convert" {
            validate_coinbase_conversion_boundary(spec, prepared)
        } else if prepared.operation == "coinbase.edit-order" {
            let mut original = prepared.clone();
            original.operation = "coinbase.create-order".into();
            original.input = prepared.binding["originalInput"].clone();
            original.binding = prepared.binding["originalBinding"].clone();
            original.input["orderConfiguration"]["limit_limit_gtc"]["limit_price"] =
                prepared.binding["newPrice"].clone();
            validate_coinbase_boundary(spec, &original)
        } else {
            validate_coinbase_boundary(spec, prepared)
        }
    }
    async fn prepare_plan_permission(
        &self,
        operation: &str,
        input: &Value,
        bounds: &Value,
    ) -> Result<gofin_domain::financial_plans::PlanPermission> {
        if crate::institutional_authority::supported(operation) {
            let prepared = crate::institutional::prepare_order_for_plan(self, operation, input)
                .await?
                .ok_or_else(|| {
                    failure(
                        "PLAN_PERMISSION_UNSUPPORTED",
                        "This institutional action has no bounded spot-order plan contract.",
                    )
                })?;
            return crate::institutional_authority::plan_permission(&prepared, bounds);
        }
        if operation == "coinbase.convert" {
            let prepared = self.prepare_convert(input).await?;
            let spec = coinbase_conversion_boundary(&prepared, bounds)?;
            return Ok(gofin_domain::financial_plans::PlanPermission {
                spec,
                recovery_parent: None,
                possible_credits: vec![
                    gofin_domain::financial_plans::FinancialPlanFundingIdentity {
                        resource: prepared.intent.locks[0].clone(),
                        asset: product_asset(input["toCurrency"].as_str().expect("schema")),
                    },
                ],
                fee_assets: vec![product_asset(
                    input["fromCurrency"].as_str().expect("schema"),
                )],
                deferred_receipt: None,
                authorization_seconds: None,
                cumulative_scope: None,
            });
        }
        if operation == "coinbase.move-portfolio-funds" {
            let prepared = self.prepare_portfolio_move(input).await?;
            let spec = coinbase_portfolio_move_boundary(&prepared, bounds)?;
            let target: gofin_domain::ResourceId = serde_json::from_value(
                prepared.binding["targetResource"].clone(),
            )
            .map_err(|_| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Target portfolio resource is missing.",
                )
            })?;
            return Ok(gofin_domain::financial_plans::PlanPermission {
                spec,
                recovery_parent: None,
                possible_credits: vec![
                    gofin_domain::financial_plans::FinancialPlanFundingIdentity {
                        resource: target,
                        asset: product_asset(input["currency"].as_str().expect("schema")),
                    },
                ],
                fee_assets: vec![],
                deferred_receipt: None,
                authorization_seconds: None,
                cumulative_scope: None,
            });
        }
        if operation != "coinbase.create-order" {
            return Err(failure(
                "PLAN_PERMISSION_UNSUPPORTED",
                "Named Coinbase plans currently support Advanced spot limit GTC/FOK and market IOC order steps.",
            ));
        }
        // Plan review runs before earlier funding steps execute. Coinbase may
        // return its explicit insufficient-funds code while still furnishing
        // authoritative exact trade/fee fields. This candidate cannot submit;
        // runtime preparation always requires an empty native errs array.
        let prepared = self.prepare_order_with_funding_policy(input, true).await?;
        let spec = coinbase_order_boundary(&prepared, bounds)?;
        let possible_credits = prepared
            .intent
            .expected
            .iter()
            .filter_map(|effect| match effect {
                EffectConstraint::Conversion {
                    resource, output, ..
                } => Some(
                    gofin_domain::financial_plans::FinancialPlanFundingIdentity {
                        resource: resource.clone(),
                        asset: output.clone(),
                    },
                ),
                _ => None,
            })
            .collect();
        let fee_assets = spec
            .maximum_costs
            .iter()
            .map(|cost| cost.amount.asset.clone())
            .collect();
        Ok(gofin_domain::financial_plans::PlanPermission {
            spec,
            recovery_parent: None,
            possible_credits,
            fee_assets,
            deferred_receipt: None,
            authorization_seconds: None,
            cumulative_scope: None,
        })
    }
    async fn validate_plan_feasibility(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<Vec<String>> {
        if crate::institutional_authority::supported(operation) {
            let prepared = crate::institutional::prepare_order_for_plan(self, operation, input)
                .await?
                .ok_or_else(|| {
                    failure(
                        "PLAN_PERMISSION_UNSUPPORTED",
                        "This institutional action has no bounded spot-order plan contract.",
                    )
                })?;
            let quote_asset = prepared
                .intent
                .maximum_costs
                .first()
                .map(|cost| cost.amount.asset.reference.clone())
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "The institutional quote-currency fee identity is missing.",
                    )
                })?;
            let fee_bounds = json!({(quote_asset):input["maximumFee"]});
            let bounds = if input["side"].as_str() == Some("BUY") {
                json!({"maximumSpend":input["maximumSpend"],"maximumPrice":input["limitPrice"],"maximumFees":fee_bounds})
            } else {
                json!({"maximumQuantity":input["baseSize"],"minimumPrice":input["limitPrice"],"maximumFees":fee_bounds})
            };
            let permission = crate::institutional_authority::plan_permission(&prepared, &bounds)?;
            crate::institutional_authority::validate_candidate(&permission.spec, &prepared)?;
            let mut warnings = vec!["Institutional spot metadata and native preview were checked without submitting; product permissions, fee bounds, intervening holds and fills are revalidated for the approved plan step.".into()];
            warnings.extend(prepared.warnings);
            return Ok(warnings);
        }
        if operation == "coinbase.convert" {
            let _prepared = self.prepare_convert(input).await?;
            return Ok(vec![format!(
                "Coinbase returned a bounded {} to {} conversion quote with exact account identities and source-currency fee.",
                input["fromCurrency"], input["toCurrency"]
            )]);
        }
        if operation == "coinbase.move-portfolio-funds" {
            let _prepared = self.prepare_portfolio_move(input).await?;
            return Ok(vec!["Both enrolled Advanced portfolios, source transfer permission, target visibility, exact currency accounts, and available source balance were verified. Coinbase provides no move idempotency key; uncertain outcomes will not be replayed.".into()]);
        }
        if operation == "coinbase.withdraw" {
            let prepared = self
                .prepare_withdrawal_with_funding_policy(input, true)
                .await?;
            let mut warnings = vec!["Exact Coinbase withdrawal identities and transfer permission were checked. Available principal plus fee reserve is required when the step executes; a principal-only dependency may use verified prior proceeds. Actual venue-calculated fees are uncapped and paid from the approved Coinbase account cash.".into()];
            warnings.extend(prepared.warnings);
            return Ok(warnings);
        }
        if operation != "coinbase.create-order" {
            return Err(failure(
                "PLAN_PERMISSION_UNSUPPORTED",
                "This Coinbase action has no reviewed named-plan feasibility contract.",
            ));
        }
        let prepared = self.prepare_order_with_funding_policy(input, true).await?;
        if ![
            "market_market_ioc",
            "sor_limit_ioc",
            "limit_limit_gtc",
            "limit_limit_gtd",
            "limit_limit_fok",
            "stop_limit_stop_limit_gtc",
            "stop_limit_stop_limit_gtd",
        ]
        .iter()
        .any(|name| !prepared.input["orderConfiguration"][*name].is_null())
        {
            return Err(failure(
                "PLAN_PERMISSION_UNSUPPORTED",
                "Plan steps must use a single order with bounded spot execution; TWAP and multi-child configurations remain unsupported.",
            ));
        }
        let mut warnings = vec!["Read-only product metadata and venue preview passed; balances, intervening holds, fee changes and fills are rechecked when each plan step executes.".into()];
        warnings.extend(prepared.warnings);
        Ok(warnings)
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        if let Some(snapshot) = crate::snapshot::mixed_product_snapshot(self, None).await? {
            return Ok(snapshot);
        }
        if let Some(snapshot) = crate::institutional::snapshot(self, None).await? {
            return Ok(snapshot);
        }
        if self
            .accounts
            .iter()
            .any(|a| a.product != CoinbaseProduct::Advanced)
        {
            return Err(failure(
                "COINBASE_SNAPSHOT_SCOPE_UNSUPPORTED",
                "Full snapshot currently requires an Advanced-only profile.",
            ));
        }
        if self.accounts.is_empty() {
            return Err(failure(
                "COINBASE_ACCOUNT_REQUIRED",
                "No Advanced profile is configured.",
            ));
        }
        self.snapshot_scoped(&SnapshotScope::default()).await
    }
    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> Result<StateSnapshot> {
        if let Some(snapshot) = crate::snapshot::mixed_product_snapshot(self, Some(scope)).await? {
            return Ok(snapshot);
        }
        if let Some(snapshot) = crate::institutional::snapshot(self, Some(scope)).await? {
            return Ok(snapshot);
        }
        let selected = self
            .accounts
            .iter()
            .filter(|a| scope.matches_connection(&Self::connection(a)))
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(failure(
                "COINBASE_ACCOUNT_REQUIRED",
                "No enrolled Advanced account matches this snapshot scope.",
            ));
        }
        let mut rows = Vec::new();
        for account in selected {
            if account.product != CoinbaseProduct::Advanced {
                return Err(failure(
                    "COINBASE_SNAPSHOT_SCOPE_UNSUPPORTED",
                    "This scoped snapshot includes a non-Advanced product.",
                ));
            }
            rows.push(self.advanced_cash_snapshot(account, scope).await?);
        }
        let mut coverage = rows
            .iter()
            .flat_map(|s| s.coverage.clone())
            .collect::<Vec<_>>();
        let scoped_to_cash = !scope.is_full()
            && (!scope.networks.is_empty() || !scope.assets.is_empty())
            && scope.networks.iter().all(|network| network == "coinbase")
            && scope.assets.iter().all(|asset| asset.network == "coinbase");
        coverage.push(if scoped_to_cash {
            "Requested Coinbase cash identities fully observed; derivatives, loans, collateral and staking positions are outside this cash-only scope.".into()
        } else {
            "Advanced portfolio currency cash only; derivatives, loans, collateral and staking positions are not observed by this unscoped snapshot.".into()
        });
        Ok(StateSnapshot {
            state: FinancialState {
                holdings: rows.iter().flat_map(|s| s.state.holdings.clone()).collect(),
                positions: vec![],
                authorities: vec![],
                in_flight: rows
                    .iter()
                    .flat_map(|s| s.state.in_flight.clone())
                    .collect(),
            },
            observed_at: gofin_domain::now(),
            complete: scoped_to_cash && rows.iter().all(|s| s.complete),
            coverage,
            pending: rows.iter().flat_map(|s| s.pending.clone()).collect(),
        })
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if let Some(prepared) = crate::institutional::prepare(self, operation, input).await? {
            return Ok(prepared);
        }
        if operation == "coinbase.create-order" {
            return self.prepare_order(input).await;
        }
        if operation == "coinbase.cancel-order" {
            return self.prepare_cancel_order(input).await;
        }
        if operation == "coinbase.cancel-orders" {
            return self.prepare_cancel_orders(input).await;
        }
        if operation == "coinbase.create-deposit-address" {
            return self.prepare_deposit_address_configuration(input).await;
        }
        if matches!(
            operation,
            "coinbase.create-portfolio" | "coinbase.rename-portfolio"
        ) {
            return self.prepare_portfolio_configuration(operation, input).await;
        }
        if operation == "coinbase.delete-portfolio" {
            return self.prepare_portfolio_delete(input).await;
        }
        if operation == "coinbase.set-cfm-margin-setting" {
            return self.prepare_cfm_margin_setting(input).await;
        }
        if operation == "coinbase.convert" {
            return self.prepare_convert(input).await;
        }
        if operation == "coinbase.move-portfolio-funds" {
            return self.prepare_portfolio_move(input).await;
        }
        if operation == "coinbase.withdraw" {
            return self.prepare_withdrawal(input).await;
        }
        Err(failure(
            "COINBASE_WRITE_UNAVAILABLE",
            "This Coinbase profile does not yet expose this write as a prepared financial operation.",
        ))
    }
    async fn refresh_prepared(&self, approved: &PreparedOperation) -> Result<PreparedOperation> {
        if approved.operation == "coinbase.edit-order" {
            return self.refresh_edit_order(approved).await;
        }
        if approved.operation == "coinbase.convert" {
            return self.refresh_conversion(approved).await;
        }
        if let Some(fresh) = crate::institutional::refresh(self, approved).await? {
            return Ok(fresh);
        }
        let mut fresh = self.prepare(&approved.operation, &approved.input).await?;
        if approved.operation == "coinbase.withdraw" {
            // Idempotency is durable execution identity: refreshing may update
            // observations, but must preserve the UUID approved by the user.
            fresh.binding["idem"] = approved.binding["idem"].clone();
            fresh.binding["requestBodyExact"] = approved.binding["requestBodyExact"].clone();
        }
        let immutable_fields: &[&str] = if approved.operation == "coinbase.create-order" {
            &[
                "account",
                "resource",
                "physicalIdentity",
                "portfolioId",
                "productId",
                "clientOrderId",
                "requestBodyExact",
                "baseCurrencyId",
                "quoteCurrencyId",
            ]
        } else if approved.operation == "coinbase.cancel-order" {
            &[
                "account",
                "resource",
                "physicalIdentity",
                "portfolioId",
                "orderId",
                "clientOrderId",
                "productId",
                "operationId",
                "originalSubmission",
                "requestBodyExact",
            ]
        } else if approved.operation == "coinbase.cancel-orders" {
            &[
                "account",
                "resource",
                "physicalIdentity",
                "portfolioId",
                "requestId",
                "orders",
                "requestBodyExact",
            ]
        } else if approved.operation == "coinbase.move-portfolio-funds" {
            &[
                "account",
                "targetAccount",
                "resource",
                "targetResource",
                "physicalIdentity",
                "targetPhysicalIdentity",
                "portfolioId",
                "targetPortfolioId",
                "currency",
                "sourceCurrencyAccountId",
                "targetCurrencyAccountId",
                "requestId",
                "requestBodyExact",
            ]
        } else if approved.operation == "coinbase.create-deposit-address" {
            &[
                "account",
                "resource",
                "physicalIdentity",
                "portfolioId",
                "currencyAccountId",
                "currency",
            ]
        } else if matches!(
            approved.operation.as_str(),
            "coinbase.create-portfolio" | "coinbase.rename-portfolio" | "coinbase.delete-portfolio"
        ) {
            &[
                "account",
                "resource",
                "physicalIdentity",
                "portfolioId",
                "requestId",
                "newName",
                "previousName",
                "portfolioId",
                "requestBodyExact",
                "configurationRoute",
                "checkedAccountCount",
                "checkedOpenOrderCount",
            ]
        } else if approved.operation == "coinbase.set-cfm-margin-setting" {
            &[
                "account",
                "resource",
                "physicalIdentity",
                "portfolioId",
                "requestId",
                "previousSetting",
                "setting",
                "requestBodyExact",
            ]
        } else if approved.operation == "coinbase.withdraw" {
            &[
                "account",
                "resource",
                "physicalIdentity",
                "portfolioId",
                "currencyAccountId",
                "network",
                "currency",
                "amount",
                "amountAtoms",
                "feeMode",
                "feeReserveUsdc",
                "destinationName",
                "destinationAddress",
                "destination",
                "travelRuleData",
                "requestId",
                "idem",
                "requestBodyExact",
            ]
        } else {
            return Err(failure(
                "WRONG_OPERATION_KIND",
                "Only typed Coinbase financial operations can be refreshed.",
            ));
        };
        if fresh.operation != approved.operation
            || fresh.input != approved.input
            || immutable_fields
                .iter()
                .any(|field| fresh.binding[*field] != approved.binding[*field])
        {
            return Err(failure(
                "COINBASE_PREVIEW_CHANGED",
                "A bound Coinbase account, product, order identity, or exact request changed during refresh.",
            ));
        }
        if approved.operation == "coinbase.move-portfolio-funds" {
            let source = fresh.binding["account"].as_str().expect("bound");
            let target = fresh.binding["targetAccount"].as_str().expect("bound");
            self.verify_advanced_key(
                self.account(source, CoinbaseProduct::Advanced)?,
                false,
                true,
            )
            .await?;
            self.verify_advanced_key(
                self.account(target, CoinbaseProduct::Advanced)?,
                false,
                false,
            )
            .await?;
        }
        if matches!(
            approved.operation.as_str(),
            "coinbase.create-portfolio" | "coinbase.rename-portfolio" | "coinbase.delete-portfolio"
        ) {
            let account = fresh.binding["account"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Portfolio account binding is missing.",
                )
            })?;
            self.verify_advanced_key(
                self.account(account, CoinbaseProduct::Advanced)?,
                true,
                false,
            )
            .await?;
        }
        gofin_domain::value_bounds::carry(approved, &mut fresh)?;
        if approved.operation == "coinbase.create-order" {
            // A Coinbase preview is a time-bound observation, not the identity
            // of the order we will submit. `prepare` above has re-read the
            // product, regenerated the venue preview, validated its native
            // errors and exact principal/fee against the approved input, and
            // `carry` has revalidated any Fin-owned USD ceilings. Keep that
            // fresh evidence in checks/valuation while retaining the complete
            // approved binding (including broker-owned plan metadata and the
            // original preview as historical evidence). The durable execution
            // identity remains the exact client order ID and request body.
            let preview = fresh.binding.get("preview").ok_or_else(|| {
                failure(
                    "COINBASE_PREVIEW_INCONCLUSIVE",
                    "Refreshed order preview evidence is missing.",
                )
            })?;
            let preview_id = preview
                .get("preview_id")
                .or_else(|| preview.get("previewId"))
                .and_then(Value::as_str)
                .unwrap_or("not-returned");
            let order_total = preview
                .get("order_total")
                .and_then(Value::as_str)
                .unwrap_or("not-returned");
            let commission_total = preview
                .get("commission_total")
                .and_then(Value::as_str)
                .unwrap_or("not-returned");
            fresh.checks.push(Check {
                name: "refreshed_order_preview".into(),
                status: CheckStatus::Passed,
                detail: format!(
                    "Fresh native preview {preview_id} passed the original order bounds (order total {order_total}; commission {commission_total}); preview identity is observation-only."
                ),
            });
            fresh.binding = approved.binding.clone();
        }
        Ok(fresh)
    }
    async fn finalize_after_reservation(
        &self,
        approved: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        if approved.operation != "coinbase.create-order" {
            return Ok(approved.clone());
        }

        // Reservation can take time. Recheck permissions, product metadata,
        // the native preview and USD pricing after the reservation is held,
        // while keeping the identity and authority the user approved.
        let mut finalized = self.refresh_prepared(approved).await?;
        if finalized.input != approved.input
            || finalized.binding != approved.binding
            || finalized.intent != approved.intent
            || finalized.operation_version != approved.operation_version
            || finalized.action_deadline != approved.action_deadline
        {
            return Err(failure(
                "COINBASE_PREVIEW_CHANGED",
                "Post-reservation refresh changed the approved order identity or financial authority.",
            ));
        }

        // Do not extend the user's original preview lifetime during finalization.
        finalized.expires_at = approved.expires_at.clone();
        finalized.checks.push(Check {
            name: "advanced_trade_permission_finalized".into(),
            status: CheckStatus::Passed,
            detail: "Advanced Trade key permissions, order preview, product metadata and USD valuation were freshly checked after reservation; the approved client order identity, exact request and financial limits were preserved.".into(),
        });
        Ok(finalized)
    }
    fn valuation_matches(
        &self,
        approved: &PreparedOperation,
        refreshed: &PreparedOperation,
    ) -> bool {
        if approved
            .binding
            .get(gofin_domain::value_bounds::KEY)
            .is_some()
        {
            gofin_domain::value_bounds::validate(refreshed).is_ok()
        } else {
            approved.valuation == refreshed.valuation
        }
    }
    async fn plan_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<Self::Request>> {
        if current.operation == "coinbase.withdraw" {
            return self.plan_withdrawal_submission(current).await;
        }
        if let Some(submission) = crate::institutional::plan_submission(self, current).await? {
            return Ok(submission);
        }
        if current.operation == "coinbase.move-portfolio-funds" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure("COINBASE_BINDING_INVALID", "Source portfolio is missing.")
            })?;
            let target = current.binding["targetAccount"].as_str().ok_or_else(|| {
                failure("COINBASE_BINDING_INVALID", "Target portfolio is missing.")
            })?;
            let source_cfg = self.account(account, CoinbaseProduct::Advanced)?;
            let target_cfg = self.account(target, CoinbaseProduct::Advanced)?;
            self.verify_advanced_key(source_cfg, false, true).await?;
            self.verify_advanced_key(target_cfg, false, false).await?;
            if !self
                .advanced_portfolio_contains(
                    source_cfg,
                    current.binding["portfolioId"].as_str().unwrap_or(""),
                )
                .await?
                || !self
                    .advanced_portfolio_contains(
                        source_cfg,
                        current.binding["targetPortfolioId"].as_str().unwrap_or(""),
                    )
                    .await?
            {
                return Err(failure(
                    "COINBASE_PORTFOLIO_NOT_VISIBLE",
                    "Both exact portfolio IDs must remain visible before dispatch.",
                ));
            }
            let currency = current.binding["currency"].as_str().expect("bound");
            let amount = current.input["amount"].as_str().expect("schema");
            let balance = self
                .advanced_currency_account_balance(
                    source_cfg,
                    current.binding["sourceCurrencyAccountId"]
                        .as_str()
                        .expect("bound"),
                    currency,
                )
                .await?;
            let _target_balance = self
                .advanced_currency_account_balance(
                    target_cfg,
                    current.binding["targetCurrencyAccountId"]
                        .as_str()
                        .expect("bound"),
                    currency,
                )
                .await?;
            if !decimal_le(amount, &balance)? {
                return Err(failure(
                    "COINBASE_TRANSFER_BALANCE_CHANGED",
                    "Available source balance no longer covers the authorized portfolio move.",
                ));
            }
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact portfolio move body is missing.",
                    )
                })?
                .to_owned();
            let reference = client_order_id(
                current.binding["physicalIdentity"].as_str().unwrap_or(""),
                &format!(
                    "portfolio-move:{}",
                    current.input["requestId"].as_str().unwrap_or("")
                ),
            );
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference,
                data: json!({"product":"coinbase","account":account,"targetAccount":target,"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::PortfolioMoveFunds,
                    account: account.into(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if matches!(
            current.operation.as_str(),
            "coinbase.create-portfolio" | "coinbase.rename-portfolio" | "coinbase.delete-portfolio"
        ) {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure("COINBASE_BINDING_INVALID", "Portfolio account is missing.")
            })?;
            let cfg = self.account(account, CoinbaseProduct::Advanced)?;
            self.verify_advanced_key(cfg, true, false).await?;
            let (endpoint, reference_suffix) = match current.operation.as_str() {
                "coinbase.create-portfolio" => (CoinbaseRoute::PortfolioCreate, "portfolio-create"),
                "coinbase.rename-portfolio" => {
                    let portfolio_id =
                        current.binding["portfolioId"].as_str().ok_or_else(|| {
                            failure(
                                "COINBASE_BINDING_INVALID",
                                "Bound portfolio identity is missing.",
                            )
                        })?;
                    let detail = self
                        .request_for_account(
                            account,
                            CoinbaseProduct::Advanced,
                            CoinbaseRoute::PortfolioDetail {
                                portfolio_id: portfolio_id.into(),
                            },
                            CoinbaseRequest::get(),
                            None,
                        )
                        .await?;
                    let doc: Value = serde_json::from_slice(&detail.body).map_err(|_| {
                        failure(
                            "COINBASE_RESPONSE_INVALID",
                            "Coinbase returned invalid portfolio detail.",
                        )
                    })?;
                    let row = doc.get("portfolio").unwrap_or(&doc);
                    if !(200..300).contains(&detail.status)
                        || row.get("uuid").and_then(Value::as_str) != Some(portfolio_id)
                        || row.get("name") != current.binding.get("previousName")
                        || row.get("deleted").and_then(Value::as_bool) != Some(false)
                    {
                        return Err(failure(
                            "COINBASE_PORTFOLIO_NOT_VISIBLE",
                            "The exact portfolio ID and reviewed previous name must still be current before rename dispatch.",
                        ));
                    }
                    (
                        CoinbaseRoute::PortfolioUpdate {
                            portfolio_id: portfolio_id.into(),
                        },
                        "portfolio-rename",
                    )
                }
                "coinbase.delete-portfolio" => {
                    let portfolio_id =
                        current.binding["portfolioId"].as_str().ok_or_else(|| {
                            failure("COINBASE_BINDING_INVALID", "Portfolio ID is missing.")
                        })?;
                    let (name, checked_accounts, checked_orders) =
                        self.verify_portfolio_empty(account, portfolio_id).await?;
                    if name != current.binding["previousName"]
                        || checked_accounts
                            != current.binding["checkedAccountCount"]
                                .as_u64()
                                .unwrap_or(u64::MAX) as usize
                        || checked_orders
                            != current.binding["checkedOpenOrderCount"]
                                .as_u64()
                                .unwrap_or(u64::MAX) as usize
                    {
                        return Err(failure(
                            "COINBASE_PORTFOLIO_CHANGED",
                            "Portfolio identity, account set, or order state changed after review.",
                        ));
                    }
                    (
                        CoinbaseRoute::PortfolioDelete {
                            portfolio_id: portfolio_id.into(),
                        },
                        "portfolio-delete",
                    )
                }
                _ => unreachable!(),
            };
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact portfolio configuration body is missing.",
                    )
                })?
                .to_owned();
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: client_order_id(
                    current.binding["physicalIdentity"].as_str().unwrap_or(""),
                    &format!(
                        "{reference_suffix}:{}",
                        current.input["requestId"].as_str().unwrap_or("")
                    ),
                ),
                data: json!({"product":"coinbase","account":account,"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint,
                    account: account.into(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation == "coinbase.set-cfm-margin-setting" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "CFM account identity is missing.",
                )
            })?;
            let cfg = self.account(account, CoinbaseProduct::Advanced)?;
            self.verify_advanced_key(cfg, true, false).await?;
            let observed = self
                .read(
                    "coinbase.read-v3-private-brokerage-cfm-intraday-margin-setting",
                    &json!({"account":account}),
                )
                .await?;
            if native_cfm_setting(&observed) != current.binding["previousSetting"].as_str() {
                return Err(failure(
                    "COINBASE_CFM_SETTING_CHANGED",
                    "The native margin setting changed since review; refresh configuration authority.",
                ));
            }
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact CFM setting body is missing.",
                    )
                })?
                .to_owned();
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: client_order_id(
                    current.binding["physicalIdentity"].as_str().unwrap_or(""),
                    &format!(
                        "cfm-margin:{}",
                        current.input["requestId"].as_str().unwrap_or("")
                    ),
                ),
                data: json!({"product":"coinbase","account":account,"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::CfmMarginSetting,
                    account: account.into(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation == "coinbase.edit-order" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Edit account identity is missing.",
                )
            })?;
            let cfg = self.account(account, CoinbaseProduct::Advanced)?;
            self.verify_advanced_key(cfg, true, false).await?;
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact edit request body is missing.",
                    )
                })?
                .to_owned();
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: client_order_id(
                    current.binding["physicalIdentity"].as_str().unwrap_or(""),
                    &format!("edit:{}", current.input["requestId"].as_str().unwrap_or("")),
                ),
                data: json!({"product":"coinbase","account":account,"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::OrderEdit,
                    account: account.into(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation == "coinbase.cancel-order" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Cancellation account identity is missing.",
                )
            })?;
            let account_config = self.account(account, CoinbaseProduct::Advanced)?;
            self.verify_advanced_key(account_config, true, false)
                .await?;
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact cancellation request body is missing.",
                    )
                })?
                .to_owned();
            let reference = client_order_id(
                current.binding["physicalIdentity"].as_str().unwrap_or(""),
                &format!(
                    "cancel:{}",
                    current.input["requestId"].as_str().unwrap_or("")
                ),
            );
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference,
                data: json!({"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::OrderCancel,
                    account: account.into(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation == "coinbase.cancel-orders" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Batch cancellation account is missing.",
                )
            })?;
            self.verify_advanced_key(
                self.account(account, CoinbaseProduct::Advanced)?,
                true,
                false,
            )
            .await?;
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact batch cancellation body is missing.",
                    )
                })?
                .to_owned();
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: client_order_id(
                    current.binding["physicalIdentity"].as_str().unwrap_or(""),
                    &format!(
                        "cancel-batch:{}",
                        current.input["requestId"].as_str().unwrap_or("")
                    ),
                ),
                data: json!({"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::OrderCancelBatch,
                    account: account.into(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation == "coinbase.edit-order" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Edit account identity is missing.",
                )
            })?;
            let cfg = self.account(account, CoinbaseProduct::Advanced)?;
            self.verify_advanced_key(cfg, true, false).await?;
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact order edit body is missing.",
                    )
                })?
                .to_owned();
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: client_order_id(
                    current.binding["physicalIdentity"].as_str().unwrap_or(""),
                    &format!("edit:{}", current.input["requestId"].as_str().unwrap_or("")),
                ),
                data: json!({"product":"coinbase","account":account,"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::OrderEdit,
                    account: account.into(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation == "coinbase.create-deposit-address" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure("COINBASE_BINDING_INVALID", "Address account is missing.")
            })?;
            let cfg = self.account(account, CoinbaseProduct::Advanced)?;
            self.verify_advanced_key(cfg, false, false).await?;
            let state = self
                .advanced_currency_account_details(
                    cfg,
                    current.binding["currencyAccountId"].as_str().unwrap_or(""),
                )
                .await?;
            let current_currency = state
                .get("currency")
                .and_then(|value| {
                    value
                        .get("code")
                        .or_else(|| value.get("id"))
                        .or(Some(value))
                })
                .and_then(Value::as_str)
                .or_else(|| state.get("currency_id").and_then(Value::as_str));
            if current_currency != current.binding["currency"].as_str() {
                return Err(failure(
                    "COINBASE_CURRENCY_ACCOUNT_MISMATCH",
                    "The currency account identity changed after review.",
                ));
            }
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: client_order_id(
                    current.binding["physicalIdentity"].as_str().unwrap_or(""),
                    &format!(
                        "deposit-address:{}",
                        current.input["requestId"].as_str().unwrap_or("")
                    ),
                ),
                data: json!({"product":"coinbase","account":account,"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::DepositAddressCreate {
                        account_id: current.binding["currencyAccountId"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                    },
                    account: account.to_owned(),
                    exact_body: String::new(),
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation == "coinbase.convert" {
            let account = current.binding["account"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Conversion account identity is missing.",
                )
            })?;
            let trade_id = current.binding["tradeId"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Conversion quote trade ID is missing.",
                )
            })?;
            self.verify_advanced_key(
                self.account(account, CoinbaseProduct::Advanced)?,
                true,
                false,
            )
            .await?;
            let body = current.binding["requestBodyExact"]
                .as_str()
                .ok_or_else(|| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Exact conversion commit body is missing.",
                    )
                })?
                .to_owned();
            let submission = gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: trade_id.to_owned(),
                data: json!({"product":"coinbase","account":account,"tradeId":trade_id,"binding":current.binding}),
            };
            return Ok(PlannedSubmission {
                submission: submission.clone(),
                request: CoinbaseDispatch {
                    product: CoinbaseProduct::Advanced,
                    endpoint: CoinbaseRoute::ConvertCommit {
                        trade_id: trade_id.to_owned(),
                    },
                    account: account.to_owned(),
                    exact_body: body,
                    submission,
                    candidate: current.clone(),
                },
            });
        }
        if current.operation != "coinbase.create-order" {
            return Err(failure(
                "WRONG_OPERATION_KIND",
                "Only typed Coinbase spot orders are currently submittable.",
            ));
        }
        let account = current.binding["account"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Prepared order account identity is missing.",
            )
        })?;
        let account_config = self.account(account, CoinbaseProduct::Advanced)?;
        let permission_was_finalized = current.checks.iter().any(|check| {
            check.name == "advanced_trade_permission_finalized"
                && check.status == CheckStatus::Passed
        });
        if !permission_was_finalized {
            // Keep direct profile callers safe. The engine's reserved path
            // carries the adapter-produced check from finalize_after_reservation
            // and skips only this redundant read.
            self.verify_advanced_key(account_config, true, false)
                .await?;
        }
        let body = current.binding["requestBodyExact"]
            .as_str()
            .ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Prepared exact order body is missing.",
                )
            })?
            .to_owned();
        let client_id = current.binding["clientOrderId"]
            .as_str()
            .ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Prepared client order identity is missing.",
                )
            })?
            .to_owned();
        self.assert_unique_client_order_id(CoinbaseProduct::Advanced, &client_id, None)?;
        Ok(PlannedSubmission {
            submission: gofin_domain::Submission {
                kind: "coinbase".into(),
                reference: client_id.clone(),
                data: json!({"product":"coinbase","clientOrderId":client_id,"account":account,"productId":current.binding["productId"]}),
            },
            request: CoinbaseDispatch {
                product: CoinbaseProduct::Advanced,
                endpoint: CoinbaseRoute::Orders,
                account: account.into(),
                exact_body: body,
                submission: gofin_domain::Submission {
                    kind: "coinbase".into(),
                    reference: client_id.clone(),
                    data: json!({"product":"coinbase","clientOrderId":client_id,"account":account,"productId":current.binding["productId"]}),
                },
                candidate: current.clone(),
            },
        })
    }
    async fn dispatch(
        &self,
        request: &Self::Request,
        journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value> {
        if let Err(error) = journal.assert_authorized() {
            persist_no_dispatch(request, journal.as_ref(), "authorization_gate", None)?;
            return Err(error);
        }
        if request.candidate.operation == "coinbase.withdraw"
            && let Err(error) = self
                .revalidate_withdrawal_before_dispatch(&request.candidate)
                .await
        {
            persist_no_dispatch(
                request,
                journal.as_ref(),
                "withdrawal_pre_dispatch_revalidation",
                None,
            )?;
            return Err(error);
        }
        let guard_journal = journal.clone();
        let candidate = request.candidate.clone();
        let guard_diagnostic = Arc::new(Mutex::new(None::<Value>));
        let guard_error = Arc::new(Mutex::new(None::<GoFinError>));
        let diagnostic_slot = guard_diagnostic.clone();
        let error_slot = guard_error.clone();
        let guard: gofin_exchange::coinbase::DispatchGuard = Arc::new(move || {
            let reject = |reason: &str, error: Option<&GoFinError>| {
                let diagnostic = json!({
                    "reason":reason,
                    "journalErrorCode":error.map(|error|error.code.as_str()),
                    "journalErrorStage":error.map(|error|error.stage.as_str())
                });
                if let Ok(mut slot) = diagnostic_slot.lock() {
                    *slot = Some(diagnostic);
                }
                if let Some(error) = error
                    && let Ok(mut slot) = error_slot.lock()
                {
                    *slot = Some(error.clone());
                }
                false
            };
            let unexpired = chrono::DateTime::parse_from_rfc3339(&candidate.expires_at)
                .is_ok_and(|expires| Utc::now() < expires);
            if !unexpired {
                return reject("prepared_candidate_expired", None);
            }
            if let Err(error) = guard_journal.assert_candidate(&candidate) {
                return reject("prepared_candidate_rejected", Some(&error));
            }
            if let Err(error) = guard_journal.assert_authorized() {
                return reject("authorization_rejected", Some(&error));
            }
            true
        });
        let request_body = if request.exact_body.is_empty() {
            CoinbaseRequest::get()
        } else {
            CoinbaseRequest::json(request.exact_body.clone())
        };
        let response = match self
            .request_for_account(
                &request.account,
                request.product,
                request.endpoint.clone(),
                request_body,
                Some(guard),
            )
            .await
        {
            Ok(response) => response,
            Err(error) if is_definitive_predispatch_error(&error.code) => {
                let reason = match error.code.as_str() {
                    "COINBASE_DISPATCH_GUARD_REJECTED" => "transport_guard_rejected",
                    "COINBASE_CREDENTIAL_INVALID" => "credential_rejected_before_send",
                    "COINBASE_ROUTE_INVALID" => "closed_route_rejected_before_send",
                    _ => "pre_dispatch_rejected",
                };
                let diagnostic = guard_diagnostic.lock().ok().and_then(|value| value.clone());
                persist_no_dispatch(request, journal.as_ref(), reason, diagnostic.as_ref())?;
                let journal_error = guard_error.lock().ok().and_then(|value| value.clone());
                if let (Some(journal_error), Some(diagnostic)) = (journal_error, diagnostic) {
                    return Err(preserve_guard_journal_error(journal_error, &diagnostic));
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let evidence = json!({"httpStatus":response.status,"bodyHex":hex::encode(&response.body),"bodySha256":hex::encode(Sha256::digest(&response.body))});
        journal.persist_dispatch_evidence(&request.submission, &evidence)?;
        let body: Value = serde_json::from_slice(&response.body)
            .unwrap_or_else(|_| json!({"raw":"invalid-json"}));
        Ok(json!({"status":response.status,"body":body}))
    }
    async fn reconcile_submission(
        &self,
        record: &OperationRecord,
        submission: &gofin_domain::Submission,
    ) -> Result<Reconciliation> {
        if let Some(certificate) = validated_no_dispatch_certificate(record, submission) {
            let evidence = json!({"noDispatch":certificate});
            let mut verified = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects: Vec::new(),
                evidence: evidence.clone(),
                observed_at: gofin_domain::now(),
                proof_digest: String::new(),
            };
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
            return Ok(Reconciliation {
                status: Status::Failed,
                evidence,
                verified: Some(verified),
            });
        }
        if record.operation == "coinbase.withdraw" {
            return self.reconcile_withdrawal(record, submission).await;
        }
        if record.operation == "coinbase.set-cfm-margin-setting" && submission.kind == "coinbase" {
            let binding = &submission.data["binding"];
            if record
                .preview
                .as_ref()
                .is_none_or(|p| p.operation != record.operation || p.binding != *binding)
            {
                return Ok(coinbase_unknown(
                    "CFM setting submission differs from its exact protected preview",
                ));
            }
            if !self.configuration_binding_matches(binding) {
                return Ok(coinbase_unknown(
                    "CFM setting account identity no longer matches the enrolled account mapping",
                ));
            }
            let Some(dispatch) = record.dispatch_evidence.as_ref() else {
                return Ok(coinbase_unknown(
                    "CFM setting has no captured response and will not be replayed",
                ));
            };
            let Some((code, receipt)) = captured_configuration_response(dispatch) else {
                return Ok(coinbase_unknown(
                    "CFM setting response digest or bytes are invalid",
                ));
            };
            if (400..500).contains(&code) {
                return Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: json!({"httpStatus":code,"receipt":receipt,"reason":"Coinbase rejected the CFM setting request"}),
                    verified: None,
                });
            }
            if !(200..300).contains(&code) || !receipt.is_object() {
                return Ok(coinbase_unknown(
                    "captured CFM setting receipt is not a successful native JSON object",
                ));
            }
            let account = binding["account"]
                .as_str()
                .ok_or_else(|| failure("COINBASE_BINDING_INVALID", "CFM account is missing."))?;
            let current = match self
                .read(
                    "coinbase.read-v3-private-brokerage-cfm-intraday-margin-setting",
                    &json!({"account":account}),
                )
                .await
            {
                Ok(v) => v,
                Err(_) => {
                    return Ok(coinbase_unknown(
                        "native CFM setting readback is unavailable",
                    ));
                }
            };
            if native_cfm_setting(&current) != binding["setting"].as_str() {
                return Ok(coinbase_unknown(
                    "native CFM setting readback does not prove the requested mode",
                ));
            }
            return Ok(Reconciliation {
                status: Status::Completed,
                evidence: json!({"httpStatus":code,"receipt":receipt,"readback":current,"setting":binding["setting"],"exactBytesDigest":dispatch["bodySha256"]}),
                verified: None,
            });
        }
        if matches!(
            record.operation.as_str(),
            "coinbase.create-portfolio" | "coinbase.rename-portfolio" | "coinbase.delete-portfolio"
        ) {
            if record
                .preview
                .as_ref()
                .is_none_or(|prepared| prepared.binding != submission.data["binding"])
            {
                return Ok(coinbase_unknown(
                    "configuration submission does not match its exact durable preview",
                ));
            }
            if !self.configuration_binding_matches(&submission.data["binding"]) {
                return Ok(coinbase_unknown(
                    "portfolio account identity no longer matches the enrolled account mapping",
                ));
            }
            let Some(dispatch) = record.dispatch_evidence.as_ref() else {
                return Ok(coinbase_unknown(
                    "portfolio configuration has no captured native receipt; it will not be replayed",
                ));
            };
            let Some((status, body)) = captured_configuration_response(dispatch) else {
                return Ok(coinbase_unknown(
                    "portfolio configuration response digest or bytes are invalid",
                ));
            };
            let response = json!({"status":status,"body":body});
            if record.operation == "coinbase.delete-portfolio" {
                if self.validate_response(submission, &response).is_err() {
                    return Ok(if (400..500).contains(&status) {
                        Reconciliation {
                            status: Status::Failed,
                            evidence: json!({"httpStatus":status,"nativeReceipt":body,"reason":"Coinbase definitively rejected this portfolio deletion"}),
                            verified: None,
                        }
                    } else {
                        coinbase_unknown("captured delete receipt does not prove native success")
                    });
                }
                let portfolio_id = submission.data["binding"]["portfolioId"]
                    .as_str()
                    .ok_or_else(|| {
                        failure("COINBASE_BINDING_INVALID", "Portfolio ID is missing.")
                    })?;
                let listed = match self
                    .request_for_account(
                        submission.data["account"].as_str().unwrap_or(""),
                        CoinbaseProduct::Advanced,
                        CoinbaseRoute::Portfolios,
                        CoinbaseRequest::get(),
                        None,
                    )
                    .await
                {
                    Ok(value) if (200..300).contains(&value.status) => value,
                    Ok(_) => {
                        return Ok(coinbase_unknown(
                            "portfolio list readback is unavailable after deletion",
                        ));
                    }
                    Err(error) => {
                        return Ok(coinbase_unknown_request(
                            "portfolio list readback failed after deletion",
                            &error,
                        ));
                    }
                };
                let list: Value = serde_json::from_slice(&listed.body).map_err(|_| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Coinbase returned malformed portfolio readback.",
                    )
                })?;
                let rows = list
                    .get("portfolios")
                    .or_else(|| list.get("data"))
                    .and_then(Value::as_array);
                let matching = rows
                    .into_iter()
                    .flatten()
                    .filter(|row| row.get("uuid").and_then(Value::as_str) == Some(portfolio_id))
                    .collect::<Vec<_>>();
                if matching.len() != 1
                    || matching[0].get("deleted").and_then(Value::as_bool) != Some(true)
                    || matching[0].get("name") != submission.data["binding"].get("previousName")
                {
                    return Ok(coinbase_unknown(
                        "portfolio list does not prove the exact portfolio was deleted",
                    ));
                }
                return Ok(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"httpStatus":status,"nativeReceipt":body,"deletedPortfolio":matching[0],"exactBytesDigest":dispatch["bodySha256"]}),
                    verified: None,
                });
            }
            return match self.validate_response(submission, &response) {
                Ok(()) => Ok(Reconciliation {
                    status: Status::Completed,
                    evidence: json!({"httpStatus":status,"nativeReceipt":body,"exactBytesDigest":dispatch["bodySha256"],"submissionReference":submission.reference}),
                    verified: None,
                }),
                Err(_) if (400..500).contains(&status) => Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: json!({"httpStatus":status,"nativeReceipt":body,"reason":"Coinbase definitively rejected this configuration request"}),
                    verified: None,
                }),
                Err(_) => Ok(coinbase_unknown(
                    "captured portfolio configuration response does not prove the exact requested resulting state",
                )),
            };
        }
        if let Some(reconciliation) =
            crate::institutional::reconcile(self, record, submission).await?
        {
            return Ok(reconciliation);
        }
        if record.operation == "coinbase.move-portfolio-funds" {
            return self.reconcile_portfolio_move(record, submission).await;
        }
        if record.operation == "coinbase.convert" {
            return self.reconcile_conversion(record, submission).await;
        }
        if record.operation == "coinbase.edit-order" && submission.kind == "coinbase" {
            let binding = &submission.data["binding"];
            let Some(prepared) = record
                .preview
                .as_ref()
                .filter(|p| p.operation == "coinbase.edit-order" && p.binding == *binding)
            else {
                return Ok(coinbase_unknown(
                    "persisted edit preview does not match its durable submission identity",
                ));
            };
            let (
                Some(account),
                Some(order_id),
                Some(client_id),
                Some(product_id),
                Some(operation_id),
                Some(new_price),
            ) = (
                binding["account"].as_str(),
                binding["orderId"].as_str(),
                binding["clientOrderId"].as_str(),
                binding["productId"].as_str(),
                binding["operationId"].as_str(),
                binding["newPrice"].as_str(),
            )
            else {
                return Ok(coinbase_unknown("durable edit binding is incomplete"));
            };
            let Some(dispatch) = record.dispatch_evidence.as_ref() else {
                return Ok(coinbase_unknown(
                    "edit dispatch has no captured response; the edit will not be replayed",
                ));
            };
            let status_code = dispatch["httpStatus"].as_u64().unwrap_or(0) as u16;
            let response_body = dispatch["bodyHex"]
                .as_str()
                .and_then(|hex| hex::decode(hex).ok())
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            let Some(response_body) = response_body else {
                return Ok(coinbase_unknown(
                    "captured edit response cannot be interpreted",
                ));
            };
            if !(200..300).contains(&status_code)
                || response_body.get("success").and_then(Value::as_bool) != Some(true)
            {
                return Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: json!({"dispatch":dispatch,"reason":"Coinbase did not confirm the edit"}),
                    verified: None,
                });
            }
            let response = self
                .request_for_account(
                    account,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Order {
                        order_id: order_id.into(),
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Ok(coinbase_unknown("edited order cannot yet be verified"));
            }
            let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid edited order data.",
                )
            })?;
            let order = doc.get("order").unwrap_or(&doc);
            let status = order["status"].as_str().unwrap_or("").to_ascii_uppercase();
            let config = &order["order_configuration"]["limit_limit_gtc"];
            let old_size=prepared.binding["originalInput"]["orderConfiguration"]["limit_limit_gtc"]["base_size"].as_str();
            let new_size = config["base_size"].as_str();
            if order["order_id"].as_str() != Some(order_id)
                || order["client_order_id"].as_str() != Some(client_id)
                || order["product_id"].as_str() != Some(product_id)
                || config["limit_price"].as_str() != Some(new_price)
                || old_size.is_none()
                || old_size != new_size
                || !matches!(status.as_str(), "OPEN" | "PENDING" | "QUEUED")
            {
                return Ok(coinbase_unknown(
                    "captured edit acknowledgment and authoritative order state do not prove the exact requested revision",
                ));
            }
            let original: gofin_domain::Submission =
                serde_json::from_value(binding["originalSubmission"].clone()).map_err(|_| {
                    failure(
                        "COINBASE_BINDING_INVALID",
                        "Original order submission is invalid.",
                    )
                })?;
            let effect = FinancialEffect::SubmissionAmendment {
                operation_id: operation_id.into(),
                submission: original,
                successor: None,
            };
            let evidence = json!({"dispatch":dispatch,"order":order,"editPrice":new_price,"editSize":new_size,"originalOperationId":operation_id});
            let mut verified = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects: vec![effect],
                evidence: evidence.clone(),
                observed_at: gofin_domain::now(),
                proof_digest: String::new(),
            };
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
            return Ok(Reconciliation {
                status: Status::Completed,
                evidence,
                verified: Some(verified),
            });
        }
        if record.operation == "coinbase.cancel-order" && submission.kind == "coinbase" {
            let binding = &submission.data["binding"];
            let (
                Some(account),
                Some(order_id),
                Some(client_id),
                Some(product_id),
                Some(operation_id),
            ) = (
                binding["account"].as_str(),
                binding["orderId"].as_str(),
                binding["clientOrderId"].as_str(),
                binding["productId"].as_str(),
                binding["operationId"].as_str(),
            )
            else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"durable cancellation binding is incomplete"}),
                    verified: None,
                });
            };
            let Some(account_config) = self
                .accounts
                .iter()
                .find(|entry| entry.name == account && entry.product == CoinbaseProduct::Advanced)
            else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"persisted Advanced account is no longer enrolled"}),
                    verified: None,
                });
            };
            if self
                .verify_advanced_key(account_config, false, false)
                .await
                .is_err()
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"current key permissions no longer attest the persisted portfolio"}),
                    verified: None,
                });
            }
            let original: gofin_domain::Submission = match serde_json::from_value(
                binding["originalSubmission"].clone(),
            ) {
                Ok(value) => value,
                Err(_) => {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"reason":"original order submission binding is invalid"}),
                        verified: None,
                    });
                }
            };
            if record.submission.as_ref() != Some(submission)
                || record.preview.as_ref().is_none_or(|preview| {
                    preview.operation != "coinbase.cancel-order" || preview.binding != *binding
                })
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"cancellation record differs from its prepared linked binding"}),
                    verified: None,
                });
            }
            let state = self
                .execution_state
                .read()
                .ok()
                .and_then(|state| state.clone());
            let Some(state) = state else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"protected execution history is unavailable; retain original reservation"}),
                    verified: None,
                });
            };
            let parent = state.operation(operation_id)?;
            if parent.as_ref().is_none_or(|parent| {
                parent.operation != "coinbase.create-order"
                    || parent.submission.as_ref() != Some(&original)
                    || parent.preview.as_ref().is_none_or(|preview| {
                        preview.binding["clientOrderId"].as_str() != Some(client_id)
                            || preview.binding["productId"].as_str() != Some(product_id)
                    })
                    || !matches!(
                        parent.status,
                        Status::Submitted | Status::Pending | Status::Unknown
                    )
            }) {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"linked original Fin operation is no longer an unresolved exact match"}),
                    verified: None,
                });
            }
            if original.kind != "coinbase"
                || original.reference != client_id
                || submission.data["binding"]["orderId"] != order_id
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"reason":"cancellation is detached from its immutable original order identity"}),
                    verified: None,
                });
            }
            let response = match self
                .request_for_account(
                    account,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Order {
                        order_id: order_id.to_owned(),
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await
            {
                Ok(value) => value,
                Err(_) => {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"orderId":order_id,"reason":"order status is unavailable; retain original reservation"}),
                        verified: None,
                    });
                }
            };
            if !(200..300).contains(&response.status) {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"order status is unavailable; retain original reservation"}),
                    verified: None,
                });
            }
            let doc: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid cancellation reconciliation data.",
                )
            })?;
            let order = doc.get("order").unwrap_or(&doc);
            if order
                .get("order_id")
                .or_else(|| order.get("orderId"))
                .and_then(Value::as_str)
                != Some(order_id)
                || order
                    .get("client_order_id")
                    .or_else(|| order.get("clientOrderId"))
                    .and_then(Value::as_str)
                    != Some(client_id)
                || order
                    .get("product_id")
                    .or_else(|| order.get("productId"))
                    .and_then(Value::as_str)
                    != Some(product_id)
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"cancellation lookup did not prove the original order identity"}),
                    verified: None,
                });
            }
            let state = order
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_uppercase();
            if matches!(
                state.as_str(),
                "OPEN" | "PENDING" | "QUEUED" | "PARTIALLY_FILLED" | "PENDING_CANCEL"
            ) {
                return Ok(Reconciliation {
                    status: Status::Pending,
                    evidence: json!({"orderId":order_id,"venueStatus":state,"operationId":operation_id,"openRemainder":true}),
                    verified: None,
                });
            }
            if !matches!(
                state.as_str(),
                "CANCELLED" | "CANCELED" | "EXPIRED" | "FILLED" | "REJECTED" | "FAILED"
            ) {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"venueStatus":state,"reason":"unclassified cancellation target state"}),
                    verified: None,
                });
            }
            let evidence = json!({"orderId":order_id,"venueStatus":state,"operationId":operation_id,"openRemainder":false,"accountingOwner":"original_operation","executedValue":order.get("filled_value"),"filledSize":order.get("filled_size")});
            let mut verified = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects: vec![FinancialEffect::SubmissionCancellation {
                    operation_id: operation_id.to_owned(),
                    submission: original,
                }],
                evidence: evidence.clone(),
                observed_at: gofin_domain::now(),
                proof_digest: String::new(),
            };
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
            return Ok(Reconciliation {
                status: Status::Completed,
                evidence,
                verified: Some(verified),
            });
        }
        if record.operation == "coinbase.cancel-orders" && submission.kind == "coinbase" {
            let binding = &submission.data["binding"];
            let Some(prepared) = record
                .preview
                .as_ref()
                .filter(|p| p.operation == record.operation && p.binding == *binding)
            else {
                return Ok(coinbase_unknown(
                    "batch cancellation preview does not match its durable submission",
                ));
            };
            let Some(dispatch) = record.dispatch_evidence.as_ref() else {
                return Ok(coinbase_unknown(
                    "batch cancellation has no captured response and will not be replayed",
                ));
            };
            let status_code = dispatch["httpStatus"].as_u64().unwrap_or(0) as u16;
            let response_body = dispatch["bodyHex"]
                .as_str()
                .and_then(|v| hex::decode(v).ok())
                .and_then(|v| serde_json::from_slice::<Value>(&v).ok());
            let Some(body) = response_body else {
                return Ok(coinbase_unknown(
                    "batch cancellation response bytes are unavailable or malformed",
                ));
            };
            if !(200..300).contains(&status_code) {
                return Ok(if (400..500).contains(&status_code) {
                    Reconciliation {
                        status: Status::Failed,
                        evidence: json!({"dispatch":dispatch,"reason":"Coinbase rejected the batch cancellation"}),
                        verified: None,
                    }
                } else {
                    coinbase_unknown("batch cancellation outcome is not definitive")
                });
            }
            let results = body.get("results").and_then(Value::as_array);
            let Some(results) = results else {
                return Ok(coinbase_unknown(
                    "successful batch response omitted its native per-order results array",
                ));
            };
            let Some(rows) = binding["orders"].as_array() else {
                return Ok(coinbase_unknown(
                    "durable batch order bindings are incomplete",
                ));
            };
            if results.len() != rows.len() {
                return Ok(coinbase_unknown(
                    "native batch result count differs from the persisted requested order set",
                ));
            }
            let mut requested = BTreeSet::new();
            for row in rows {
                requested.insert(row["orderId"].as_str().unwrap_or("").to_owned());
            }
            let mut seen = BTreeSet::new();
            for result in results {
                let id = result["order_id"].as_str().unwrap_or("");
                if !requested.contains(id)
                    || !seen.insert(id.to_owned())
                    || result["success"].as_bool() != Some(true)
                {
                    return Ok(coinbase_unknown(
                        "batch response contains a missing, duplicate, failed, or unrequested order result",
                    ));
                }
            }
            let account = binding["account"]
                .as_str()
                .ok_or_else(|| failure("COINBASE_BINDING_INVALID", "Batch account is missing."))?;
            let mut effects = Vec::new();
            let mut states = Vec::new();
            for row in rows {
                let (id, client, product, operation_id) = (
                    row["orderId"].as_str().unwrap_or(""),
                    row["clientOrderId"].as_str().unwrap_or(""),
                    row["productId"].as_str().unwrap_or(""),
                    row["operationId"].as_str().unwrap_or(""),
                );
                let original: gofin_domain::Submission =
                    serde_json::from_value(row["originalSubmission"].clone()).map_err(|_| {
                        failure(
                            "COINBASE_BINDING_INVALID",
                            "Batch original submission binding is invalid.",
                        )
                    })?;
                let response = match self
                    .request_for_account(
                        account,
                        CoinbaseProduct::Advanced,
                        CoinbaseRoute::Order {
                            order_id: id.into(),
                        },
                        CoinbaseRequest::get(),
                        None,
                    )
                    .await
                {
                    Ok(r) if (200..300).contains(&r.status) => r,
                    _ => {
                        return Ok(coinbase_unknown(
                            "one batch order could not be independently read after cancellation",
                        ));
                    }
                };
                let document: Value = serde_json::from_slice(&response.body).map_err(|_| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Coinbase returned invalid batch order readback.",
                    )
                })?;
                let order = document.get("order").unwrap_or(&document);
                let state = order["status"].as_str().unwrap_or("").to_ascii_uppercase();
                if order["order_id"].as_str() != Some(id)
                    || order["client_order_id"].as_str() != Some(client)
                    || order["product_id"].as_str() != Some(product)
                {
                    return Ok(coinbase_unknown(
                        "batch order readback does not prove the exact requested identity",
                    ));
                }
                if matches!(
                    state.as_str(),
                    "OPEN" | "PENDING" | "QUEUED" | "PARTIALLY_FILLED" | "PENDING_CANCEL"
                ) {
                    states.push(json!({"orderId":id,"status":state}));
                    continue;
                }
                if !matches!(
                    state.as_str(),
                    "CANCELLED" | "CANCELED" | "EXPIRED" | "FILLED" | "REJECTED" | "FAILED"
                ) {
                    return Ok(coinbase_unknown(
                        "batch order has an unclassified post-cancel status",
                    ));
                }
                effects.push(FinancialEffect::SubmissionCancellation {
                    operation_id: operation_id.into(),
                    submission: original,
                });
                states.push(json!({"orderId":id,"status":state}));
            }
            let outcome = if states.iter().any(|v| {
                matches!(
                    v["status"].as_str(),
                    Some("OPEN" | "PENDING" | "QUEUED" | "PARTIALLY_FILLED" | "PENDING_CANCEL")
                )
            }) {
                Status::Pending
            } else {
                Status::Completed
            };
            let evidence = json!({"dispatch":dispatch,"orders":states,"verifiedCancellations":effects.len(),"requested":rows.len()});
            let verified = if effects.is_empty() {
                None
            } else {
                let mut v = VerifiedEffects {
                    evidence_version: 1,
                    submission: submission.clone(),
                    effects,
                    evidence: evidence.clone(),
                    observed_at: gofin_domain::now(),
                    proof_digest: String::new(),
                };
                v.proof_digest = gofin_domain::verified_effect_digest(&v)?;
                Some(v)
            };
            let _ = prepared;
            return Ok(Reconciliation {
                status: outcome,
                evidence,
                verified,
            });
        }
        if record.operation != "coinbase.create-order" || submission.kind != "coinbase" {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"this Coinbase submission has no implemented reconciliation route"}),
                verified: None,
            });
        }
        let Some(prepared) = record.preview.as_ref().filter(|prepared| {
            prepared.operation == "coinbase.create-order"
                && prepared.binding["clientOrderId"].as_str()
                    == submission.data["clientOrderId"].as_str()
                && prepared.binding["productId"].as_str() == submission.data["productId"].as_str()
                && prepared.binding["account"].as_str() == submission.data["account"].as_str()
                && prepared.binding["clientOrderId"].as_str() == Some(submission.reference.as_str())
        }) else {
            return Ok(coinbase_unknown(
                "persisted create-order preview does not bind this exact request identity",
            ));
        };
        if self
            .assert_unique_client_order_id(
                CoinbaseProduct::Advanced,
                submission.reference.as_str(),
                Some(record.id.as_str()),
            )
            .is_err()
        {
            return Ok(coinbase_unknown(
                "another durable operation already owns this native client order ID; effects cannot be assigned unambiguously",
            ));
        }
        // Resolve an explicit rejection for the exact durable create request
        // before consulting order history. Missing history alone cannot prove
        // that an uncertain request was rejected.
        if let Some(dispatch) = record.dispatch_evidence.as_ref()
            && let (Some(status), Some(encoded), Some(expected_hash)) = (
                dispatch["httpStatus"].as_u64().map(|v| v as u16),
                dispatch["bodyHex"].as_str(),
                dispatch["bodySha256"].as_str(),
            )
        {
            let bytes = hex::decode(encoded).map_err(|_| {
                failure(
                    "COINBASE_EVIDENCE_INVALID",
                    "Persisted dispatch evidence is malformed.",
                )
            })?;
            if hex::encode(Sha256::digest(&bytes)) != expected_hash {
                return Err(failure(
                    "COINBASE_EVIDENCE_INVALID",
                    "Persisted dispatch evidence digest does not match its exact bytes.",
                ));
            }
            if let Ok(receipt) = serde_json::from_slice::<Value>(&bytes)
                && is_conclusive_create_order_rejection(status, &receipt)
            {
                let evidence = json!({
                    "httpStatus":status,
                    "nativeRejection":receipt,
                    "clientOrderId":submission.reference,
                    "productId":prepared.binding["productId"],
                    "requestBodySha256":hex::encode(Sha256::digest(prepared.binding["requestBodyExact"].as_str().unwrap_or("").as_bytes())),
                    "responseBodySha256":expected_hash,
                    "reason":"Coinbase explicitly rejected this create-order request; it will not be replayed"
                });
                let mut verified = VerifiedEffects {
                    evidence_version: 1,
                    submission: submission.clone(),
                    effects: Vec::new(),
                    evidence: evidence.clone(),
                    observed_at: gofin_domain::now(),
                    proof_digest: String::new(),
                };
                verified.proof_digest = gofin_domain::verified_effect_digest(&verified)?;
                return Ok(Reconciliation {
                    status: Status::Failed,
                    evidence,
                    verified: Some(verified),
                });
            }
        }
        let Some(account_name) = submission.data["account"].as_str() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"submission account identity is missing"}),
                verified: None,
            });
        };
        let Some(account) = self.accounts.iter().find(|account| {
            account.name == account_name && account.product == CoinbaseProduct::Advanced
        }) else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"persisted Advanced account is no longer enrolled"}),
                verified: None,
            });
        };
        if self
            .verify_advanced_key(account, false, false)
            .await
            .is_err()
        {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"current key permissions no longer attest the persisted portfolio"}),
                verified: None,
            });
        }
        let Some(client_id) = submission.data["clientOrderId"].as_str() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"durable client order id is missing"}),
                verified: None,
            });
        };
        let Some(product_id) = submission.data["productId"].as_str() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"durable product identity is missing"}),
                verified: None,
            });
        };
        // Prefer the exact order ID returned by the original dispatch when its
        // byte-exact evidence is present. A lost/invalidated response falls
        // back to a fully paginated, product-scoped order history scan using
        // the immutable client order ID stored before dispatch.
        let mut order_id = None;
        if let Some(dispatch) = record.dispatch_evidence.as_ref()
            && let (Some(encoded), Some(expected_hash)) = (
                dispatch["bodyHex"].as_str(),
                dispatch["bodySha256"].as_str(),
            )
        {
            let bytes = hex::decode(encoded).map_err(|_| {
                failure(
                    "COINBASE_EVIDENCE_INVALID",
                    "Persisted dispatch evidence is malformed.",
                )
            })?;
            if hex::encode(Sha256::digest(&bytes)) != expected_hash {
                return Err(failure(
                    "COINBASE_EVIDENCE_INVALID",
                    "Persisted dispatch evidence digest does not match its exact bytes.",
                ));
            }
            let response: Value = serde_json::from_slice(&bytes).map_err(|_| {
                failure(
                    "COINBASE_EVIDENCE_INVALID",
                    "Persisted order response is not valid JSON.",
                )
            })?;
            let root = response.get("success_response").unwrap_or(&response);
            order_id = root
                .get("order_id")
                .or_else(|| root.get("orderId"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if order_id.is_none() {
            let mut cursor = None;
            let mut seen = BTreeSet::new();
            let mut match_row: Option<Value> = None;
            for page_num in 0..1000 {
                let response = match self
                    .request_for_account(
                        account_name,
                        CoinbaseProduct::Advanced,
                        CoinbaseRoute::HistoricalOrders {
                            query: CoinbaseQuery {
                                cursor: cursor.clone(),
                                limit: Some(250),
                                product_ids: vec![product_id.to_owned()],
                                ..Default::default()
                            },
                        },
                        CoinbaseRequest::get(),
                        None,
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        return Ok(coinbase_unknown_request(
                            "venue history request failed; the persisted order may still exist and will not be replayed",
                            &error,
                        ));
                    }
                };
                if !(200..300).contains(&response.status) {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"clientOrderId":client_id,"reason":"venue history scan is unavailable; do not replay"}),
                        verified: None,
                    });
                }
                let page: Value = serde_json::from_slice(&response.body).map_err(|_| {
                    failure(
                        "COINBASE_RESPONSE_INVALID",
                        "Coinbase returned invalid order history.",
                    )
                })?;
                let rows = page
                    .get("orders")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_RESPONSE_INVALID",
                            "Historical order page omitted orders.",
                        )
                    })?;
                for row in rows {
                    if row
                        .get("client_order_id")
                        .or_else(|| row.get("clientOrderId"))
                        .and_then(Value::as_str)
                        == Some(client_id)
                    {
                        if match_row.as_ref().is_some_and(|prior| prior != row) {
                            return Err(failure(
                                "COINBASE_ORDER_IDENTITY_CONFLICT",
                                "Multiple venue orders claim the same persisted client order ID.",
                            ));
                        }
                        match_row = Some(row.clone());
                    }
                }
                if page.get("has_next").and_then(Value::as_bool) != Some(true) {
                    break;
                }
                let next = page
                    .get("cursor")
                    .or_else(|| page.get("next_cursor"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_PAGINATION_INVALID",
                            "Historical order page omitted its continuation cursor.",
                        )
                    })?
                    .to_owned();
                if !seen.insert(next.clone()) || page_num == 999 {
                    return Err(failure(
                        "COINBASE_PAGINATION_INVALID",
                        "Historical order scan did not advance safely.",
                    ));
                }
                cursor = Some(next);
            }
            order_id = match_row
                .as_ref()
                .and_then(|row| row.get("order_id").or_else(|| row.get("orderId")))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        let Some(order_id) = order_id else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"clientOrderId":client_id,"productId":product_id,"reason":"complete venue history scan found no matching order; non-execution is not proven"}),
                verified: None,
            });
        };
        let order_response = match self
            .request_for_account(
                account_name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Order {
                    order_id: order_id.to_owned(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return Ok(coinbase_unknown_request(
                    "venue order lookup failed; retain the original reservation and do not replay",
                    &error,
                ));
            }
        };
        if !(200..300).contains(&order_response.status) {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"orderId":order_id,"reason":"venue order lookup is unavailable"}),
                verified: None,
            });
        }
        let order_doc: Value = serde_json::from_slice(&order_response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid order status.",
            )
        })?;
        let order = order_doc.get("order").unwrap_or(&order_doc);
        let returned_order_id = order
            .get("order_id")
            .or_else(|| order.get("orderId"))
            .and_then(Value::as_str);
        let returned_product_id = order
            .get("product_id")
            .or_else(|| order.get("productId"))
            .and_then(Value::as_str);
        if returned_order_id != Some(order_id.as_str())
            || returned_product_id != Some(product_id)
            || order
                .get("client_order_id")
                .or_else(|| order.get("clientOrderId"))
                .and_then(Value::as_str)
                != Some(client_id)
        {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"orderId":order_id,"reason":"venue order ID, product ID or client order ID does not match the persisted submission"}),
                verified: None,
            });
        }
        let Some(prepared) = record.preview.as_ref() else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"orderId":order_id,"reason":"prepared order binding is missing"}),
                verified: None,
            });
        };
        let base = prepared.binding["baseCurrencyId"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Prepared base currency identity is missing.",
            )
        })?;
        let quote = prepared.binding["quoteCurrencyId"]
            .as_str()
            .ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Prepared quote currency identity is missing.",
                )
            })?;
        let side = prepared.input["side"].as_str().ok_or_else(|| {
            failure(
                "COINBASE_BINDING_INVALID",
                "Prepared order side is missing.",
            )
        })?;
        let mut cursor = None;
        let mut seen_cursors = BTreeSet::new();
        let mut fills = BTreeMap::<String, (String, Value)>::new();
        for page_num in 0..1000 {
            let response = match self
                .request_for_account(
                    account_name,
                    CoinbaseProduct::Advanced,
                    CoinbaseRoute::Fills {
                        query: CoinbaseQuery {
                            order_ids: vec![order_id.to_owned()],
                            cursor: cursor.clone(),
                            limit: Some(250),
                            ..Default::default()
                        },
                    },
                    CoinbaseRequest::get(),
                    None,
                )
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    return Ok(coinbase_unknown_request(
                        "venue fill-history request failed; the persisted order will not be replayed",
                        &error,
                    ));
                }
            };
            if !(200..300).contains(&response.status) {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"fill history is unavailable"}),
                    verified: None,
                });
            }
            let page: Value = serde_json::from_slice(&response.body).map_err(|_| {
                failure(
                    "COINBASE_RESPONSE_INVALID",
                    "Coinbase returned invalid fills.",
                )
            })?;
            match page.get("proof_token_required") {
                Some(Value::Bool(false)) | None => {}
                Some(_) => {
                    return Ok(Reconciliation {
                        status: Status::Unknown,
                        evidence: json!({"orderId":order_id,"reason":"fill response requires or malformedly signals an EU SCA proof token"}),
                        verified: None,
                    });
                }
            }
            let rows = page
                .get("fills")
                .and_then(Value::as_array)
                .ok_or_else(|| failure("COINBASE_RESPONSE_INVALID", "Fill page omitted fills."))?;
            if rows.is_empty() {
                break;
            }
            for fill in rows {
                if fill
                    .get("order_id")
                    .or_else(|| fill.get("orderId"))
                    .and_then(Value::as_str)
                    != Some(order_id.as_str())
                {
                    return Err(failure(
                        "COINBASE_FILL_IDENTITY_INVALID",
                        "Coinbase returned a fill for a different order.",
                    ));
                }
                let id = fill
                    .get("entry_id")
                    .or_else(|| fill.get("trade_id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        failure(
                            "COINBASE_FILL_IDENTITY_MISSING",
                            "Coinbase fill omitted its stable identity.",
                        )
                    })?
                    .to_owned();
                let digest = hex::encode(Sha256::digest(
                    gofin_domain::canonical_json(fill)?.as_bytes(),
                ));
                if let Some(prior) = fills.get(&id) {
                    if prior.0 != digest {
                        return Err(failure(
                            "COINBASE_FILL_CONFLICT",
                            "Coinbase returned conflicting immutable fill evidence.",
                        ));
                    }
                } else {
                    fills.insert(id, (digest, fill.clone()));
                }
            }
            let Some(next) = page
                .get("cursor")
                .or_else(|| page.get("next_cursor"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
            else {
                break;
            };
            if !seen_cursors.insert(next.clone()) || page_num == 999 {
                return Err(failure(
                    "COINBASE_PAGINATION_INVALID",
                    "Fill history pagination did not advance safely.",
                ));
            }
            cursor = Some(next);
        }
        let mut base_total = Quantity::zero();
        let mut quote_total = Quantity::zero();
        let mut fee_total = Quantity::zero();
        let mut quote_sized: Option<bool> = None;
        let mut event_digests = BTreeMap::new();
        let mut proof_events = Vec::new();
        for (id, (digest, fill)) in &fills {
            let returned_product = fill
                .get("product_id")
                .or_else(|| fill.get("productId"))
                .and_then(Value::as_str);
            let returned_side = fill.get("side").and_then(Value::as_str);
            if returned_product != prepared.binding["productId"].as_str()
                || returned_side != Some(side)
            {
                return Err(failure(
                    "COINBASE_FILL_IDENTITY_INVALID",
                    "Coinbase fill omitted or changed its product or side identity.",
                ));
            }
            let price_text = fill
                .get("price")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("COINBASE_FILL_INCOMPLETE", "Fill price is missing."))?;
            let _ = exact_atoms(price_text, 36)?;
            let size_text = fill
                .get("size")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("COINBASE_FILL_INCOMPLETE", "Fill size is missing."))?;
            let size = exact_atoms(size_text, 36)?;
            let commission = exact_atoms(
                fill.get("commission")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        failure("COINBASE_FILL_INCOMPLETE", "Fill commission is missing.")
                    })?,
                36,
            )?;
            let Some(is_quote_size) = fill.get("size_in_quote").and_then(Value::as_bool) else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"fillId":id,"reason":"fill omitted the native size_in_quote denomination flag"}),
                    verified: None,
                });
            };
            if quote_sized.is_some_and(|prior| prior != is_quote_size) {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"fill pages mixed base-sized and quote-sized fills"}),
                    verified: None,
                });
            }
            quote_sized = Some(is_quote_size);
            if is_quote_size {
                quote_total = quote_total.checked_add(&size)?;
            } else {
                let value = decimal_product_atoms_exact(price_text, size_text, 36)?;
                base_total = base_total.checked_add(&size)?;
                quote_total = quote_total.checked_add(&value)?;
            }
            fee_total = fee_total.checked_add(&commission)?;
            let event_id = format!("coinbase:fill:{id}");
            event_digests.insert(event_id.clone(), digest.clone());
            proof_events.push(json!({"eventId":event_id,"fill":fill}));
        }
        if !fills.is_empty() && order.get("size_in_quote").and_then(Value::as_bool) != quote_sized {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"orderId":order_id,"reason":"authoritative order size_in_quote flag conflicts with fill denomination"}),
                verified: None,
            });
        }
        if quote_sized == Some(true) {
            let Some(filled_size) = order.get("filled_size").and_then(Value::as_str) else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"quote-sized fills require authoritative order filled_size"}),
                    verified: None,
                });
            };
            let Some(filled_value) = order.get("filled_value").and_then(Value::as_str) else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"quote-sized fills require authoritative order filled_value"}),
                    verified: None,
                });
            };
            let order_base_total = exact_atoms(filled_size, 36)?;
            let order_quote_total = exact_atoms(filled_value, 36)?;
            if order_quote_total != quote_total {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"quote-sized fill sum conflicts with authoritative order filled_value"}),
                    verified: None,
                });
            }
            base_total = order_base_total;
        }
        // A fills page is not, by itself, a terminal accounting proof. Advanced
        // order detail exposes cumulative base, quote and fee totals; require
        // them to agree with the complete fill walk in either denomination.
        if !fills.is_empty() {
            let (Some(native_base), Some(native_quote), Some(native_fees)) = (
                order.get("filled_size").and_then(Value::as_str),
                order.get("filled_value").and_then(Value::as_str),
                order.get("total_fees").and_then(Value::as_str),
            ) else {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"authoritative order omitted cumulative filled_size, filled_value or total_fees"}),
                    verified: None,
                });
            };
            let native_base = exact_atoms(native_base, 36)?;
            let native_quote = exact_atoms(native_quote, 36)?;
            let native_fees = exact_atoms(native_fees, 36)?;
            if native_base != base_total || native_quote != quote_total || native_fees != fee_total
            {
                return Ok(Reconciliation {
                    status: Status::Unknown,
                    evidence: json!({"orderId":order_id,"reason":"complete fill totals disagree with authoritative order aggregates","fillsBase":base_total,"orderBase":native_base,"fillsQuote":quote_total,"orderQuote":native_quote,"fillsFees":fee_total,"orderFees":native_fees}),
                    verified: None,
                });
            }
        }
        // The order may have changed while fills were paginated. A second
        // authoritative read must match the first snapshot before it can own
        // immutable fill effects or a terminal classification.
        let confirm_response = self
            .request_for_account(
                account_name,
                CoinbaseProduct::Advanced,
                CoinbaseRoute::Order {
                    order_id: order_id.to_owned(),
                },
                CoinbaseRequest::get(),
                None,
            )
            .await?;
        if !(200..300).contains(&confirm_response.status) {
            return Ok(coinbase_unknown(
                "order changed or became unavailable during fill pagination",
            ));
        }
        let confirm_doc: Value = serde_json::from_slice(&confirm_response.body).map_err(|_| {
            failure(
                "COINBASE_RESPONSE_INVALID",
                "Coinbase returned invalid order status.",
            )
        })?;
        let confirm = confirm_doc.get("order").unwrap_or(&confirm_doc);
        let stable_fields = [
            "order_id",
            "client_order_id",
            "product_id",
            "status",
            "filled_size",
            "filled_value",
            "total_fees",
            "size_in_quote",
            "settled",
        ];
        if stable_fields
            .iter()
            .any(|key| order.get(*key) != confirm.get(*key))
        {
            return Ok(coinbase_unknown(
                "authoritative order snapshot changed during fill pagination",
            ));
        }
        let mut effects = Vec::new();
        if !fills.is_empty() {
            let (spent_asset, received_asset, spent, received) = if side == "BUY" {
                (
                    product_asset(quote),
                    product_asset(base),
                    quote_total.clone(),
                    base_total.clone(),
                )
            } else {
                (
                    product_asset(base),
                    product_asset(quote),
                    base_total.clone(),
                    quote_total.clone(),
                )
            };
            let resource = self
                .accounts
                .iter()
                .find(|a| a.name == account_name)
                .map(|a| a.resource.clone())
                .ok_or_else(|| {
                    failure(
                        "COINBASE_ACCOUNT_UNKNOWN",
                        "Persisted order account is no longer enrolled.",
                    )
                })?;
            effects.push(FinancialEffect::Conversion {
                resource: resource.clone(),
                spent: Amount {
                    asset: spent_asset,
                    atoms: spent,
                },
                received: Amount {
                    asset: received_asset,
                    atoms: received,
                },
            });
            if !fee_total.is_zero() {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash { resource },
                    amount: Amount {
                        asset: product_asset(quote),
                        atoms: fee_total.clone(),
                    },
                    category: "coinbase-trading-fee".into(),
                    treatment: if side == "SELL" && quote_total >= fee_total {
                        CostTreatment::DeductedFromCredit
                    } else {
                        CostTreatment::AdditionalDebit
                    },
                });
            }
        }
        let status_text = order
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        let gross_output_total = if side == "BUY" {
            base_total.clone()
        } else {
            quote_total.clone()
        };
        let fee_within_proceeds = side != "SELL" || gross_output_total >= fee_total;
        let output_total = if side == "SELL" && gross_output_total >= fee_total {
            gross_output_total.checked_sub(&fee_total)?
        } else {
            gross_output_total
        };
        let minimum_output = exact_atoms(
            prepared.input["minimumReceive"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Prepared minimum receive is missing.",
                )
            })?,
            36,
        )?;
        let meets_minimum = fee_within_proceeds && output_total >= minimum_output;
        let realized_spend = if side == "BUY" {
            quote_total.clone()
        } else {
            base_total.clone()
        };
        let maximum_spend = exact_atoms(
            prepared.input["maximumSpend"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Prepared maximum spend is missing.",
                )
            })?,
            36,
        )?;
        let maximum_fee = exact_atoms(
            prepared.input["maximumFee"].as_str().ok_or_else(|| {
                failure(
                    "COINBASE_BINDING_INVALID",
                    "Prepared maximum fee is missing.",
                )
            })?,
            36,
        )?;
        let within_authorized_costs = realized_spend <= maximum_spend && fee_total <= maximum_fee;
        let mut status = match status_text.as_str() {
            "FILLED" if fills.is_empty() => Status::Unknown,
            "FILLED" if meets_minimum && within_authorized_costs => Status::Completed,
            "FILLED" => Status::Failed,
            "CANCELLED" | "CANCELED" | "EXPIRED" if fills.is_empty() => Status::Failed,
            "CANCELLED" | "CANCELED" | "EXPIRED" if meets_minimum && within_authorized_costs => {
                Status::Completed
            }
            "CANCELLED" | "CANCELED" | "EXPIRED" => Status::Failed,
            "REJECTED" | "FAILED" => Status::Failed,
            "OPEN" | "PENDING" | "QUEUED" | "PARTIALLY_FILLED" => Status::Pending,
            _ => Status::Unknown,
        };
        if !fills.is_empty()
            && matches!(
                status_text.as_str(),
                "FILLED" | "CANCELLED" | "CANCELED" | "EXPIRED"
            )
        {
            status = match order.get("settled").and_then(Value::as_bool) {
                Some(true) => status,
                Some(false) => Status::Pending,
                None => Status::Unknown,
            };
        }
        if status == Status::Unknown {
            return Ok(Reconciliation {
                status,
                evidence: json!({"orderId":order_id,"order":order,"fillCount":fills.len()}),
                verified: None,
            });
        }
        let previous = record.effects.as_ref().and_then(|e| {
            SettlementProgress::from_evidence(&e.evidence)
                .ok()
                .flatten()
        });
        if let Some(old) = &previous {
            for (id, digest) in &old.event_digests {
                if event_digests.get(id).is_none_or(|new| new != digest) {
                    return Err(failure(
                        "COINBASE_FILL_CONFLICT",
                        "Previously observed fill evidence is missing or changed.",
                    ));
                }
            }
        }
        let snapshot_id = gofin_domain::binding_hash(
            &json!({"orderId":order_id,"status":status_text,"events":event_digests,"baseTotal":base_total,"quoteTotal":quote_total,"feeTotal":fee_total}),
        )?;
        let sequence = previous.as_ref().map_or(1, |p| {
            if p.snapshot_id == snapshot_id {
                p.sequence
            } else {
                p.sequence + 1
            }
        });
        let progress = SettlementProgress {
            version: 1,
            sequence,
            snapshot_id,
            event_digests,
            position_orders: vec![],
        };
        let evidence = json!({"order":order,"reconciledOrderId":order_id,"proofEvents":proof_events,"settlement_progress":progress,"fillCount":fills.len(),"withinAuthorizedCosts":within_authorized_costs,"meetsMinimum":meets_minimum,"realizedSpend":realized_spend,"realizedFee":fee_total});
        let mut verified = None;
        if !effects.is_empty() || status.terminal() {
            let mut bundle = VerifiedEffects {
                evidence_version: 1,
                submission: submission.clone(),
                effects,
                evidence: evidence.clone(),
                observed_at: gofin_domain::now(),
                proof_digest: String::new(),
            };
            bundle.proof_digest = gofin_domain::verified_effect_digest(&bundle)?;
            verified = Some(bundle);
        }
        Ok(Reconciliation {
            status,
            evidence,
            verified,
        })
    }
    fn validate_response(
        &self,
        submission: &gofin_domain::Submission,
        response: &Value,
    ) -> Result<()> {
        if submission.data["binding"]["feeMode"] == "venue_calculated"
            && (200..500).contains(&(response["status"].as_u64().unwrap_or(0) as u16))
        {
            // Capture native transaction errors so withdrawal reconciliation can
            // distinguish a definitive rejection from an ambiguous response.
            return Ok(());
        }
        // A captured Advanced create-order rejection is a valid native receipt,
        // even though it is not an accepted order. Let the reconciler classify
        // the bounded machine error and release the reservation with empty
        // verified effects. Unrecognized errors remain ambiguous.
        if submission.data["product"].as_str() == Some("coinbase")
            && !submission.data["clientOrderId"]
                .as_str()
                .unwrap_or("")
                .is_empty()
            && is_conclusive_create_order_rejection(
                response["status"].as_u64().unwrap_or(0) as u16,
                &response["body"],
            )
        {
            return Ok(());
        }
        if matches!(
            submission.data["binding"]["configurationRoute"].as_str(),
            Some("portfolios_create" | "portfolios_edit" | "portfolios_delete")
        ) {
            let body = &response["body"];
            let portfolio = body.get("portfolio").unwrap_or(&Value::Null);
            let binding = &submission.data["binding"];
            if binding["configurationRoute"] == "portfolios_delete" {
                if (200..300).contains(&response["status"].as_u64().unwrap_or(0))
                    && body.as_object().is_some_and(serde_json::Map::is_empty)
                {
                    return Ok(());
                }
                return Err(failure(
                    "COINBASE_CONFIGURATION_RECEIPT_INVALID",
                    "Coinbase did not return the expected empty portfolio deletion receipt; the attempt remains unresolved.",
                ));
            }
            let exact_name =
                portfolio.get("name").and_then(Value::as_str) == binding["newName"].as_str();
            let identity_matches = if binding["configurationRoute"] == "portfolios_create" {
                portfolio
                    .get("uuid")
                    .and_then(Value::as_str)
                    .is_some_and(|uuid| !uuid.is_empty())
            } else {
                portfolio.get("uuid").and_then(Value::as_str) == binding["portfolioId"].as_str()
            };
            let valid_receipt = portfolio.get("deleted").and_then(Value::as_bool) == Some(false)
                && portfolio
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                && exact_name
                && identity_matches;
            if (200..300).contains(&response["status"].as_u64().unwrap_or(0)) && valid_receipt {
                return Ok(());
            }
            return Err(failure(
                "COINBASE_CONFIGURATION_RECEIPT_INVALID",
                "Coinbase did not return an exact portfolio identity and resulting name; keep this configuration attempt unresolved and do not replay it.",
            ));
        }
        if response["status"]
            .as_u64()
            .is_some_and(|status| (200..300).contains(&(status as u16)))
            && response["body"]["success"].as_bool() != Some(false)
        {
            Ok(())
        } else {
            Err(failure(
                if submission.data["binding"]["feeMode"] == "venue_calculated" {
                    "COINBASE_WITHDRAWAL_SUBMISSION_UNCONFIRMED"
                } else {
                    "COINBASE_ORDER_SUBMISSION_UNCONFIRMED"
                },
                if submission.data["binding"]["feeMode"] == "venue_calculated" {
                    "Coinbase did not confirm withdrawal acceptance; reconcile the durable idempotency key before any retry."
                } else {
                    "Coinbase did not confirm order acceptance; reconcile the durable client order ID before any retry."
                },
            ))
        }
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        self.prepare(operation, input).await
    }
    async fn prepare_with_linked_context(
        &self,
        operation: &str,
        input: &Value,
        _context: &PreparationContext,
        linked: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<PreparedOperation> {
        if let Some(prepared) =
            crate::institutional::prepare_with_linked_context(self, operation, input, linked)
                .await?
        {
            return Ok(prepared);
        }
        if operation == "coinbase.edit-order" {
            return self.prepare_edit_order(input, linked).await;
        }
        self.prepare(operation, input).await
    }
}

#[cfg(test)]
mod no_dispatch_plan_metadata_tests {
    use super::*;

    fn terminal_no_dispatch_record() -> OperationRecord {
        let mut binding = json!({
            "clientOrderId":"client-attempt-1",
            "maximumSpend":"100",
            "maximumFees":{"USD":"1"},
            "requestBodyExact":"{\"client_oid\":\"client-attempt-1\"}"
        });
        binding[gofin_domain::financial_plans::PLAN_METADATA_KEY] = json!({
            "version":1,
            "plan_id":"plan-1",
            "step_id":"step-1"
        });
        let prepared = PreparedOperation {
            operation: "coinbase.create-order".into(),
            input: json!({"requestId":"request-1"}),
            binding,
            summary: "synthetic order".into(),
            checks: Vec::new(),
            warnings: Vec::new(),
            observed_at: gofin_domain::now(),
            expires_at: gofin_domain::now(),
            valuation: None,
            intent: FinancialIntent {
                debits: Vec::new(),
                maximum_costs: Vec::new(),
                expected: Vec::new(),
                locks: Vec::new(),
            },
            operation_version: 1,
            action_deadline: None,
        };
        let candidate = prepared.clone();
        let mut candidate = candidate;
        candidate
            .binding
            .as_object_mut()
            .unwrap()
            .remove(gofin_domain::financial_plans::PLAN_METADATA_KEY);
        let binding_hash = no_dispatch_binding_digest(&candidate).unwrap();
        let submission = gofin_domain::Submission {
            kind: "coinbase".into(),
            reference: "client-attempt-1".into(),
            data: json!({"clientOrderId":"client-attempt-1"}),
        };
        let submission_json =
            gofin_domain::canonical_json(&serde_json::to_value(&submission).unwrap()).unwrap();
        let body = prepared.binding["requestBodyExact"].as_str().unwrap();
        let certificate = json!({
            "version":1,
            "reason":"transport_guard_rejected",
            "operation":"coinbase.create-order",
            "bindingSha256":binding_hash,
            "submissionSha256":hex::encode(Sha256::digest(submission_json.as_bytes())),
            "requestBodySha256":hex::encode(Sha256::digest(body.as_bytes()))
        });
        let evidence = json!({"noDispatch":certificate});
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: Vec::new(),
            evidence,
            observed_at: gofin_domain::now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
        OperationRecord {
            id: "operation-1".into(),
            request_id: "request-1".into(),
            fingerprint: "fingerprint".into(),
            fingerprint_version: 1,
            operation: prepared.operation.clone(),
            input: prepared.input.clone(),
            status: Status::Failed,
            created_at: gofin_domain::now(),
            updated_at: gofin_domain::now(),
            preview: Some(prepared),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: Some(json!({"finNoDispatch":certificate})),
            position_baseline: None,
            effects: Some(verified),
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        }
    }

    #[test]
    fn stripped_plan_metadata_allows_retry_but_substantive_binding_tamper_blocks_it() {
        let record = terminal_no_dispatch_record();
        assert!(is_terminal_verified_no_dispatch(
            &record,
            CoinbaseProduct::Advanced,
            "client-attempt-1"
        ));

        let mut tampered = record.clone();
        tampered.preview.as_mut().unwrap().binding["maximumFees"]["USD"] = json!("1000");
        assert!(!is_terminal_verified_no_dispatch(
            &tampered,
            CoinbaseProduct::Advanced,
            "client-attempt-1"
        ));
    }
}
