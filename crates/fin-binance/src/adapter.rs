use super::*;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use fin_domain::{
    AccountingLocation, Adapter, Amount, AssetId, AssetPrice, AuthorityState, Check, CheckStatus,
    ConnectionMetadata, CostBound, CostTreatment, Definition, EffectConstraint, ExecutionActor,
    FinError, FinancialEffect, FinancialIntent, FinancialState, OperationKind, OperationRecord,
    PreparedOperation, ProjectedEffect, ProjectedRequirement, Quantity, Reconciliation,
    ResourceAmount, ResourceId, ReturnRouteContext, ReturnRouteProof, SignedQuantity,
    StateSnapshot, Status, Submission, SubmissionJournal, Valuation, VerifiedEffects, binding_hash,
    now,
};
use num_bigint::BigUint;
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountRole {
    Main,
    Sub,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BinanceAccount {
    pub name: String,
    pub resource: ResourceId,
    pub credential: CredentialHandle,
    pub role: AccountRole,
    pub parent: Option<String>,
    pub email: Option<String>,
    pub wallet_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EthereumDestination {
    pub name: String,
    pub resource: ResourceId,
    pub address: String,
    pub chain_id: u64,
    pub assets: BTreeMap<String, AssetId>,
    pub confirmations: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BinanceConfig {
    pub accounts: Vec<BinanceAccount>,
    pub destinations: Vec<EthereumDestination>,
}

#[derive(Clone, Debug)]
pub struct PriceQuote {
    pub usd_per_unit: String,
    pub observed_at: String,
    pub source: String,
}

#[async_trait]
pub trait BinancePriceProvider: Send + Sync {
    async fn quote_usd(&self, coin: &str) -> Result<PriceQuote, FinError>;
}

#[derive(Clone, Debug)]
pub struct PayoutRequest {
    pub tx_hash: String,
    pub chain_id: u64,
    pub recipient: String,
    pub asset: AssetId,
    pub minimum_amount: String,
    pub maximum_amount: String,
    pub confirmations: u32,
}

#[derive(Clone, Debug)]
pub struct PayoutResult {
    pub status: Status,
    pub amount_atoms: Option<Quantity>,
    pub evidence: Value,
}

#[async_trait]
pub trait BinanceRecipientVerifier: Send + Sync {
    async fn verify(&self, request: PayoutRequest) -> PayoutResult;
}

#[derive(Clone)]
pub struct BinanceAdapter {
    config: Arc<BinanceConfig>,
    transport: Arc<dyn BinanceTransport>,
    price_provider: Option<Arc<dyn BinancePriceProvider>>,
    recipient_verifier: Option<Arc<dyn BinanceRecipientVerifier>>,
}

impl BinanceAdapter {
    pub fn new(
        config: BinanceConfig,
        transport: Arc<dyn BinanceTransport>,
        price_provider: Option<Arc<dyn BinancePriceProvider>>,
        recipient_verifier: Option<Arc<dyn BinanceRecipientVerifier>>,
    ) -> Result<Self, FinError> {
        let mut names = BTreeSet::new();
        for account in &config.accounts {
            if account.name.is_empty() || !names.insert(account.name.clone()) {
                return Err(error(
                    "INVALID_CONNECTIONS",
                    "setup",
                    "Binance account names must be unique.",
                ));
            }
            if account.wallet_type != "SPOT" {
                return Err(error(
                    "UNSUPPORTED_WALLET",
                    "setup",
                    "Only Binance SPOT accounts are implemented.",
                ));
            }
            match account.role {
                AccountRole::Main if account.parent.is_some() || account.email.is_some() => {
                    return Err(error(
                        "INVALID_HIERARCHY",
                        "setup",
                        "Main Binance connections cannot have a parent or subaccount email.",
                    ));
                }
                AccountRole::Sub
                    if account.parent.is_none()
                        || account.email.as_deref().unwrap_or("").is_empty() =>
                {
                    return Err(error(
                        "INVALID_HIERARCHY",
                        "setup",
                        "Subaccount connections require a parent and Binance account email.",
                    ));
                }
                _ => {}
            }
        }
        for account in &config.accounts {
            if let AccountRole::Sub = account.role {
                let parent = account
                    .parent
                    .as_deref()
                    .and_then(|n| config.accounts.iter().find(|candidate| candidate.name == n));
                if !matches!(parent.map(|p| &p.role), Some(AccountRole::Main)) {
                    return Err(error(
                        "INVALID_HIERARCHY",
                        "setup",
                        "Configured subaccount parent must be a configured main account.",
                    ));
                }
            }
        }
        let mut destination_names = BTreeSet::new();
        for dest in &config.destinations {
            if dest.chain_id != 1
                || !valid_eth_address(&dest.address)
                || dest.confirmations == 0
                || !destination_names.insert(dest.name.clone())
            {
                return Err(error(
                    "INVALID_DESTINATION",
                    "setup",
                    "Ethereum destinations require a unique name, valid address, mainnet chain, and positive confirmation count.",
                ));
            }
        }
        Ok(Self {
            config: Arc::new(config),
            transport,
            price_provider,
            recipient_verifier,
        })
    }

    /// Build a non-executable conditional consolidation preview. Every selected
    /// child transfer is read-validated against the main authority first; this
    /// method never submits transfers and the returned preparation is blocked
    /// from revalidation/submission until a fresh ordinary withdrawal preview
    /// is created after transfers settle.
    pub async fn preview_withdrawal_after_funding(
        &self,
        withdrawal: &Value,
        sources: Vec<ConditionalFundingSource>,
    ) -> Result<PreparedOperation, FinError> {
        let input: WithdrawInput = self.parse_input(withdrawal).await?;
        if sources.is_empty() {
            return Err(FinError::validation(
                "Conditional funding preview requires at least one selected child source.",
            ));
        }
        let mut children = BTreeSet::new();
        let mut private_sources = Vec::with_capacity(sources.len());
        let mut total = ExactDecimal::zero();
        for (index, source) in sources.into_iter().enumerate() {
            if source.authority != input.account
                || source.coin != input.coin
                || !children.insert(source.from.clone())
            {
                return Err(FinError::validation(
                    "Conditional sources must use the withdrawal's main account and asset, with each child selected once.",
                ));
            }
            let value = amount(&source.amount)?;
            let main = self.require_main(&source.authority)?;
            if source.from == main.name {
                return Err(FinError::validation(
                    "Conditional consolidation sources must be configured subaccounts.",
                ));
            }
            let transfer = TransferInput {
                request_id: format!(
                    "conditional-{}-{index}",
                    input.request_id.chars().take(72).collect::<String>()
                ),
                authority: source.authority.clone(),
                from: source.from.clone(),
                to: main.name.clone(),
                coin: source.coin.clone(),
                amount: value.canonical(),
                from_wallet: "SPOT".into(),
                to_wallet: "SPOT".into(),
            };
            self.transfer_route(&transfer).await?;
            total = add_decimal(&total, &value)?;
            private_sources.push(FundingSourceInput {
                authority: source.authority,
                from: source.from,
                coin: source.coin,
                amount: value.canonical(),
            });
        }
        self.prepare_withdrawal(
            input,
            Some(ConditionalFunding {
                sources: private_sources,
                total_amount: total,
            }),
        )
        .await
    }

    fn account(&self, name: &str) -> Result<&BinanceAccount, FinError> {
        self.config
            .accounts
            .iter()
            .find(|connection| connection.name == name)
            .ok_or_else(|| {
                error(
                    "UNKNOWN_CONNECTION",
                    "validation",
                    "The selected Binance connection is not configured.",
                )
            })
    }

    fn destination(&self, name: &str) -> Result<&EthereumDestination, FinError> {
        self.config
            .destinations
            .iter()
            .find(|connection| connection.name == name)
            .ok_or_else(|| {
                error(
                    "UNKNOWN_CONNECTION",
                    "validation",
                    "The selected Ethereum destination is not configured.",
                )
            })
    }

    fn require_main<'a>(&'a self, name: &str) -> Result<&'a BinanceAccount, FinError> {
        let account = self.account(name)?;
        if !matches!(account.role, AccountRole::Main) {
            return Err(error(
                "WRONG_AUTHORITY",
                "validation",
                "The selected authority must be an explicitly configured main Binance account.",
            ));
        }
        Ok(account)
    }

    async fn call(
        &self,
        account: &BinanceAccount,
        request: BinanceRequest,
        stage: &str,
    ) -> Result<Value, FinError> {
        request
            .validate()
            .map_err(|e| error("INVALID_ENDPOINT_REQUEST", stage, e.to_string()))?;
        self.transport
            .request(&account.credential, request)
            .await
            .map_err(|e| match e {
                TransportError::Unavailable => {
                    error("VENUE_UNAVAILABLE", stage, "Binance could not be reached.")
                }
                TransportError::RateLimited => {
                    error("VENUE_RATE_LIMIT", stage, "Binance rate limited the request.")
                }
                TransportError::PermissionDenied => error(
                    "VENUE_PERMISSION",
                    stage,
                    "The selected Binance credential was denied.",
                ),
                TransportError::InvalidResponse => error("VENUE_RESPONSE_INVALID", stage, "Binance returned an invalid response; reconcile any persisted submission before retrying."),
                TransportError::Timeout => error("VENUE_TIMEOUT", stage, "Binance timed out; reconcile any persisted submission before retrying."),
                TransportError::Rejected => error("VENUE_REQUEST_FAILED", stage, "Binance rejected the request; reconcile any persisted submission before retrying."),
                TransportError::HttpFailure {
                    status,
                    binance_code,
                    kind,
                } => http_failure_error(stage, status, binance_code, kind),
            })
    }

    async fn permissions(&self, account: &BinanceAccount) -> Result<ApiRestrictions, FinError> {
        let response = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::ApiRestrictions),
                "read",
            )
            .await?;
        parse_response(response).map_err(|_| {
            error(
                "VENUE_RESPONSE_INVALID",
                "read",
                "Binance permissions response was invalid.",
            )
        })
    }

    async fn account_identity(
        &self,
        account: &BinanceAccount,
    ) -> Result<(String, String), FinError> {
        let response = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::Account),
                "read",
            )
            .await?;
        let account_type = response
            .get("accountType")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "read",
                    "Binance Account response omitted its account type.",
                )
            })?;
        if account_type != "SPOT" {
            return Err(error(
                "ACCOUNT_TYPE_MISMATCH",
                "read",
                "The authenticated Binance key is not a SPOT account.",
            ));
        }
        let uid_value = response.get("uid").ok_or_else(|| {
            error(
                "VENUE_RESPONSE_INVALID",
                "read",
                "Binance Account response omitted its physical account UID.",
            )
        })?;
        let uid = uid_value
            .as_u64()
            .map(|value| value.to_string())
            .or_else(|| {
                uid_value
                    .as_str()
                    .filter(|value| {
                        !value.is_empty()
                            && value.bytes().all(|byte| byte.is_ascii_digit())
                            && value.as_bytes()[0] != b'0'
                    })
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "read",
                    "Binance Account response contained a noncanonical physical UID.",
                )
            })?;
        if uid == "0" {
            return Err(error(
                "VENUE_RESPONSE_INVALID",
                "read",
                "Binance Account UID must be positive.",
            ));
        }
        let resource = format!("binance:uid:{uid}:SPOT");
        if account.resource.0 != resource {
            return Err(error(
                "ACCOUNT_IDENTITY_MISMATCH",
                "read",
                "Authenticated Binance UID does not match the enrolled physical resource.",
            ));
        }
        Ok((uid, account_type.to_owned()))
    }

    async fn spot_balances(
        &self,
        account: &BinanceAccount,
        authority: Option<&BinanceAccount>,
    ) -> Result<Vec<SpotBalanceRow>, FinError> {
        let response = match authority {
            Some(main) if main.name != account.name => {
                if !matches!(main.role, AccountRole::Main)
                    || account.parent.as_deref() != Some(main.name.as_str())
                {
                    return Err(error(
                        "WRONG_HIERARCHY",
                        "read",
                        "Main authority and selected Binance child do not match.",
                    ));
                }
                let email = account.email.as_deref().ok_or_else(|| {
                    error(
                        "INVALID_HIERARCHY",
                        "read",
                        "Configured child email is unavailable.",
                    )
                })?;
                self.call(
                    main,
                    BinanceRequest::new(BinanceEndpoint::SubAccountSpotAssets).push("email", email),
                    "read",
                )
                .await?
            }
            _ => {
                self.call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::Account),
                    "read",
                )
                .await?
            }
        };
        let parsed: SpotBalancesResponse = parse_response(response).map_err(|_| {
            error(
                "VENUE_RESPONSE_INVALID",
                "read",
                "Binance SPOT balance response contained a missing or non-string quantity.",
            )
        })?;
        Ok(parsed.balances)
    }

    fn free<'a>(balances: &'a [SpotBalanceRow], asset: &str) -> Option<&'a ExactDecimal> {
        balances
            .iter()
            .find(|balance| balance.asset == asset)
            .map(|balance| &balance.free)
    }

    async fn verify_subaccount(
        &self,
        child: &BinanceAccount,
        authority: &BinanceAccount,
    ) -> Result<(), FinError> {
        if !matches!(child.role, AccountRole::Sub)
            || child.parent.as_deref() != Some(authority.name.as_str())
        {
            return Err(error(
                "WRONG_HIERARCHY",
                "preview",
                "Selected Binance child is not configured under the selected main account.",
            ));
        }
        let email = child.email.as_deref().ok_or_else(|| {
            error(
                "INVALID_HIERARCHY",
                "preview",
                "Configured child email is unavailable.",
            )
        })?;
        let response = self
            .call(
                authority,
                BinanceRequest::new(BinanceEndpoint::SubAccountList)
                    .push("email", email)
                    .push("page", "1")
                    .push("limit", "200"),
                "preview",
            )
            .await?;
        let parsed: SubAccountListResponse = parse_response(response).map_err(|_| {
            error(
                "VENUE_RESPONSE_INVALID",
                "preview",
                "Binance subaccount list response was invalid.",
            )
        })?;
        match parsed.sub_accounts.iter().find(|row| row.email == email) {
            None => Err(error(
                "HIERARCHY_UNVERIFIED",
                "preview",
                "Configured child was not found under the selected main account.",
            )),
            Some(row) if row.is_freeze != Some(false) => Err(error(
                "ACCOUNT_FROZEN_OR_UNKNOWN",
                "preview",
                "Subaccount is frozen or its freeze status is unknown.",
            )),
            Some(_) => Ok(()),
        }
    }

    async fn capital_config(
        &self,
        account: &BinanceAccount,
    ) -> Result<Vec<CapitalConfigCurrency>, FinError> {
        let response = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::CapitalConfig),
                "read",
            )
            .await?;
        parse_response(response).map_err(|_| {
            error(
                "VENUE_RESPONSE_INVALID",
                "read",
                "Binance funding network response was invalid.",
            )
        })
    }

    async fn currency(
        &self,
        account: &BinanceAccount,
        coin: &str,
    ) -> Result<CapitalConfigCurrency, FinError> {
        self.capital_config(account)
            .await?
            .into_iter()
            .find(|currency| currency.coin == coin)
            .ok_or_else(|| {
                error(
                    "UNKNOWN_ASSET",
                    "preview",
                    "Selected Binance crypto asset is unavailable.",
                )
            })
    }

    fn network<'a>(
        &self,
        currency: &'a CapitalConfigCurrency,
        network: &str,
    ) -> Result<&'a CapitalNetwork, FinError> {
        currency
            .network_list
            .iter()
            .find(|route| route.coin == currency.coin && route.network == network)
            .ok_or_else(|| {
                error(
                    "UNKNOWN_NETWORK",
                    "preview",
                    "Selected Binance asset and network pair is not advertised.",
                )
            })
    }

    async fn valuation(
        &self,
        coin: &str,
        movement: &str,
        max_fee: &str,
    ) -> Result<Option<Valuation>, FinError> {
        let Some(provider) = &self.price_provider else {
            return Ok(None);
        };
        let quote = provider.quote_usd(coin).await?;
        let price = ExactDecimal::parse(&quote.usd_per_unit).map_err(|_| {
            error(
                "VALUATION_INVALID",
                "preview",
                "USD price must be an exact positive decimal.",
            )
        })?;
        if price.is_zero() || quote.source.is_empty() {
            return Err(error(
                "VALUATION_INVALID",
                "preview",
                "USD price source and positive price are required.",
            ));
        }
        let observed = DateTime::parse_from_rfc3339(&quote.observed_at).map_err(|_| {
            error(
                "STALE_VALUATION",
                "preview",
                "Price observation time is invalid.",
            )
        })?;
        let age = Utc::now().signed_duration_since(observed.with_timezone(&Utc));
        if age < Duration::seconds(-5) || age > Duration::seconds(60) {
            return Err(error(
                "STALE_VALUATION",
                "preview",
                "USD price must be observed within the last 60 seconds.",
            ));
        }
        let movement_usd = decimal_product_micros(movement, &quote.usd_per_unit)?;
        let fee_usd = decimal_product_micros(max_fee, &quote.usd_per_unit)?;
        let unit_price_micros = decimal_product_micros("1", &quote.usd_per_unit)?;
        let price_asset = AssetId {
            network: "binance".into(),
            reference: coin.into(),
            decimals: 36,
        };
        Ok(Some(Valuation {
            movement_usd_micros: Quantity::new(movement_usd).map_err(domain_error)?,
            maximum_fee_usd_micros: Quantity::new(fee_usd).map_err(domain_error)?,
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: quote.observed_at.clone(),
            source: quote.source.clone(),
            prices: vec![AssetPrice {
                asset: price_asset,
                usd_micros_per_unit: Quantity::new(unit_price_micros).map_err(domain_error)?,
                unit_atoms: Quantity::new(format!("1{}", "0".repeat(36))).map_err(domain_error)?,
                observed_at: quote.observed_at,
                source: quote.source,
            }],
        }))
    }

    fn check(name: &str, status: CheckStatus, detail: &str) -> Check {
        Check {
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }

    async fn parse_input<T: for<'de> Deserialize<'de>>(
        &self,
        input: &Value,
    ) -> Result<T, FinError> {
        serde_json::from_value(input.clone()).map_err(|_| {
            FinError::validation("Input does not match the strict Binance operation schema.")
        })
    }

    fn parse_window(
        &self,
        start: Option<u64>,
        end: Option<u64>,
        days: u64,
    ) -> Result<HistoryWindow, FinError> {
        let now = Utc::now().timestamp_millis().max(0) as u64;
        let end = end.unwrap_or(now);
        let start = start.unwrap_or(end.saturating_sub(days * 86_400_000).saturating_add(1));
        if end < start
            || end.saturating_sub(start) >= days * 86_400_000
            || end > now.saturating_add(5000)
        {
            return Err(error(
                "INVALID_HISTORY_WINDOW",
                "validation",
                "History window must be positive, bounded, and end no later than current time.",
            ));
        }
        Ok(HistoryWindow {
            start_time_ms: start,
            end_time_ms: end,
        })
    }

    async fn read_funding_history(
        &self,
        account: &BinanceAccount,
        deposit: bool,
        input: &HistoryInput,
    ) -> Result<Value, FinError> {
        let days = if !deposit && input.withdraw_order_id.is_some() {
            7
        } else {
            90
        };
        let window = self.parse_window(input.start_time, input.end_time, days)?;
        let endpoint = if deposit {
            BinanceEndpoint::DepositHistory
        } else {
            BinanceEndpoint::WithdrawalHistory
        };
        let mut records = Vec::<FundingHistoryRow>::new();
        let mut complete = false;
        let page_size = 1000u64;
        for page in 0..20u64 {
            let mut request = BinanceRequest::new(endpoint)
                .push("startTime", window.start_time_ms.to_string())
                .push("endTime", window.end_time_ms.to_string())
                .push("offset", (page * page_size).to_string())
                .push("limit", page_size.to_string());
            if let Some(coin) = &input.coin {
                request = request.push("coin", coin);
            }
            if !deposit {
                if let Some(reference) = &input.withdraw_order_id {
                    request = request.push("withdrawOrderId", reference);
                }
            } else if let Some(tx) = &input.tx_hash {
                request = request.push("txId", tx);
            }
            let response = self.call(account, request, "read").await?;
            let batch: Vec<FundingHistoryRow> = parse_response(response).map_err(|_| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "read",
                    "Binance funding history did not match the documented exact response types.",
                )
            })?;
            if batch.iter().any(|row| {
                if deposit {
                    !row.has_valid_deposit_complete_time()
                } else {
                    !row.has_valid_withdrawal_complete_time()
                }
            }) {
                return Err(error(
                    "VENUE_RESPONSE_INVALID",
                    "read",
                    "Binance funding history contains a completeTime value with the wrong endpoint type or format.",
                ));
            }
            let length = batch.len();
            records.extend(batch);
            if length < page_size as usize {
                complete = true;
                break;
            }
        }
        if let Some(coin) = &input.coin {
            records.retain(|record| &record.coin == coin);
        }
        if let Some(tx) = &input.tx_hash {
            records.retain(|record| {
                record
                    .tx_id
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(tx))
            });
        }
        if let Some(order) = &input.withdraw_order_id {
            records.retain(|record| record.withdraw_order_id.as_deref() == Some(order.as_str()));
        }
        let safe: Vec<Value> = records
            .iter()
            .map(|record| {
                let mut output = redact_funding_row(record);
                let status = &record.status;
                let credit_state = if record.usable_deposit_credit() {
                    "credited"
                } else if record.locked_deposit_credit() {
                    "credited_locked"
                } else {
                    "pending"
                };
                let venue_state = if record.completed_external_withdrawal() {
                    "completed"
                } else if [1u64, 3, 5].contains(status) {
                    "failed"
                } else {
                    "pending"
                };
                if let Some(object) = output.as_object_mut() {
                    if deposit {
                        object.insert("creditState".into(), json!(credit_state));
                    } else {
                        object.insert("venueState".into(), json!(venue_state));
                    }
                }
                output
            })
            .collect();
        Ok(
            json!({"operation": if deposit {"binance.deposit-history"} else {"binance.withdrawal-history"}, "account": account.name, "observedAt": now(), "complete": complete, "records": safe}),
        )
    }

    fn validate_transfer_route(
        &self,
        input: &TransferInput,
    ) -> Result<(&BinanceAccount, &BinanceAccount, &BinanceAccount), FinError> {
        let authority = self.require_main(&input.authority)?;
        let from = self.account(&input.from)?;
        let to = self.account(&input.to)?;
        if from.name == to.name {
            return Err(error(
                "SAME_ACCOUNT",
                "validation",
                "Transfer source and destination must differ.",
            ));
        }
        if from.name != authority.name && from.parent.as_deref() != Some(authority.name.as_str()) {
            return Err(error(
                "WRONG_HIERARCHY",
                "validation",
                "Transfer source does not belong to the selected main authority.",
            ));
        }
        if to.name != authority.name && to.parent.as_deref() != Some(authority.name.as_str()) {
            return Err(error(
                "WRONG_HIERARCHY",
                "validation",
                "Transfer destination does not belong to the selected main authority.",
            ));
        }
        if from.name != authority.name && to.name != authority.name {
            return Err(error(
                "UNSUPPORTED_ROUTE",
                "validation",
                "Only main-to-subaccount and subaccount-to-main SPOT transfers are implemented.",
            ));
        }
        Ok((authority, from, to))
    }

    async fn transfer_route(
        &self,
        input: &TransferInput,
    ) -> Result<(BinanceRequest, String, String), FinError> {
        let (authority, from, to) = self.validate_transfer_route(input)?;
        if input.from_wallet != "SPOT" || input.to_wallet != "SPOT" {
            return Err(error(
                "UNSUPPORTED_WALLET",
                "validation",
                "Only Binance SPOT wallet transfers are implemented.",
            ));
        }
        if from.name != authority.name {
            self.verify_subaccount(from, authority).await?;
        }
        if to.name != authority.name {
            self.verify_subaccount(to, authority).await?;
        }
        let permissions = self.permissions(authority).await?;
        if permissions.enable_internal_transfer != Some(true)
            || permissions.permits_universal_transfer != Some(true)
        {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "Main credential must report internal-transfer and universal-transfer permissions.",
            ));
        }
        let currency = self.currency(authority, &input.coin).await?;
        let route = currency
            .network_list
            .iter()
            .find(|n| n.network == "ETH")
            .or_else(|| currency.network_list.first())
            .ok_or_else(|| {
                error(
                    "PRECISION_UNKNOWN",
                    "preview",
                    "No Binance funding precision is available for this asset.",
                )
            })?;
        let increment = route.withdraw_integer_multiple.as_ref().ok_or_else(|| {
            error(
                "PRECISION_UNKNOWN",
                "preview",
                "Binance funding increment is unavailable.",
            )
        })?;
        if !amount(&input.amount)?.is_multiple_of(increment) {
            return Err(error(
                "INVALID_PRECISION",
                "preview",
                "Amount does not match the exact Binance funding increment.",
            ));
        }
        let amount = amount(&input.amount)?;
        let source_balances = self.spot_balances(from, Some(authority)).await?;
        if Self::free(&source_balances, &input.coin)
            .is_none_or(|free| free.checked_cmp(&amount).is_lt())
        {
            return Err(error(
                "INSUFFICIENT_BALANCE",
                "preview",
                "SPOT free balance does not cover the amount; locked balances are unavailable.",
            ));
        }
        let intent = UniversalTransferIntent {
            asset: input.coin.clone(),
            amount,
            from_email: from.email.clone().filter(|_| from.name != authority.name),
            to_email: to.email.clone().filter(|_| to.name != authority.name),
            client_id: ClientIdentity::transfer(&input.request_id).map_err(domain_error)?,
        };
        let request = intent.request().map_err(domain_error)?;
        let source_free = Self::free(&source_balances, &input.coin)
            .expect("checked balance")
            .canonical();
        let destination_balances = self.spot_balances(to, Some(authority)).await?;
        let destination_free = Self::free(&destination_balances, &input.coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero)
            .canonical();
        Ok((request, source_free, destination_free))
    }

    async fn prepare_transfer(&self, input: TransferInput) -> Result<PreparedOperation, FinError> {
        let (authority, from, to) = self.validate_transfer_route(&input)?;
        let (request, _, _) = self.transfer_route(&input).await?;
        let value = amount(&input.amount)?;
        let currency = self.currency(authority, &input.coin).await?;
        let _precision = currency
            .network_list
            .iter()
            .find(|n| n.network == "ETH")
            .or_else(|| currency.network_list.first())
            .and_then(|n| n.withdraw_integer_multiple.as_ref())
            .ok_or_else(|| {
                error(
                    "PRECISION_UNKNOWN",
                    "preview",
                    "Binance funding precision is unavailable.",
                )
            })?;
        // Binance's REST balances and transfer quantities are decimal strings
        // without a canonical native-unit scale. Use a stable 36-place
        // adapter representation and enforce the live increment separately.
        let venue_decimals = 36;
        let value_atoms = decimal_to_atoms(&value, venue_decimals)?;
        let valuation = self.valuation(&input.coin, &value.canonical(), "0").await?;
        let binding = json!({
            "authority": authority.name, "from": from.name, "to": to.name,
            "coin": input.coin, "amount": value.canonical(), "fromWallet": "SPOT", "toWallet": "SPOT",
            "clientTranId": request.get("clientTranId").unwrap_or_default(),
            "asset": AssetId { network: "binance".into(), reference: input.coin.clone(), decimals: venue_decimals },
            "fromEmail": request.get("fromEmail"), "toEmail": request.get("toEmail"),
            "params": request,
        });
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: from.resource.clone(),
                amount: Amount {
                    asset: AssetId {
                        network: "binance".into(),
                        reference: input.coin.clone(),
                        decimals: venue_decimals,
                    },
                    atoms: value_atoms.clone(),
                },
            }],
            maximum_costs: vec![],
            expected: vec![EffectConstraint::Transfer {
                from: from.resource.clone(),
                to: to.resource.clone(),
                debit_asset: AssetId {
                    network: "binance".into(),
                    reference: input.coin.clone(),
                    decimals: venue_decimals,
                },
                credit_asset: AssetId {
                    network: "binance".into(),
                    reference: input.coin.clone(),
                    decimals: venue_decimals,
                },
                maximum_debit: value_atoms.clone(),
                minimum_credit: value_atoms,
            }],
            locks: vec![from.resource.clone(), to.resource.clone()],
        };
        let summary = format!(
            "Transfer {} {} from {} SPOT to {} SPOT using main authority {}; internal transfer fee 0.",
            value.canonical(),
            input.coin,
            from.name,
            to.name,
            authority.name
        );
        Ok(prepared(
            "binance.transfer",
            serde_json::to_value(&input).unwrap_or(Value::Null),
            binding,
            summary,
            vec![
                Self::check(
                    "hierarchy",
                    CheckStatus::Passed,
                    "Configured child identities and unfrozen status were observed under the selected main authority.",
                ),
                Self::check(
                    "permission",
                    CheckStatus::Passed,
                    "Internal-transfer and universal-transfer permission were observed on the selected authority.",
                ),
                Self::check(
                    "free_balance",
                    CheckStatus::Passed,
                    "SPOT free balance covers the exact requested quantity.",
                ),
                Self::check(
                    "precision",
                    CheckStatus::Passed,
                    "Exact funding increment was checked without rounding.",
                ),
            ],
            vec![],
            valuation,
            intent,
        ))
    }

    async fn prepare_withdrawal(
        &self,
        input: WithdrawInput,
        funding: Option<ConditionalFunding>,
    ) -> Result<PreparedOperation, FinError> {
        let account = self.require_main(&input.account)?;
        let destination = self.destination(&input.destination)?;
        if destination.chain_id != 1 {
            return Err(error(
                "WRONG_NETWORK",
                "preview",
                "Binance withdrawal target must be Ethereum mainnet.",
            ));
        }
        if input.network != "ETH" || input.wallet_type != "SPOT" {
            return Err(error(
                "WRONG_NETWORK",
                "preview",
                "Only Binance ETH network withdrawals from SPOT are implemented.",
            ));
        }
        let permissions = self.permissions(account).await?;
        if permissions.enable_withdrawals != Some(true) {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "Selected main credential must report withdrawal permission.",
            ));
        }
        let travel: TravelRuleRequirements = parse_response(self.call(account, BinanceRequest::new(BinanceEndpoint::TravelRuleRequirements), "preview").await?).map_err(|_| error("TRAVEL_RULE_UNKNOWN", "preview", "Travel Rule requirements could not be established; standard withdrawal is blocked."))?;
        if travel.questionnaire_country_code != "NIL" {
            return Err(error(
                "TRAVEL_RULE_REQUIRED",
                "preview",
                "This account requires a Travel Rule withdrawal flow that is not implemented; standard withdrawal is blocked.",
            ));
        }
        let currency = self.currency(account, &input.coin).await?;
        let network = self.network(&currency, "ETH")?;
        if currency.withdraw_all_enable != Some(true)
            || network.withdraw_enable != Some(true)
            || network.busy == Some(true)
        {
            return Err(error(
                "WITHDRAWAL_DISABLED",
                "preview",
                "Selected Ethereum withdrawal route is disabled or busy.",
            ));
        }
        if network.withdraw_tag == Some(true) {
            return Err(error(
                "MEMO_REQUIRED",
                "preview",
                "Selected Ethereum route requires a memo/tag and is not supported.",
            ));
        }
        let increment = network.withdraw_integer_multiple.as_ref().ok_or_else(|| {
            error(
                "PRECISION_UNKNOWN",
                "preview",
                "Binance withdrawal increment is unavailable.",
            )
        })?;
        let quantity = amount(&input.amount)?;
        let fee = network.withdraw_fee.as_ref().ok_or_else(|| {
            error(
                "FEE_UNKNOWN",
                "preview",
                "Binance withdrawal fee is unavailable.",
            )
        })?;
        let max_fee = amount(&input.max_fee)?;
        let min_receive = amount(&input.min_receive_amount)?;
        if !quantity.is_multiple_of(increment) {
            return Err(error(
                "INVALID_PRECISION",
                "preview",
                "Withdrawal amount does not match the advertised network increment.",
            ));
        }
        if fee.checked_cmp(&max_fee).is_gt() {
            return Err(error(
                "FEE_BOUND_EXCEEDED",
                "preview",
                "Current published fee exceeds the user-approved maximum.",
            ));
        }
        let min = network.withdraw_min.as_ref().ok_or_else(|| {
            error(
                "WITHDRAWAL_LIMIT_UNKNOWN",
                "preview",
                "Binance withdrawal minimum is unavailable.",
            )
        })?;
        let max = network.withdraw_max.as_ref().ok_or_else(|| {
            error(
                "WITHDRAWAL_LIMIT_UNKNOWN",
                "preview",
                "Binance withdrawal maximum is unavailable.",
            )
        })?;
        if quantity.checked_cmp(min).is_lt() || quantity.checked_cmp(max).is_gt() {
            return Err(error(
                "WITHDRAWAL_LIMIT",
                "preview",
                "API quantity is outside the advertised network minimum/maximum.",
            ));
        }
        let asset = destination.assets.get(&input.coin).ok_or_else(|| {
            error(
                "UNSUPPORTED_ASSET_IDENTITY",
                "preview",
                "No reviewed Ethereum mainnet identity is configured for the Binance coin.",
            )
        })?;
        if asset.decimals > 36
            || asset.network != format!("ethereum:{}", destination.chain_id)
            || (input.coin == "ETH" && asset.reference != "native")
            || (input.coin != "ETH"
                && asset.reference.to_lowercase()
                    != network
                        .contract_address
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase())
        {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "Binance ETH route contract does not match the configured Ethereum asset identity.",
            ));
        }
        let min_receive_atoms = decimal_to_atoms(&min_receive, asset.decimals)?;
        // The receiver bound is independently verified against the chain.
        // transactionFee is a separate venue debit, so a user may require the
        // full API quantity as the minimum payout.
        if min_receive.checked_cmp(&quantity).is_gt() {
            return Err(error(
                "RECEIPT_BOUND_UNSUPPORTED",
                "preview",
                "Minimum receipt exceeds the exact API quantity bound.",
            ));
        }
        let debit_asset = AssetId {
            network: "binance".into(),
            reference: input.coin.clone(),
            decimals: 36,
        };
        let max_fee_atoms = decimal_to_atoms(&max_fee, debit_asset.decimals)?;
        let gross = add_decimal(&quantity, &max_fee)?;
        let _required_main = if let Some(funding) = &funding {
            add_decimal(&gross, &funding.total_amount)?
        } else {
            gross.clone()
        };
        let balances = self.spot_balances(account, None).await?;
        let free = Self::free(&balances, &input.coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        let projected = if let Some(funding) = &funding {
            add_decimal(&free, &funding.total_amount)?
        } else {
            free.clone()
        };
        if projected.checked_cmp(&gross).is_lt() {
            return Err(error(
                "INSUFFICIENT_BALANCE",
                "preview",
                "Main SPOT free balance must cover the API amount plus approved maximum fee.",
            ));
        }
        let verifier = self.recipient_verifier.as_ref().ok_or_else(|| {
            error(
                "RECEIPT_VERIFIER_MISSING",
                "preview",
                "Ethereum recipient receipt verification is required before withdrawal.",
            )
        })?;
        let client_id = ClientIdentity::withdrawal(&input.request_id).map_err(domain_error)?;
        let prepared_request = StandardWithdrawalIntent {
            coin: input.coin.clone(),
            amount: quantity.clone(),
            address: destination.address.clone(),
            client_id: client_id.clone(),
        }
        .request()
        .map_err(domain_error)?;
        let mut valuation = self
            .valuation(&input.coin, &quantity.canonical(), &max_fee.canonical())
            .await?;
        if let Some(valuation) = valuation.as_mut()
            && let Some(exchange_price) = valuation.prices.first().cloned()
        {
            valuation.prices.push(AssetPrice {
                asset: asset.clone(),
                usd_micros_per_unit: exchange_price.usd_micros_per_unit,
                unit_atoms: Quantity::new(format!("1{}", "0".repeat(asset.decimals as usize)))
                    .map_err(domain_error)?,
                observed_at: exchange_price.observed_at,
                source: exchange_price.source,
            });
        }
        let mut binding = json!({
            "account": account.name, "destination": destination.name, "chainId": 1,
            "asset": asset, "debitAsset": debit_asset, "params": prepared_request, "maximumFee": max_fee.canonical(),
            "maximumGrossDebit": gross.canonical(), "minimumReceive": min_receive.canonical(),
            "maximumReceive": quantity.canonical(), "confirmations": destination.confirmations,
            "observedFree": free.canonical(), "publishedFee": fee.canonical(),
            "withdrawOrderId": client_id.as_str()
        });
        let mut warnings = vec!["Binance documents network fee and transaction amount separately from recipient net receipt. Debit is bounded by API amount plus the maximum fee; receipt is bounded by API amount minus maximum fee.".to_owned()];
        let mut checks = vec![
            Self::check(
                "permission",
                CheckStatus::Passed,
                "Withdrawal permission was observed on the selected main key.",
            ),
            Self::check(
                "travel_rule",
                CheckStatus::Passed,
                "Account reported questionnaireCountryCode NIL.",
            ),
            Self::check(
                "network_asset",
                CheckStatus::Passed,
                "ETH network and reviewed Ethereum mainnet asset identity match.",
            ),
            Self::check(
                "precision_balance_limits",
                CheckStatus::Passed,
                "Exact Binance funding increment, available free balance, and advertised limits were checked.",
            ),
            Self::check(
                "recipient_receipt",
                CheckStatus::Unchecked,
                "Recipient, token/native transfer, exact amount, chain, and confirmations are checked after submission.",
            ),
        ];
        if let Some(funding) = funding {
            binding["preparationOnly"] = json!(true);
            binding["fundingDependencies"] =
                serde_json::to_value(&funding.sources).unwrap_or(Value::Null);
            binding["projectedMainFree"] = json!(projected.canonical());
            checks.push(Self::check("funding_dependency", CheckStatus::Unchecked, "Preparation only. Complete and reconcile every selected child transfer, then prepare a fresh ordinary withdrawal preview and obtain new authorization."));
            warnings.push("This conditional funding preparation cannot be submitted or revalidated; it authorizes no withdrawal.".into());
        }
        let effects = vec![EffectConstraint::Transfer {
            from: account.resource.clone(),
            to: destination.resource.clone(),
            debit_asset: debit_asset.clone(),
            credit_asset: asset.clone(),
            maximum_debit: decimal_to_atoms(&quantity, debit_asset.decimals)?,
            minimum_credit: min_receive_atoms,
        }];
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: debit_asset.clone(),
                    atoms: decimal_to_atoms(&quantity, debit_asset.decimals)?,
                },
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: account.resource.clone(),
                },
                amount: Amount {
                    asset: debit_asset,
                    atoms: max_fee_atoms,
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: effects,
            locks: vec![account.resource.clone(), destination.resource.clone()],
        };
        let _ = verifier;
        Ok(prepared(
            "binance.withdraw",
            serde_json::to_value(&input).unwrap_or(Value::Null),
            binding,
            format!(
                "Withdraw API quantity {} {} from {} SPOT to {} ({}) on Ethereum mainnet; published fee {}, approved maximum fee {}, maximum gross debit {}, minimum net receipt {}.",
                quantity.canonical(),
                input.coin,
                account.name,
                destination.name,
                destination.address,
                fee.canonical(),
                max_fee.canonical(),
                gross.canonical(),
                min_receive.canonical()
            ),
            checks,
            warnings,
            valuation,
            intent,
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AccountInput {
    account: String,
    authority: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AccountNameInput {
    account: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AuthorityInput {
    authority: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NetworksInput {
    account: String,
    coin: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DepositAddressInput {
    account: String,
    coin: String,
    network: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct HistoryInput {
    account: String,
    coin: Option<String>,
    start_time: Option<u64>,
    end_time: Option<u64>,
    tx_hash: Option<String>,
    withdraw_order_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TransferInput {
    request_id: String,
    authority: String,
    from: String,
    to: String,
    coin: String,
    amount: String,
    from_wallet: String,
    to_wallet: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WithdrawInput {
    request_id: String,
    account: String,
    destination: String,
    coin: String,
    amount: String,
    network: String,
    wallet_type: String,
    max_fee: String,
    min_receive_amount: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FundingSourceInput {
    authority: String,
    from: String,
    coin: String,
    amount: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConditionalFundingSource {
    pub authority: String,
    pub from: String,
    pub coin: String,
    pub amount: String,
}

#[derive(Clone, Debug)]
struct ConditionalFunding {
    sources: Vec<FundingSourceInput>,
    total_amount: ExactDecimal,
}

pub fn operation_definitions() -> Vec<Definition> {
    definitions()
}

fn definitions() -> Vec<Definition> {
    let defs = [
        (
            "binance.account-identity",
            "Authenticated Binance UID and SPOT account type from the fixed Account endpoint, checked against the enrolled physical resource.",
            OperationKind::Read,
            schema_for!(AccountNameInput),
            "Physical account identity observed; no submission.",
        ),
        (
            "binance.balances",
            "Observed Binance SPOT free and committed balances. Main authority can read a configured subaccount.",
            OperationKind::Read,
            schema_for!(AccountInput),
            "Observed balances; no submission.",
        ),
        (
            "binance.permissions",
            "Observed Binance API permissions for the selected named connection. Unknown values remain unknown.",
            OperationKind::Read,
            schema_for!(AccountNameInput),
            "Permissions observed; availability is not eligibility.",
        ),
        (
            "binance.hierarchy",
            "Configured main/subaccount hierarchy compared with the main account subaccount list.",
            OperationKind::Read,
            schema_for!(AuthorityInput),
            "Configured child identities compared with venue records.",
        ),
        (
            "binance.networks",
            "Current Binance asset networks, precision, fee, minimum, maximum, and deposit/withdraw availability.",
            OperationKind::Read,
            schema_for!(NetworksInput),
            "Current funding metadata observed.",
        ),
        (
            "binance.deposit-address",
            "Selected Binance deposit address and memo/tag. Address lookup is read-only.",
            OperationKind::Read,
            schema_for!(DepositAddressInput),
            "Address observed; credit requires a matching Binance deposit record.",
        ),
        (
            "binance.deposit-history",
            "Paginated Binance deposit history with pending, locked, and usable credit kept distinct.",
            OperationKind::Read,
            schema_for!(HistoryInput),
            "A matching credited record establishes usable exchange credit.",
        ),
        (
            "binance.withdrawal-history",
            "Paginated Binance withdrawal history; venue completion and destination chain receipt are separate.",
            OperationKind::Read,
            schema_for!(HistoryInput),
            "Venue withdrawal history observed.",
        ),
        (
            "binance.transfer",
            "Main-authorized SPOT transfer between a configured main account and one configured subaccount, with persistent clientTranId.",
            OperationKind::Write,
            schema_for!(TransferInput),
            "One matching successful history row plus observed source debit and destination credit.",
        ),
        (
            "binance.withdraw",
            "Standard Binance SPOT withdrawal to a named Ethereum mainnet wallet with current fee, network, precision, Travel Rule, and receipt checks.",
            OperationKind::Write,
            schema_for!(WithdrawInput),
            "Matching completed withdrawal, bounded source debit/fee, and verified Ethereum mainnet recipient receipt.",
        ),
    ];
    defs.into_iter().map(|(name, description, kind, input, completion)| Definition {
        name: name.into(), description: description.into(), kind,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: serde_json::to_value(input).unwrap_or(Value::Null),
        output_schema: json!({"type":"object"}),
        error_schema: schema_for!(FinError).into(),
        examples: vec![], coverage: "Endpoint fixtures and synthetic transport; production account permissions and live financial acceptance remain unverified.".into(),
        completion: completion.into(), semantics: json!({"walletType":"SPOT", "amount":"exact decimal string", "credentialValues":"never exposed"}),
    }).collect()
}

#[allow(clippy::too_many_arguments)]
fn prepared(
    operation: &str,
    input: Value,
    binding: Value,
    summary: String,
    checks: Vec<Check>,
    warnings: Vec<String>,
    valuation: Option<Valuation>,
    intent: FinancialIntent,
) -> PreparedOperation {
    let now = Utc::now();
    PreparedOperation {
        operation: operation.into(),
        input,
        binding,
        summary,
        checks,
        warnings,
        observed_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        expires_at: (now + Duration::seconds(60))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        valuation,
        intent,
        operation_version: 1,
    }
}

fn error(code: &str, stage: &str, message: impl Into<String>) -> FinError {
    FinError::new(
        code,
        stage,
        message,
        "Inspect operation status and reconcile the original request before starting another financial action.",
    )
}

fn http_failure_error(
    stage: &str,
    status: u16,
    binance_code: Option<i64>,
    kind: HttpFailureKind,
) -> FinError {
    let mut error = match kind {
        HttpFailureKind::RateLimited => error(
            "VENUE_RATE_LIMIT",
            stage,
            "Binance rate limited the request.",
        ),
        HttpFailureKind::PermissionDenied => error(
            "VENUE_PERMISSION",
            stage,
            "The selected Binance credential was denied.",
        ),
        HttpFailureKind::Rejected => error(
            "VENUE_REQUEST_FAILED",
            stage,
            "Binance rejected the request; reconcile any persisted submission before retrying.",
        ),
        HttpFailureKind::InvalidResponse => error(
            "VENUE_RESPONSE_INVALID",
            stage,
            "Binance returned an invalid response; reconcile any persisted submission before retrying.",
        ),
    };
    // These values are parsed from the protocol status and a JSON signed
    // integer only. Never attach upstream messages or response contents.
    error.message.push_str(&format!(" [HTTP {status}"));
    if let Some(code) = binance_code {
        error.message.push_str(&format!("; Binance code {code}"));
    }
    error.message.push(']');
    error
}

fn domain_error(error: impl std::fmt::Display) -> FinError {
    FinError::new(
        "BINANCE_VALIDATION",
        "validation",
        error.to_string(),
        "Inspect the operation input and configured Binance connection metadata.",
    )
}
fn observed_decimal(input: &str) -> Result<ExactDecimal, FinError> {
    ExactDecimal::parse(input).map_err(domain_error)
}

fn amount(input: &str) -> Result<ExactDecimal, FinError> {
    let result = ExactDecimal::parse(input).map_err(domain_error)?;
    if result.is_zero() {
        return Err(error(
            "INVALID_AMOUNT",
            "validation",
            "Financial amount must be positive.",
        ));
    }
    Ok(result)
}
fn decimal_to_atoms(value: &ExactDecimal, decimals: u8) -> Result<Quantity, FinError> {
    Quantity::from_decimal(&value.canonical(), decimals).map_err(|_| error("INVALID_PRECISION", "preview", "Amount cannot be represented in the selected asset's exact base units; no rounding is performed."))
}
fn add_decimal(left: &ExactDecimal, right: &ExactDecimal) -> Result<ExactDecimal, FinError> {
    let scale = left.scale().max(right.scale());
    let l = left.coefficient_for_scale(scale);
    let r = right.coefficient_for_scale(scale);
    ExactDecimal::from_coefficient(l + r, scale).map_err(domain_error)
}
fn decimal_product_micros(amount_text: &str, price: &str) -> Result<String, FinError> {
    // Zero fees are valid valuation inputs even though financial operation
    // quantities themselves must be strictly positive.
    let quantity = ExactDecimal::parse(amount_text).map_err(domain_error)?;
    let price = ExactDecimal::parse(price).map_err(domain_error)?;
    let scale = quantity.scale().saturating_add(price.scale());
    let product = quantity.coefficient_for_scale(quantity.scale())
        * price.coefficient_for_scale(price.scale());
    let numerator = product * BigUint::from(1_000_000u64);
    let divisor = BigUint::from(10u8).pow(scale);
    let micros = (numerator + (&divisor - BigUint::from(1u8))) / divisor;
    Ok(micros.to_string())
}
fn valid_eth_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[async_trait]
impl Adapter for BinanceAdapter {
    fn definitions(&self) -> Vec<Definition> {
        definitions()
    }

    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.config
            .accounts
            .iter()
            .map(|account| ConnectionMetadata {
                name: account.name.clone(),
                provider: "binance".into(),
                resource: account.resource.clone(),
                role: Some(
                    match account.role {
                        AccountRole::Main => "main",
                        AccountRole::Sub => "sub",
                    }
                    .into(),
                ),
                parent: account.parent.clone(),
                network: None,
                address: None,
                credential_ref: None,
                signing_enabled: matches!(account.role, AccountRole::Main),
                details: json!({"walletType":"SPOT", "credentialEnrolled":true}),
            })
            .collect()
    }

    async fn snapshot(&self) -> Result<StateSnapshot, FinError> {
        let end = Utc::now().timestamp_millis().max(0) as u64;
        let start_90d = end.saturating_sub(90 * 86_400_000).saturating_add(1);
        let start_7d = end.saturating_sub(7 * 86_400_000).saturating_add(1);
        let mut state = FinancialState::default();
        let mut observer_complete = true;
        let mut coverage = Vec::new();
        let mut pending = Vec::new();
        for account in &self.config.accounts {
            let mut identity_verified = false;
            match self.account_identity(account).await {
                Ok((uid, _)) => {
                    identity_verified = true;
                    let _ = uid;
                }
                Err(_) => {
                    observer_complete = false;
                    pending.push(format!(
                        "{} Binance SPOT identity unavailable or mismatched",
                        account.name
                    ));
                }
            }
            let balance_authority = match account.role {
                AccountRole::Main => None,
                AccountRole::Sub => account.parent.as_deref().and_then(|parent| {
                    self.config
                        .accounts
                        .iter()
                        .find(|candidate| candidate.name == parent)
                }),
            };
            match self.spot_balances(account, balance_authority).await {
                Ok(balances) => {
                    if identity_verified {
                        // The engine needs stable physical-resource coverage
                        // keys in addition to the human-readable observation
                        // horizons below.
                        coverage.push(account.resource.0.clone());
                    }
                    for row in balances {
                        if row.freeze.as_ref().is_some_and(|v| !v.is_zero())
                            || row.withdrawing.as_ref().is_some_and(|v| !v.is_zero())
                        {
                            observer_complete = false;
                            pending.push(format!("{}:{} additional freeze/withdrawing balance semantics are unclassified", account.name, row.asset));
                        }
                        let total = add_decimal(&row.free, &row.locked)?;
                        let atoms =
                            Quantity::from_decimal(&total.canonical(), 36).map_err(domain_error)?;
                        state.holdings.push(ResourceAmount {
                            resource: account.resource.clone(),
                            amount: Amount {
                                asset: AssetId {
                                    network: "binance".into(),
                                    reference: row.asset,
                                    decimals: 36,
                                },
                                atoms,
                            },
                        });
                    }
                }
                Err(_) => {
                    observer_complete = false;
                    pending.push(format!(
                        "{} SPOT balance observation unavailable",
                        account.name
                    ));
                }
            }
            if let AccountRole::Sub = account.role {
                state.authorities.push(AuthorityState {
                    resource: account.resource.clone(),
                    authority: "binance_main_account".into(),
                    value: account.parent.clone().unwrap_or_else(|| "unknown".into()),
                });
            }
            for (deposit, label) in [(true, "deposit"), (false, "withdrawal")] {
                let input = HistoryInput {
                    account: account.name.clone(),
                    coin: None,
                    start_time: Some(start_90d),
                    end_time: Some(end),
                    tx_hash: None,
                    withdraw_order_id: None,
                };
                match self.read_funding_history(account, deposit, &input).await {
                    Ok(history) => {
                        let history_complete = history
                            .get("complete")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let records = history
                            .get("records")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        for record in records {
                            let state_name = if deposit {
                                record
                                    .get("creditState")
                                    .and_then(Value::as_str)
                                    .unwrap_or("pending")
                            } else {
                                record
                                    .get("venueState")
                                    .and_then(Value::as_str)
                                    .unwrap_or("pending")
                            };
                            if state_name == "pending" || state_name == "credited_locked" {
                                let id = record
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                pending.push(format!(
                                    "binance:{label}:{}:{id}:{state_name}",
                                    account.name
                                ));
                                state.in_flight.push(format!(
                                    "binance:{label}:{}:{id}:{state_name}",
                                    account.name
                                ));
                            }
                        }
                        if !history_complete {
                            observer_complete = false;
                            pending.push(format!("{} {label} history reached the adapter pagination or lookback boundary", account.name));
                        }
                    }
                    Err(_) => {
                        observer_complete = false;
                        pending.push(format!("{} {label} history unavailable", account.name));
                    }
                }
            }
        }
        // Binance's universal-transfer history endpoint only supports a short
        // lookback. Query it for current unresolved movements and make the
        // history horizon explicit instead of claiming an all-time baseline.
        for main in self
            .config
            .accounts
            .iter()
            .filter(|a| matches!(a.role, AccountRole::Main))
        {
            let sources = self
                .config
                .accounts
                .iter()
                .filter(|source| {
                    source.name == main.name || source.parent.as_deref() == Some(main.name.as_str())
                })
                .collect::<Vec<_>>();
            let mut seen = BTreeSet::new();
            for source in sources {
                let mut source_complete = false;
                for page in 1..=20u64 {
                    let mut request =
                        BinanceRequest::new(BinanceEndpoint::UniversalTransferHistory)
                            .push("startTime", start_7d.to_string())
                            .push("endTime", end.to_string())
                            .push("page", page.to_string())
                            .push("limit", "500");
                    if let Some(email) = source.email.as_deref() {
                        request = request.push("fromEmail", email);
                    }
                    let response = match self.call(main, request, "snapshot").await {
                        Ok(value) => value,
                        Err(_) => {
                            observer_complete = false;
                            break;
                        }
                    };
                    let batch: TransferHistoryResponse = match parse_response(response) {
                        Ok(value) => value,
                        Err(_) => {
                            observer_complete = false;
                            break;
                        }
                    };
                    let count = batch.result.len();
                    for row in batch.result {
                        if !seen.insert(row.tran_id.as_str().to_owned()) {
                            continue;
                        }
                        if row.status != "SUCCESS"
                            && row.status != "FAILED"
                            && row.status != "FAILURE"
                        {
                            let movement = format!(
                                "binance:transfer:{}:{}:{}",
                                row.client_tran_id
                                    .as_deref()
                                    .unwrap_or(row.tran_id.as_str()),
                                row.asset,
                                row.status
                            );
                            state.in_flight.push(movement.clone());
                            pending.push(movement);
                        }
                    }
                    if count < 500 || batch.total_count.is_some_and(|total| page * 500 >= total) {
                        source_complete = true;
                        break;
                    }
                }
                if !source_complete {
                    observer_complete = false;
                    pending.push(format!(
                        "{} universal-transfer history is incomplete or unavailable",
                        source.name
                    ));
                }
            }
        }
        Ok(StateSnapshot {
            state,
            observed_at: now(),
            complete: observer_complete && pending.is_empty(),
            coverage,
            pending,
        })
    }

    async fn preflight_return_route(
        &self,
        operation: &str,
        raw: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, FinError> {
        if operation == "binance.withdraw" {
            return self.preflight_withdrawal(raw, context).await;
        }
        if operation != "binance.transfer" {
            return Err(error(
                "RETURN_ROUTE_UNSUPPORTED",
                "preflight",
                "Only main-authorized Binance SPOT transfers have a conditional return proof.",
            ));
        }
        let input: TransferInput = self.parse_input(raw).await?;
        let (authority, from, to) = self.validate_transfer_route(&input)?;
        if input.from_wallet != "SPOT" || input.to_wallet != "SPOT" {
            return Err(error(
                "UNSUPPORTED_WALLET",
                "preflight",
                "Only Binance SPOT wallet transfers can be planned.",
            ));
        }
        for account in [from, to] {
            if account.name != authority.name {
                self.verify_subaccount(account, authority).await?;
            }
            if !context.baseline.coverage.contains(&account.resource.0) {
                return Err(error(
                    "PLAN_BASELINE_INCOMPLETE",
                    "preflight",
                    "Every physical Binance transfer resource must be captured in the baseline.",
                ));
            }
        }
        let permissions = self.permissions(authority).await?;
        if permissions.enable_internal_transfer != Some(true)
            || permissions.permits_universal_transfer != Some(true)
        {
            return Err(error(
                "MISSING_PERMISSION",
                "preflight",
                "Main credential must report internal-transfer and universal-transfer permissions.",
            ));
        }
        let currency = self.currency(authority, &input.coin).await?;
        let increment = currency
            .network_list
            .iter()
            .find(|network| network.network == "ETH")
            .or_else(|| currency.network_list.first())
            .and_then(|network| network.withdraw_integer_multiple.as_ref())
            .ok_or_else(|| {
                error(
                    "PRECISION_UNKNOWN",
                    "preflight",
                    "Binance funding increment is unavailable for this asset.",
                )
            })?;
        let decimal = amount(&input.amount)?;
        if decimal.is_zero() || !decimal.is_multiple_of(increment) {
            return Err(error(
                "INVALID_PRECISION",
                "preflight",
                "Planned transfer must be positive and match the current Binance increment.",
            ));
        }
        let asset = AssetId {
            network: "binance".into(),
            reference: input.coin.clone(),
            decimals: 36,
        };
        let atoms = decimal_to_atoms(&decimal, asset.decimals)?;
        let available = context
            .projected
            .cash
            .iter()
            .find(|cash| cash.resource == from.resource && cash.asset == asset)
            .map(|cash| cash.amount.minimum.clone())
            .unwrap_or_else(Quantity::zero);
        if available.integer() < atoms.integer() {
            return Err(error(
                "PLAN_FUNDS_UNAVAILABLE",
                "preflight",
                "Projected SPOT cash does not guarantee the planned transfer amount.",
            ));
        }
        let source_step = context
            .prior_proofs
            .iter()
            .rev()
            .find(|proof| {
                proof.effects.iter().any(|effect| {
                    matches!(effect, ProjectedEffect::CashDelta { resource, asset: credited_asset, maximum_delta, .. }
                        if resource == &from.resource
                            && credited_asset == &asset
                            && !maximum_delta.is_negative()
                            && maximum_delta.magnitude().integer() >= atoms.integer())
                })
            })
            .map(|proof| proof.step_index);
        let baseline_amount = context
            .baseline
            .state
            .holdings
            .iter()
            .find(|holding| holding.resource == from.resource && holding.amount.asset == asset)
            .map(|holding| holding.amount.atoms.integer())
            .unwrap_or_default();
        let baseline_still_funds_request = baseline_amount >= atoms.integer()
            && !context.prior_proofs.iter().any(|proof| {
                proof.effects.iter().any(|effect| {
                    matches!(effect, ProjectedEffect::CashDelta { resource, asset: spent_asset, minimum_delta, .. }
                        if resource == &from.resource
                            && spent_asset == &asset
                            && minimum_delta.is_negative())
                })
            });
        if source_step.is_none() && !baseline_still_funds_request {
            return Err(error(
                "PLAN_SOURCE_UNATTRIBUTED",
                "preflight",
                "Projected cash combines baseline and future credits without one provable source step.",
            ));
        }
        let client_request = UniversalTransferIntent {
            asset: input.coin.clone(),
            amount: decimal,
            from_email: from.email.clone().filter(|_| from.name != authority.name),
            to_email: to.email.clone().filter(|_| to.name != authority.name),
            client_id: ClientIdentity::transfer(&input.request_id).map_err(domain_error)?,
        }
        .request()
        .map_err(domain_error)?;
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: from.resource.clone(),
                amount: Amount {
                    asset: asset.clone(),
                    atoms: atoms.clone(),
                },
            }],
            maximum_costs: vec![],
            expected: vec![EffectConstraint::Transfer {
                from: from.resource.clone(),
                to: to.resource.clone(),
                debit_asset: asset.clone(),
                credit_asset: asset.clone(),
                maximum_debit: atoms.clone(),
                minimum_credit: atoms.clone(),
            }],
            locks: vec![from.resource.clone(), to.resource.clone()],
        };
        let now_time = Utc::now();
        Ok(ReturnRouteProof {
            step_index: context.step_index,
            operation: operation.into(),
            input_hash: binding_hash(raw).map_err(domain_error)?,
            observed_at: now_time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            expires_at: (now_time + Duration::seconds(60))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            intent,
            effects: vec![
                ProjectedEffect::CashDelta {
                    resource: from.resource.clone(),
                    asset: asset.clone(),
                    minimum_delta: SignedQuantity::negative(&atoms),
                    maximum_delta: SignedQuantity::negative(&atoms),
                },
                ProjectedEffect::CashDelta {
                    resource: to.resource.clone(),
                    asset: asset.clone(),
                    minimum_delta: SignedQuantity::positive(&atoms),
                    maximum_delta: SignedQuantity::positive(&atoms),
                },
            ],
            requires: vec![ProjectedRequirement::CashAtLeast {
                resource: from.resource.clone(),
                asset,
                amount: atoms,
                source_step,
            }],
            evidence: json!({
                "method":"binance-conditional-spot-transfer-v1",
                "authority":authority.name,
                "from":from.resource,
                "to":to.resource,
                "clientTranId":client_request.get("clientTranId"),
                "permission":"current main credential reports internal-transfer and universal-transfer enabled",
                "precisionIncrement":increment.canonical(),
                "source":"broker-projected exact SPOT cash; future receipts are conditional on earlier proof chain",
                "execution":"Every actual transfer still receives a fresh ordinary preview and per-write authorization"
            }),
        })
    }

    async fn read(&self, operation: &str, raw: &Value) -> Result<Value, FinError> {
        match operation {
            "binance.account-identity" => {
                let input: AccountNameInput = self.parse_input(raw).await?;
                let account = self.account(&input.account)?;
                let (uid, account_type) = self.account_identity(account).await?;
                Ok(
                    json!({"operation":operation,"account":account.name,"uid":uid,"accountType":account_type,"resource":account.resource,"walletType":"SPOT","observedAt":now()}),
                )
            }
            "binance.balances" => {
                let input: AccountInput = self.parse_input(raw).await?;
                let account = self.account(&input.account)?;
                let authority = input
                    .authority
                    .as_deref()
                    .map(|name| self.account(name))
                    .transpose()?;
                let balances = self.spot_balances(account, authority).await?;
                let balances: Vec<Value> = balances.into_iter().map(|balance| json!({
                    "asset": balance.asset, "free": balance.free.canonical(), "locked": balance.locked.canonical(),
                    "freeze": balance.freeze.map(|value| value.canonical()), "withdrawing": balance.withdrawing.map(|value| value.canonical()),
                })).collect();
                Ok(
                    json!({"operation": operation, "account": account.name, "walletType": "SPOT", "observedAt": now(), "balances": balances}),
                )
            }
            "binance.permissions" => {
                let input: AccountNameInput = self.parse_input(raw).await?;
                let account = self.account(&input.account)?;
                let permission = self.permissions(account).await?;
                let unknown =
                    |value: Option<bool>| value.map(Value::Bool).unwrap_or(json!("unknown"));
                Ok(
                    json!({"operation": operation, "account": account.name, "observedAt": now(), "permissions": {
                        "enableReading": unknown(permission.enable_reading), "enableWithdrawals": unknown(permission.enable_withdrawals),
                        "enableInternalTransfer": unknown(permission.enable_internal_transfer), "permitsUniversalTransfer": unknown(permission.permits_universal_transfer),
                        "ipRestrict": unknown(permission.ip_restrict), "enableSpotAndMarginTrading": unknown(permission.enable_spot_and_margin_trading),
                    }}),
                )
            }
            "binance.hierarchy" => {
                let input: AuthorityInput = self.parse_input(raw).await?;
                let authority = self.require_main(&input.authority)?;
                let children: Vec<&BinanceAccount> = self
                    .config
                    .accounts
                    .iter()
                    .filter(|account| account.parent.as_deref() == Some(authority.name.as_str()))
                    .collect();
                let mut discovered = Vec::new();
                for child in children {
                    let state = match self.verify_subaccount(child, authority).await {
                        Ok(()) => json!({"ownership":"observed", "walletTypes":["SPOT"]}),
                        Err(reason) => {
                            json!({"ownership":"unverified", "reason":reason.code, "walletTypes":["SPOT"]})
                        }
                    };
                    discovered.push(json!({"name":child.name,"role":"sub","parent":authority.name,"email":child.email,"resource":child.resource,"state":state}));
                }
                Ok(
                    json!({"operation":operation,"authority":authority.name,"observedAt":now(),"accounts":discovered,"accountModel":"main_authorized_SPOT","subAuthorizedTransfer":"not_implemented"}),
                )
            }
            "binance.networks" => {
                let input: NetworksInput = self.parse_input(raw).await?;
                let account = self.account(&input.account)?;
                let currencies = self.capital_config(account).await?;
                let currencies: Vec<_> = currencies.into_iter().filter(|currency| input.coin.as_ref().is_none_or(|coin| &currency.coin == coin)).map(|currency| json!({
                    "coin":currency.coin, "depositAllEnable":currency.deposit_all_enable, "withdrawAllEnable":currency.withdraw_all_enable,
                    "networks":currency.network_list.into_iter().map(|network| json!({
                        "network":network.network,"coin":network.coin,"depositEnable":network.deposit_enable,"withdrawEnable":network.withdraw_enable,
                        "withdrawIntegerMultiple":network.withdraw_integer_multiple.map(|value|value.canonical()),"withdrawFee":network.withdraw_fee.map(|value|value.canonical()),
                        "withdrawMin":network.withdraw_min.map(|value|value.canonical()),"withdrawMax":network.withdraw_max.map(|value|value.canonical()),
                        "depositDust":network.deposit_dust.map(|value|value.canonical()),"minConfirm":network.min_confirm,"unLockConfirm":network.unlock_confirm,
                        "withdrawTag":network.withdraw_tag,"busy":network.busy,"contractAddress":network.contract_address,"denomination":network.denomination
                    })).collect::<Vec<_>>()
                })).collect();
                Ok(
                    json!({"operation":operation,"account":account.name,"observedAt":now(),"assets":currencies}),
                )
            }
            "binance.deposit-address" => {
                let input: DepositAddressInput = self.parse_input(raw).await?;
                let account = self.account(&input.account)?;
                let currency = self.currency(account, &input.coin).await?;
                let network = self.network(&currency, &input.network)?;
                if network.deposit_enable != Some(true) {
                    return Err(error(
                        "DEPOSIT_DISABLED",
                        "read",
                        "Selected Binance deposit route is unavailable.",
                    ));
                }
                let response = self
                    .call(
                        account,
                        BinanceRequest::new(BinanceEndpoint::DepositAddress)
                            .push("coin", &input.coin)
                            .push("network", &input.network),
                        "read",
                    )
                    .await?;
                let address: DepositAddress = parse_response(response).map_err(|_| {
                    error(
                        "VENUE_RESPONSE_INVALID",
                        "read",
                        "Binance deposit-address response was invalid.",
                    )
                })?;
                if address.address.is_empty()
                    || address
                        .coin
                        .as_deref()
                        .is_some_and(|coin| coin != input.coin)
                    || address
                        .network
                        .as_deref()
                        .is_some_and(|network| network != input.network)
                {
                    return Err(error(
                        "ASSET_IDENTITY_MISMATCH",
                        "read",
                        "Binance returned an address for another asset or network.",
                    ));
                }
                if input.network == "ETH" && !valid_eth_address(&address.address) {
                    return Err(error(
                        "VENUE_RESPONSE_INVALID",
                        "read",
                        "Binance returned an invalid Ethereum deposit address.",
                    ));
                }
                Ok(
                    json!({"operation":operation,"account":account.name,"observedAt":now(),"coin":input.coin,"network":input.network,"address":address.address,"memo":address.tag.unwrap_or_default(),"tokenContract":network.contract_address.as_deref().unwrap_or("unknown"),"denomination":network.denomination.map(|v|json!(v)).unwrap_or(json!("unknown")),"minimumConfirmations":network.min_confirm.map(|v|json!(v)).unwrap_or(json!("unknown")),"unlockConfirmations":network.unlock_confirm.map(|v|json!(v)).unwrap_or(json!("unknown")),"minimumDeposit":network.deposit_dust.as_ref().map(|v|json!(v.canonical())).unwrap_or(json!("unknown")),"usableCreditRequires":"matching Binance deposit history with creditState=credited"}),
                )
            }
            "binance.deposit-history" | "binance.withdrawal-history" => {
                let input: HistoryInput = self.parse_input(raw).await?;
                let account = self.account(&input.account)?;
                let deposit = operation == "binance.deposit-history";
                if (!deposit && input.tx_hash.is_some())
                    || (deposit && input.withdraw_order_id.is_some())
                {
                    return Err(FinError::validation(
                        "History filters must match the selected deposit or withdrawal operation.",
                    ));
                }
                self.read_funding_history(account, deposit, &input).await
            }
            _ => Err(error(
                "UNSUPPORTED_OPERATION",
                "validation",
                "Binance operation is not implemented.",
            )),
        }
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation, FinError> {
        match operation {
            "binance.transfer" => self.prepare_transfer(self.parse_input(input).await?).await,
            "binance.withdraw" => {
                self.prepare_withdrawal(self.parse_input(input).await?, None)
                    .await
            }
            _ => Err(error(
                "WRONG_OPERATION_KIND",
                "preview",
                "Selected Binance operation is read-only or unsupported for financial preparation.",
            )),
        }
    }

    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, FinError> {
        if prepared.binding.get("preparationOnly") == Some(&json!(true)) {
            return Err(error(
                "PREPARATION_ONLY",
                "submission",
                "Conditional funding preparation cannot be revalidated or submitted.",
            ));
        }
        let expires = DateTime::parse_from_rfc3339(&prepared.expires_at).map_err(|_| {
            error(
                "PREVIEW_INVALID",
                "submission",
                "Prepared operation expiry is invalid.",
            )
        })?;
        if expires.with_timezone(&Utc) <= Utc::now() {
            return Err(error(
                "PREVIEW_EXPIRED",
                "submission",
                "Prepared operation expired; obtain a fresh preview and approval.",
            ));
        }
        let current = self.prepare(&prepared.operation, &prepared.input).await?;
        if current.binding != prepared.binding
            || !approved_native_valuation_matches(prepared, &current)
        {
            return Err(error(
                "PREVIEW_CHANGED",
                "submission",
                "Material Binance parameters or valuation changed; obtain fresh authorization.",
            ));
        }
        Ok(current)
    }

    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, FinError> {
        if prepared.binding.get("preparationOnly") == Some(&json!(true)) {
            return Err(error(
                "PREPARATION_ONLY",
                "submission",
                "Conditional funding preparation cannot be submitted.",
            ));
        }
        let current = self.revalidate(prepared).await?;
        journal.assert_authorized()?;
        let input: Value = current.input.clone();
        let submission;
        let authority;
        let request;
        match current.operation.as_str() {
            "binance.transfer" => {
                let input: TransferInput = self.parse_input(&input).await?;
                let (main, from, to) = self.validate_transfer_route(&input)?;
                let (r, source_before, dest_before) = self.transfer_route(&input).await?;
                request = r;
                // Binance's universal-transfer history defaults to the main
                // account's outgoing transfers. Persist the one route filter
                // needed to find the exact child leg after a lost response.
                let (selector_parameter, selector_value) = if from.name == main.name {
                    ("toEmail", request.get("toEmail"))
                } else {
                    ("fromEmail", request.get("fromEmail"))
                };
                let selector_value = selector_value.ok_or_else(|| {
                    error(
                        "TRANSFER_ROUTE_INVALID",
                        "submission",
                        "The child account email required for transfer history correlation is unavailable.",
                    )
                })?;
                let expected_email = if selector_parameter == "toEmail" {
                    to.email.as_deref()
                } else {
                    from.email.as_deref()
                };
                if Some(selector_value) != expected_email {
                    return Err(error(
                        "TRANSFER_ROUTE_INVALID",
                        "submission",
                        "The transfer history selector does not match the configured child account.",
                    ));
                }
                let history_selector =
                    json!({"parameter":selector_parameter,"value":selector_value});
                authority = main;
                submission = Submission {
                    kind: "binance".into(),
                    reference: request.get("clientTranId").unwrap_or_default().into(),
                    data: json!({
                        "operation":"binance.transfer","binding":current.binding,"submittedAt":now(),"sourceFreeBefore":source_before,"destinationFreeBefore":dest_before,
                        "authority":main.name,"from":from.name,"to":to.name,"coin":input.coin,"amount":input.amount,"fromEmail":from.email,"toEmail":to.email,
                        "historySelector":history_selector
                    }),
                };
            }
            "binance.withdraw" => {
                let input: WithdrawInput = self.parse_input(&input).await?;
                let main = self.require_main(&input.account)?;
                let destination = self.destination(&input.destination)?;
                let params: BinanceRequest =
                    serde_json::from_value(current.binding["params"].clone()).map_err(|_| {
                        error(
                            "PREVIEW_INVALID",
                            "submission",
                            "Prepared withdrawal request is invalid.",
                        )
                    })?;
                let balances = self.spot_balances(main, None).await?;
                let source_before = Self::free(&balances, &input.coin)
                    .cloned()
                    .unwrap_or_else(ExactDecimal::zero)
                    .canonical();
                authority = main;
                request = params;
                submission = Submission {
                    kind: "binance".into(),
                    reference: request.get("withdrawOrderId").unwrap_or_default().into(),
                    data: json!({
                        "operation":"binance.withdraw","binding":current.binding,"submittedAt":now(),"sourceFreeBefore":source_before,
                        "account":main.name,"destination":destination.name,"coin":input.coin,"amount":input.amount,"address":destination.address,
                        "maximumFee":current.binding["maximumFee"],"maximumGrossDebit":current.binding["maximumGrossDebit"],
                        "minimumReceive":current.binding["minimumReceive"],"maximumReceive":current.binding["maximumReceive"]
                    }),
                };
            }
            _ => {
                return Err(error(
                    "WRONG_OPERATION_KIND",
                    "submission",
                    "Selected Binance operation is not writable.",
                ));
            }
        }
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        let response = self.call(authority, request.clone(), "submission").await?;
        match current.operation.as_str() {
            "binance.transfer" => {
                let value = response.get("tranId").ok_or_else(|| error("SUBMISSION_RESPONSE_INVALID", "submission", "Binance transfer response lacks a transaction ID; reconcile its clientTranId."))?;
                // Validate any response transaction ID, but retain the
                // already-journaled clientTranId as the immutable submission
                // identity. History correlation is keyed by that ID; changing
                // durable identity after dispatch would either weaken the
                // one-use approval boundary or lose an accepted write.
                VenueIdentifier::parse(value).map_err(domain_error)?;
                if response
                    .get("clientTranId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != submission.reference)
                {
                    return Err(error(
                        "SUBMISSION_ID_MISMATCH",
                        "submission",
                        "Binance returned another clientTranId; reconcile the persisted request identity.",
                    ));
                }
            }
            "binance.withdraw" => {
                let value = response.get("id").ok_or_else(|| {
                    error(
                        "SUBMISSION_RESPONSE_INVALID",
                        "submission",
                        "Binance withdrawal response lacks an ID; reconcile its withdrawOrderId.",
                    )
                })?;
                VenueIdentifier::parse(value).map_err(domain_error)?;
            }
            _ => unreachable!(),
        }
        Ok(submission)
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, FinError> {
        let Some(submission) = &record.submission else {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"No persisted Binance submission identity exists."}),
                verified: None,
            });
        };
        if submission.kind != "binance" {
            return Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"Persisted submission kind does not match Binance."}),
                verified: None,
            });
        }
        match record.operation.as_str() {
            "binance.transfer" => self.reconcile_transfer(record, submission).await,
            "binance.withdraw" => self.reconcile_withdrawal(record, submission).await,
            _ => Ok(Reconciliation {
                status: Status::Unknown,
                evidence: json!({"reason":"Unsupported Binance reconciliation operation."}),
                verified: None,
            }),
        }
    }
}

impl BinanceAdapter {
    async fn preflight_withdrawal(
        &self,
        raw: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, FinError> {
        let input: WithdrawInput = self.parse_input(raw).await?;
        let account = self.require_main(&input.account)?;
        let destination = self.destination(&input.destination)?;
        if destination.chain_id != 1 || input.network != "ETH" || input.wallet_type != "SPOT" {
            return Err(error(
                "WRONG_NETWORK",
                "preflight",
                "Conditional withdrawal only supports ETH SPOT to an enrolled Ethereum mainnet wallet.",
            ));
        }
        if !context.baseline.coverage.contains(&account.resource.0)
            || !context.baseline.coverage.contains(&destination.resource.0)
        {
            return Err(error(
                "PLAN_BASELINE_INCOMPLETE",
                "preflight",
                "Withdrawal source and Ethereum recipient must both be covered by the captured baseline.",
            ));
        }
        if self.recipient_verifier.is_none() {
            return Err(error(
                "RECEIPT_VERIFIER_MISSING",
                "preflight",
                "A canonical Ethereum payout verifier is required for a planned withdrawal.",
            ));
        }
        let permissions = self.permissions(account).await?;
        if permissions.enable_withdrawals != Some(true) {
            return Err(error(
                "MISSING_PERMISSION",
                "preflight",
                "Selected main credential must report withdrawal permission.",
            ));
        }
        let travel: TravelRuleRequirements = parse_response(
            self.call(account, BinanceRequest::new(BinanceEndpoint::TravelRuleRequirements), "preflight").await?,
        ).map_err(|_| error("TRAVEL_RULE_UNKNOWN", "preflight", "Travel Rule requirements could not be established; standard withdrawal is blocked."))?;
        if travel.questionnaire_country_code != "NIL" {
            return Err(error(
                "TRAVEL_RULE_REQUIRED",
                "preflight",
                "This account requires an unsupported Travel Rule withdrawal flow.",
            ));
        }
        let currency = self.currency(account, &input.coin).await?;
        let network = self.network(&currency, "ETH")?;
        if currency.withdraw_all_enable != Some(true)
            || network.withdraw_enable != Some(true)
            || network.busy == Some(true)
            || network.withdraw_tag == Some(true)
        {
            return Err(error(
                "WITHDRAWAL_DISABLED",
                "preflight",
                "The reviewed Ethereum withdrawal route is disabled, busy, or requires an unsupported tag.",
            ));
        }
        let increment = network.withdraw_integer_multiple.as_ref().ok_or_else(|| {
            error(
                "PRECISION_UNKNOWN",
                "preflight",
                "Binance withdrawal increment is unavailable.",
            )
        })?;
        let fee = network.withdraw_fee.as_ref().ok_or_else(|| {
            error(
                "FEE_UNKNOWN",
                "preflight",
                "Binance withdrawal fee is unavailable.",
            )
        })?;
        let quantity = amount(&input.amount)?;
        let max_fee = amount(&input.max_fee)?;
        let min_receive = amount(&input.min_receive_amount)?;
        let min = network.withdraw_min.as_ref().ok_or_else(|| {
            error(
                "WITHDRAWAL_LIMIT_UNKNOWN",
                "preflight",
                "Binance withdrawal minimum is unavailable.",
            )
        })?;
        let max = network.withdraw_max.as_ref().ok_or_else(|| {
            error(
                "WITHDRAWAL_LIMIT_UNKNOWN",
                "preflight",
                "Binance withdrawal maximum is unavailable.",
            )
        })?;
        if !quantity.is_multiple_of(increment)
            || quantity.checked_cmp(min).is_lt()
            || quantity.checked_cmp(max).is_gt()
            || fee.checked_cmp(&max_fee).is_gt()
        {
            return Err(error(
                "WITHDRAWAL_LIMIT",
                "preflight",
                "Current fee, increment, or network limits do not support the planned withdrawal.",
            ));
        }
        let asset = destination.assets.get(&input.coin).ok_or_else(|| {
            error(
                "UNSUPPORTED_ASSET_IDENTITY",
                "preflight",
                "The destination has no reviewed Ethereum asset identity for this Binance coin.",
            )
        })?;
        if asset.network != format!("ethereum:{}", destination.chain_id)
            || asset.decimals != 18
            || (input.coin == "ETH" && asset.reference != "native")
        {
            return Err(error(
                "ASSET_IDENTITY_MISMATCH",
                "preflight",
                "Binance ETH withdrawal and Ethereum recipient asset identities do not match.",
            ));
        }
        if min_receive.checked_cmp(&quantity).is_gt() {
            return Err(error(
                "RECEIPT_BOUND_UNSUPPORTED",
                "preflight",
                "Minimum recipient credit exceeds the exact API quantity bound.",
            ));
        }
        let debit_asset = AssetId {
            network: "binance".into(),
            reference: input.coin.clone(),
            decimals: 36,
        };
        let debit_atoms = decimal_to_atoms(&quantity, debit_asset.decimals)?;
        let fee_atoms = decimal_to_atoms(&max_fee, debit_asset.decimals)?;
        let required = debit_atoms.checked_add(&fee_atoms).map_err(domain_error)?;
        let source_min = context
            .projected
            .cash
            .iter()
            .find(|cash| cash.resource == account.resource && cash.asset == debit_asset)
            .map(|cash| cash.amount.minimum.clone())
            .unwrap_or_else(Quantity::zero);
        if source_min.integer() < required.integer() {
            return Err(error(
                "PLAN_FUNDS_UNAVAILABLE",
                "preflight",
                "Projected Binance SPOT cash cannot cover the exact withdrawal plus maximum fee.",
            ));
        }
        let source_step = context
            .prior_proofs
            .iter()
            .rev()
            .find(|proof| {
                proof.effects.iter().any(|effect| {
                    matches!(effect,
                        ProjectedEffect::CashDelta { resource, asset, maximum_delta, .. }
                        if resource == &account.resource && asset == &debit_asset
                            && !maximum_delta.is_negative() && !maximum_delta.is_zero()
                    )
                })
            })
            .map(|proof| proof.step_index);
        let selected_baseline_atoms = context
            .allocations
            .iter()
            .filter(|holding| {
                holding.resource == account.resource && holding.amount.asset == debit_asset
            })
            .map(|holding| holding.amount.atoms.integer())
            .sum::<num_bigint::BigUint>();
        let baseline_untouched = selected_baseline_atoms >= required.integer()
            && !context.prior_proofs.iter().any(|proof| proof.effects.iter().any(|effect| matches!(effect,
                ProjectedEffect::CashDelta { resource, asset, minimum_delta, .. }
                if resource == &account.resource && asset == &debit_asset && minimum_delta.is_negative()
            )));
        if source_step.is_none() && !baseline_untouched {
            return Err(error(
                "PLAN_SOURCE_UNATTRIBUTED",
                "preflight",
                "Projected exchange cash is not attributable to a single selected baseline or prior transfer.",
            ));
        }
        let amount_atoms = decimal_to_atoms(&quantity, debit_asset.decimals)?;
        let fee_bound = CostBound {
            location: AccountingLocation::Cash {
                resource: account.resource.clone(),
            },
            amount: Amount {
                asset: debit_asset.clone(),
                atoms: fee_atoms,
            },
            treatment: CostTreatment::AdditionalDebit,
        };
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: account.resource.clone(),
                amount: Amount {
                    asset: debit_asset.clone(),
                    atoms: amount_atoms.clone(),
                },
            }],
            maximum_costs: vec![fee_bound],
            expected: vec![EffectConstraint::Transfer {
                from: account.resource.clone(),
                to: destination.resource.clone(),
                debit_asset: debit_asset.clone(),
                credit_asset: asset.clone(),
                maximum_debit: amount_atoms.clone(),
                minimum_credit: decimal_to_atoms(&min_receive, asset.decimals)?,
            }],
            locks: vec![account.resource.clone(), destination.resource.clone()],
        };
        // The API quantity is the transfer principal. The venue network fee
        // is separately bounded by intent.maximum_costs and folded into the
        // projected cash debit by fin-engine exactly once.
        let max_debit = required;
        let signed_min = SignedQuantity::negative(&amount_atoms);
        let signed_max = SignedQuantity::negative(&amount_atoms);
        let now_time = Utc::now();
        Ok(ReturnRouteProof {
            step_index: context.step_index,
            operation: "binance.withdraw".into(),
            input_hash: binding_hash(raw).map_err(domain_error)?,
            observed_at: now_time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            expires_at: (now_time + Duration::seconds(60))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            intent,
            effects: vec![
                ProjectedEffect::CashDelta {
                    resource: account.resource.clone(),
                    asset: debit_asset.clone(),
                    minimum_delta: signed_min,
                    maximum_delta: signed_max,
                },
                ProjectedEffect::CashDelta {
                    resource: destination.resource.clone(),
                    asset: asset.clone(),
                    minimum_delta: SignedQuantity::positive(&decimal_to_atoms(
                        &min_receive,
                        asset.decimals,
                    )?),
                    maximum_delta: SignedQuantity::positive(&decimal_to_atoms(
                        &quantity,
                        asset.decimals,
                    )?),
                },
            ],
            requires: vec![ProjectedRequirement::CashAtLeast {
                resource: account.resource.clone(),
                asset: debit_asset,
                amount: max_debit,
                source_step,
            }],
            evidence: json!({"method":"binance-conditional-eth-withdrawal-v1","account":account.resource,"destination":destination.resource,"address":destination.address,"network":"ETH","publishedFee":fee.canonical(),"maximumFee":max_fee.canonical(),"minimumReceive":min_receive.canonical(),"maximumReceive":quantity.canonical(),"precisionIncrement":increment.canonical(),"travelRule":"NIL","conditional":"Actual execution still needs a fresh ordinary Binance preview, Touch ID, original withdrawOrderId journaling, and canonical Ethereum receipt verification."}),
        })
    }

    async fn reconcile_transfer(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, FinError> {
        let unknown = |evidence: Value| Reconciliation {
            status: Status::Unknown,
            evidence,
            verified: None,
        };
        let data = &submission.data;
        let binding = &data["binding"];
        let main_name = binding["authority"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance authority is invalid.",
            )
        })?;
        let from_name = binding["from"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance source is invalid.",
            )
        })?;
        let to_name = binding["to"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance destination is invalid.",
            )
        })?;
        let coin = binding["coin"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance asset is invalid.",
            )
        })?;
        let requested = amount(binding["amount"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted transfer amount is invalid.",
            )
        })?)?;
        let client_id = submission.reference.as_str();
        let authority = self.require_main(main_name)?;
        let from = self.account(from_name)?;
        let to = self.account(to_name)?;
        if (from.name != authority.name && from.parent.as_deref() != Some(authority.name.as_str()))
            || (to.name != authority.name && to.parent.as_deref() != Some(authority.name.as_str()))
            || from.name == to.name
            || (from.name != authority.name && to.name != authority.name)
            || binding.get("authority").and_then(Value::as_str) != Some(authority.name.as_str())
            || binding.get("clientTranId").and_then(Value::as_str)
                != Some(submission.reference.as_str())
        {
            return Err(error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance transfer route or client identity is invalid.",
            ));
        }
        let params: BinanceRequest =
            serde_json::from_value(binding.get("params").cloned().ok_or_else(|| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted Binance transfer request is missing.",
                )
            })?)
            .map_err(|_| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted Binance transfer request is invalid.",
                )
            })?;
        if params.endpoint != BinanceEndpoint::UniversalTransfer
            || params.validate().is_err()
            || params.get("clientTranId") != Some(submission.reference.as_str())
            || params.get("clientTranId") != binding.get("clientTranId").and_then(Value::as_str)
            || params.get("asset") != Some(coin)
            || params
                .get("amount")
                .and_then(|value| amount(value).ok())
                .is_none_or(|value| value.checked_cmp(&requested).is_ne())
            || params.get("fromEmail") != binding.get("fromEmail").and_then(Value::as_str)
            || params.get("toEmail") != binding.get("toEmail").and_then(Value::as_str)
        {
            return Err(error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance transfer request does not match its immutable route identity.",
            ));
        }
        let (route_parameter, route_email) = if from.name == authority.name {
            ("toEmail", to.email.as_deref())
        } else if to.name == authority.name {
            ("fromEmail", from.email.as_deref())
        } else {
            return Err(error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance transfer route is unsupported.",
            ));
        };
        if route_email.is_none()
            || params.get("fromEmail")
                != (if route_parameter == "fromEmail" {
                    route_email
                } else {
                    None
                })
            || params.get("toEmail")
                != (if route_parameter == "toEmail" {
                    route_email
                } else {
                    None
                })
            || data.get("fromEmail").and_then(Value::as_str) != from.email.as_deref()
            || data.get("toEmail").and_then(Value::as_str) != to.email.as_deref()
        {
            return Err(error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance transfer parameters do not match the configured route.",
            ));
        }
        // New records persist this route selector explicitly. Older durable
        // submissions predate that field, so derive it only from their exact,
        // validated immutable UniversalTransfer request above.
        if let Some(selector) = data.get("historySelector") {
            let selector_object = selector.as_object().ok_or_else(|| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted Binance transfer history selector is invalid.",
                )
            })?;
            if selector_object.len() != 2
                || selector.get("parameter").and_then(Value::as_str) != Some(route_parameter)
                || selector.get("value").and_then(Value::as_str) != route_email
            {
                return Err(error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted Binance transfer history selector does not match its request.",
                ));
            }
        }
        let started =
            DateTime::parse_from_rfc3339(data["submittedAt"].as_str().unwrap_or_default())
                .map_err(|_| {
                    error(
                        "RECONCILIATION_INVALID",
                        "reconciliation",
                        "Persisted submission time is invalid.",
                    )
                })?
                .with_timezone(&Utc);
        if Utc::now() - started > Duration::days(7) || started > Utc::now() + Duration::seconds(5) {
            return Ok(unknown(
                json!({"clientTranId":client_id,"historyComplete":false,"reason":"Binance transfer history is outside the supported reconciliation window; operator review is required."}),
            ));
        }
        let window = HistoryWindow {
            start_time_ms: started.timestamp_millis().max(0) as u64,
            end_time_ms: Utc::now().timestamp_millis().max(0) as u64,
        };
        let mut matches = Vec::<TransferHistoryRow>::new();
        let mut complete = false;
        for page in 1..=20u64 {
            let request = BinanceRequest::new(BinanceEndpoint::UniversalTransferHistory)
                .push("startTime", window.start_time_ms.to_string())
                .push("endTime", window.end_time_ms.to_string())
                .push("clientTranId", client_id)
                .push(
                    route_parameter,
                    route_email.expect("validated child route email"),
                )
                .push("page", page.to_string())
                .push("limit", "500");
            let response = match self.call(authority, request, "reconciliation").await {
                Ok(response) => response,
                Err(_) => {
                    return Ok(unknown(
                        json!({"clientTranId":client_id,"historyComplete":false,"reason":"Transfer history is temporarily unavailable; reconcile this same request identity."}),
                    ));
                }
            };
            let batch: TransferHistoryResponse = match parse_response(response) {
                Ok(batch) => batch,
                Err(_) => {
                    return Ok(unknown(
                        json!({"clientTranId":client_id,"historyComplete":false,"reason":"Binance transfer history response is invalid."}),
                    ));
                }
            };
            let fetched = batch.result.len();
            matches.extend(
                batch
                    .result
                    .into_iter()
                    .filter(|row| row.client_tran_id.as_deref() == Some(client_id)),
            );
            if fetched < 500 || batch.total_count.is_some_and(|total| page * 500 >= total) {
                complete = true;
                break;
            }
        }
        if matches.is_empty() {
            return Ok(unknown(
                json!({"clientTranId":client_id,"historyComplete":complete,"reason":"No matching transfer history row proves whether submission occurred; do not retry."}),
            ));
        }
        if matches.len() != 1 {
            return Ok(unknown(
                json!({"clientTranId":client_id,"reason":"Multiple matching transfer history rows require operator reconciliation."}),
            ));
        }
        let row = matches.remove(0);
        let from_email = from.email.as_deref();
        let to_email = to.email.as_deref();
        let identity_matches = row.client_tran_id.as_deref() == Some(client_id)
            && row.asset == coin
            && row.amount.checked_cmp(&requested).is_eq()
            && row.from_account_type == "SPOT"
            && row.to_account_type == "SPOT"
            && row.from_email.as_deref() == from_email
            && row.to_email.as_deref() == to_email
            && data
                .get("venueId")
                .and_then(Value::as_str)
                .is_none_or(|id| row.tran_id.as_str() == id);
        if !identity_matches {
            return Ok(unknown(
                json!({"clientTranId":client_id,"historyComplete":complete,"reason":"Transfer history identity, account route, amount, wallet type or persisted venue ID mismatch."}),
            ));
        }
        let evidence_base = json!({"clientTranId":client_id,"venueId":row.tran_id.as_str(),"from":from.name,"to":to.name,"asset":coin,"amount":row.amount.canonical(),"venueStatus":row.status,"historyComplete":complete});
        if row.status == "FAILED" || row.status == "FAILURE" {
            let source_before =
                observed_decimal(data["sourceFreeBefore"].as_str().unwrap_or_default())?;
            let dest_before =
                observed_decimal(data["destinationFreeBefore"].as_str().unwrap_or_default())?;
            let source_now = Self::free(&self.spot_balances(from, Some(authority)).await?, coin)
                .cloned()
                .unwrap_or_else(ExactDecimal::zero);
            let dest_now = Self::free(&self.spot_balances(to, Some(authority)).await?, coin)
                .cloned()
                .unwrap_or_else(ExactDecimal::zero);
            if source_now.checked_cmp(&source_before).is_ne()
                || dest_now.checked_cmp(&dest_before).is_ne()
            {
                return Ok(unknown(
                    json!({"transfer":evidence_base,"reason":"Binance reports failure but net balances changed; preserve the original identity for operator reconciliation."}),
                ));
            }
            let effects = vec![];
            let verified = make_verified(submission.clone(), effects, evidence_base.clone())?;
            return Ok(Reconciliation {
                status: Status::Failed,
                evidence: evidence_base,
                verified: Some(verified),
            });
        }
        if row.status != "SUCCESS" {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: evidence_base,
                verified: None,
            });
        }
        let source_before =
            observed_decimal(data["sourceFreeBefore"].as_str().unwrap_or_default())?;
        let dest_before =
            observed_decimal(data["destinationFreeBefore"].as_str().unwrap_or_default())?;
        let source_now = Self::free(&self.spot_balances(from, Some(authority)).await?, coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        let dest_now = Self::free(&self.spot_balances(to, Some(authority)).await?, coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        let debit = decimal_delta(&source_before, &source_now);
        let credit = decimal_delta(&dest_now, &dest_before);
        let (Some(debit), Some(credit)) = (debit, credit) else {
            return Ok(unknown(
                json!({"transfer":evidence_base,"sourceFree":source_now.canonical(),"destinationFree":dest_now.canonical(),"reason":"Balance movements do not correlate with the matching transfer history row."}),
            ));
        };
        if debit.checked_cmp(&requested).is_lt() || credit.checked_cmp(&requested).is_lt() {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"transfer":evidence_base,"sourceFree":source_now.canonical(),"destinationFree":dest_now.canonical(),"sourceDebitObserved":debit.canonical(),"destinationCreditObserved":credit.canonical(),"reason":"Successful venue history exists, but exact source debit and destination credit are not yet observed."}),
                verified: None,
            });
        }
        if debit.checked_cmp(&requested).is_gt() || credit.checked_cmp(&requested).is_gt() {
            return Ok(unknown(
                json!({"transfer":evidence_base,"sourceFree":source_now.canonical(),"destinationFree":dest_now.canonical(),"sourceDebitObserved":debit.canonical(),"destinationCreditObserved":credit.canonical(),"reason":"Net balance changes exceed the matching transfer; concurrent or unexplained activity needs reconciliation."}),
            ));
        }
        let asset: AssetId = serde_json::from_value(binding["asset"].clone()).map_err(|_| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted transfer asset identity is invalid.",
            )
        })?;
        let atoms = decimal_to_atoms(&requested, asset.decimals)?;
        let effects = vec![FinancialEffect::Transfer {
            from: from.resource.clone(),
            to: to.resource.clone(),
            debit: Amount {
                asset: asset.clone(),
                atoms: atoms.clone(),
            },
            credit: Amount { asset, atoms },
        }];
        let evidence = json!({"transfer":evidence_base,"sourceFreeBefore":source_before.canonical(),"sourceFree":source_now.canonical(),"destinationFreeBefore":dest_before.canonical(),"destinationFree":dest_now.canonical(),"sourceDebitObserved":debit.canonical(),"destinationCreditObserved":credit.canonical(),"balanceCorrelation":"matching clientTranId history plus exact net balance changes; unrelated concurrent activity is not attributable"});
        let verified = make_verified(submission.clone(), effects, evidence.clone())?;
        let _ = record;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence,
            verified: Some(verified),
        })
    }

    async fn reconcile_withdrawal(
        &self,
        _record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, FinError> {
        let unknown = |evidence: Value| Reconciliation {
            status: Status::Unknown,
            evidence,
            verified: None,
        };
        let data = &submission.data;
        let binding = &data["binding"];
        let account = self.require_main(binding["account"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance account is invalid.",
            )
        })?)?;
        let destination =
            self.destination(binding["destination"].as_str().ok_or_else(|| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted Ethereum destination is invalid.",
                )
            })?)?;
        let coin = data["coin"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted withdrawal coin is invalid.",
            )
        })?;
        let amount_requested = amount(data["amount"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted withdrawal amount is invalid.",
            )
        })?)?;
        let reference = submission.reference.as_str();
        let started =
            DateTime::parse_from_rfc3339(data["submittedAt"].as_str().unwrap_or_default())
                .map_err(|_| {
                    error(
                        "RECONCILIATION_INVALID",
                        "reconciliation",
                        "Persisted withdrawal time is invalid.",
                    )
                })?
                .with_timezone(&Utc);
        if Utc::now() - started > Duration::days(7) || started > Utc::now() + Duration::seconds(5) {
            return Ok(unknown(
                json!({"withdrawOrderId":reference,"historyComplete":false,"reason":"Binance withdrawal history is outside the supported reconciliation window."}),
            ));
        }
        let start_time = started.timestamp_millis().saturating_sub(60_000).max(0) as u64;
        let end_time = Utc::now().timestamp_millis().max(0) as u64;
        let mut matches = Vec::<FundingHistoryRow>::new();
        let mut complete = false;
        for page in 0..20u64 {
            let request = BinanceRequest::new(BinanceEndpoint::WithdrawalHistory)
                .push("startTime", start_time.to_string())
                .push("endTime", end_time.to_string())
                .push("offset", (page * 1000).to_string())
                .push("limit", "1000")
                .push("coin", coin)
                .push("withdrawOrderId", reference);
            let response = match self.call(account, request.clone(), "reconciliation").await {
                Ok(response) => response,
                Err(_) => {
                    return Ok(unknown(
                        json!({"withdrawOrderId":reference,"historyComplete":false,"reason":"Binance withdrawal history is temporarily unavailable; reconcile this same request identity."}),
                    ));
                }
            };
            let batch: Vec<FundingHistoryRow> = match parse_response::<Vec<FundingHistoryRow>>(
                response,
            ) {
                Ok(batch)
                    if batch
                        .iter()
                        .all(FundingHistoryRow::has_valid_withdrawal_complete_time) =>
                {
                    batch
                }
                Err(_) => {
                    return Ok(unknown(
                        json!({"withdrawOrderId":reference,"historyComplete":false,"reason":"Binance withdrawal history response is invalid."}),
                    ));
                }
                Ok(_) => {
                    return Ok(unknown(
                        json!({"withdrawOrderId":reference,"historyComplete":false,"reason":"Binance withdrawal completeTime has an invalid documented type or UTC format."}),
                    ));
                }
            };
            let fetched = batch.len();
            matches.extend(batch.into_iter().filter(|row| {
                row.withdraw_order_id.as_deref() == Some(reference)
                    || data
                        .get("venueId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| row.id.as_str() == id)
            }));
            if fetched < 1000 {
                complete = true;
                break;
            }
        }
        if matches.is_empty() {
            return Ok(unknown(
                json!({"withdrawOrderId":reference,"historyComplete":complete,"reason":"No matching withdrawal record proves whether the write reached Binance; do not retry."}),
            ));
        }
        if matches.len() != 1 {
            return Ok(unknown(
                json!({"withdrawOrderId":reference,"reason":"Multiple Binance withdrawal records match the persisted identity."}),
            ));
        }
        let row = matches.remove(0);
        let correct = row.coin == coin
            && row.network.as_deref() == Some("ETH")
            && row
                .address
                .as_deref()
                .is_some_and(|address| address.eq_ignore_ascii_case(&destination.address))
            && row.transfer_type == Some(0)
            && row.wallet_type == Some(0)
            && row
                .withdraw_order_id
                .as_deref()
                .is_none_or(|id| id == reference)
            && data
                .get("venueId")
                .and_then(Value::as_str)
                .is_none_or(|id| row.id.as_str() == id);
        if !correct {
            return Ok(unknown(
                json!({"withdrawOrderId":reference,"historyComplete":complete,"reason":"Withdrawal identity, asset, network, address, wallet type or external transfer type mismatch."}),
            ));
        }
        let base_evidence = redact_funding_row(&row);
        let status = row.status;
        let source_before =
            observed_decimal(data["sourceFreeBefore"].as_str().unwrap_or_default())?;
        let current = Self::free(&self.spot_balances(account, None).await?, coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        let debit = decimal_delta(&source_before, &current);
        let Some(debit) = debit else {
            return Ok(unknown(
                json!({"withdrawal":base_evidence,"reason":"Main account free balance increased after withdrawal; the correlated debit cannot be established."}),
            ));
        };
        if matches!(status, 1 | 3 | 5) {
            if debit.is_zero() {
                let evidence = json!({"withdrawal":base_evidence,"historyComplete":complete,"sourceFreeBefore":source_before.canonical(),"sourceFree":current.canonical(),"sourceDebitObserved":"0","venueState":"failed"});
                return Ok(Reconciliation {
                    status: Status::Failed,
                    evidence: evidence.clone(),
                    verified: Some(make_verified(submission.clone(), vec![], evidence)?),
                });
            }
            return Ok(unknown(
                json!({"withdrawal":base_evidence,"sourceDebitObserved":debit.canonical(),"reason":"Binance reports withdrawal failure but a source debit remains; keep it unresolved."}),
            ));
        }
        if status != 6 {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"withdrawal":base_evidence,"historyComplete":complete,"sourceDebitObserved":debit.canonical(),"venueState":"pending"}),
                verified: None,
            });
        }
        let fee = row.transaction_fee.as_ref().ok_or_else(|| {
            error(
                "WITHDRAWAL_FEE_UNKNOWN",
                "reconciliation",
                "Completed Binance withdrawal has no exact transactionFee.",
            )
        })?;
        let max_fee = amount(data["maximumFee"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted maximum fee is invalid.",
            )
        })?)?;
        let lower_bound = subtract_decimal(&amount_requested, &max_fee).ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted fee bound exceeds withdrawal amount.",
            )
        })?;
        let upper_bound = add_decimal(&amount_requested, &max_fee)?;
        if fee.checked_cmp(&max_fee).is_gt()
            || debit.checked_cmp(&lower_bound).is_lt()
            || debit.checked_cmp(&upper_bound).is_gt()
        {
            return Ok(unknown(
                json!({"withdrawal":base_evidence,"actualFee":fee.canonical(),"sourceDebitObserved":debit.canonical(),"reason":"Observed Binance fee or source debit exceeds the approved exact bounds."}),
            ));
        }
        let tx_hash = row.tx_id.as_deref().filter(|hash| {
            hash.len() == 66
                && hash.starts_with("0x")
                && hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
        });
        let Some(tx_hash) = tx_hash else {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"withdrawal":base_evidence,"reason":"Binance reports completion but no valid Ethereum transaction hash is available."}),
                verified: None,
            });
        };
        let asset: AssetId = serde_json::from_value(binding["asset"].clone()).map_err(|_| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Ethereum asset identity is invalid.",
            )
        })?;
        let verifier = self.recipient_verifier.as_ref().ok_or_else(|| {
            error(
                "RECEIPT_VERIFIER_MISSING",
                "reconciliation",
                "Ethereum recipient verifier is unavailable.",
            )
        })?;
        let receipt = verifier
            .verify(PayoutRequest {
                tx_hash: tx_hash.into(),
                chain_id: destination.chain_id,
                recipient: destination.address.clone(),
                asset: asset.clone(),
                minimum_amount: data["minimumReceive"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                maximum_amount: data["maximumReceive"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                confirmations: destination.confirmations,
            })
            .await;
        if receipt.status != Status::Completed {
            let status = if receipt.status == Status::Failed {
                Status::Unknown
            } else {
                receipt.status.clone()
            };
            return Ok(Reconciliation {
                status,
                evidence: json!({"withdrawal":base_evidence,"recipient":receipt.evidence,"reason":"Mainnet receipt is unverified or reverted; retain the original Binance withdrawal identity."}),
                verified: None,
            });
        }
        let received_atoms = receipt.amount_atoms.ok_or_else(|| {
            error(
                "RECEIPT_AMOUNT_UNKNOWN",
                "reconciliation",
                "Completed recipient verification lacks exact received base units.",
            )
        })?;
        let minimum = Quantity::from_decimal(
            data["minimumReceive"].as_str().unwrap_or_default(),
            asset.decimals,
        )
        .map_err(domain_error)?;
        let maximum = Quantity::from_decimal(
            data["maximumReceive"].as_str().unwrap_or_default(),
            asset.decimals,
        )
        .map_err(domain_error)?;
        if received_atoms < minimum || received_atoms > maximum {
            return Ok(unknown(
                json!({"withdrawal":base_evidence,"recipient":receipt.evidence,"reason":"Ethereum recipient amount falls outside approved receipt bounds."}),
            ));
        }
        let debit_asset: AssetId =
            serde_json::from_value(binding["debitAsset"].clone()).map_err(|_| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted Binance debit asset identity is invalid.",
                )
            })?;
        let fee_atoms = decimal_to_atoms(fee, debit_asset.decimals)?;
        let transfer_debit = subtract_decimal(&debit, fee).ok_or_else(|| {
            error(
                "WITHDRAWAL_ACCOUNTING_UNKNOWN",
                "reconciliation",
                "Observed transaction fee exceeds total source debit.",
            )
        })?;
        let received_decimal = ExactDecimal::parse(&received_atoms.to_decimal(asset.decimals))
            .map_err(domain_error)?;
        if transfer_debit.checked_cmp(&received_decimal).is_ne() {
            return Ok(unknown(
                json!({"withdrawal":base_evidence,"recipient":receipt.evidence,"sourceDebitObserved":debit.canonical(),"actualFee":fee.canonical(),"reason":"Binance debit, reported fee, and verified recipient receipt do not conserve exactly."}),
            ));
        }
        let transfer_debit_atoms = decimal_to_atoms(&transfer_debit, debit_asset.decimals)?;
        let amount_debit = Amount {
            asset: debit_asset.clone(),
            atoms: transfer_debit_atoms.clone(),
        };
        let amount_credit = Amount {
            asset: asset.clone(),
            atoms: received_atoms.clone(),
        };
        let mut effects = vec![FinancialEffect::Transfer {
            from: account.resource.clone(),
            to: destination.resource.clone(),
            debit: amount_debit,
            credit: amount_credit,
        }];
        if !fee_atoms.is_zero() {
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: account.resource.clone(),
                },
                amount: Amount {
                    asset: debit_asset,
                    atoms: fee_atoms,
                },
                category: "binance_withdrawal_fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            });
        }
        let evidence = json!({"withdrawal":base_evidence,"historyComplete":complete,"withdrawOrderId":reference,"sourceFreeBefore":source_before.canonical(),"sourceFree":current.canonical(),"sourceDebitObserved":debit.canonical(),"actualFee":fee.canonical(),"recipient":receipt.evidence,"balanceCorrelation":"matching withdrawal identity plus venue fee and canonical Ethereum recipient receipt"});
        let verified = make_verified(submission.clone(), effects, evidence.clone())?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence,
            verified: Some(verified),
        })
    }
}

/// Compare the adapter-owned valuation fields that were approved while
/// allowing the engine to append trusted cost-origin price rows. Observation
/// timestamps are freshness metadata, not a changed financial amount; exact
/// unit, price and provider source remain part of the approved binding.
fn approved_native_valuation_matches(
    approved: &PreparedOperation,
    refreshed: &PreparedOperation,
) -> bool {
    let (Some(approved_value), Some(refreshed_value)) =
        (approved.valuation.as_ref(), refreshed.valuation.as_ref())
    else {
        return approved.valuation.is_none() && refreshed.valuation.is_none();
    };
    if approved_value.movement_usd_micros != refreshed_value.movement_usd_micros
        || approved_value.maximum_fee_usd_micros != refreshed_value.maximum_fee_usd_micros
        || approved_value.additional_allocation_usd_micros
            != refreshed_value.additional_allocation_usd_micros
    {
        return false;
    }

    let mut adapter_assets = BTreeSet::new();
    if let Some(coin) = approved.input.get("coin").and_then(Value::as_str) {
        adapter_assets.insert(AssetId {
            network: "binance".into(),
            reference: coin.into(),
            decimals: 36,
        });
    }
    if approved.operation == "binance.withdraw" {
        for constraint in &approved.intent.expected {
            if let EffectConstraint::Transfer { credit_asset, .. } = constraint {
                adapter_assets.insert(credit_asset.clone());
            }
        }
    }
    if adapter_assets.is_empty() {
        return false;
    }

    adapter_assets.iter().all(|asset| {
        let approved_rows = approved_value
            .prices
            .iter()
            .filter(|row| row.asset == *asset)
            .collect::<Vec<_>>();
        let refreshed_rows = refreshed_value
            .prices
            .iter()
            .filter(|row| row.asset == *asset)
            .collect::<Vec<_>>();
        matches!((approved_rows.as_slice(), refreshed_rows.as_slice()),
            ([before], [after])
                if before.usd_micros_per_unit == after.usd_micros_per_unit
                    && before.unit_atoms == after.unit_atoms
                    && before.source == after.source)
    })
}

fn decimal_delta(before: &ExactDecimal, after: &ExactDecimal) -> Option<ExactDecimal> {
    if before.checked_cmp(after).is_lt() {
        None
    } else {
        subtract_decimal(before, after)
    }
}

fn subtract_decimal(left: &ExactDecimal, right: &ExactDecimal) -> Option<ExactDecimal> {
    let scale = left.scale().max(right.scale());
    let l = left.coefficient_for_scale(scale);
    let r = right.coefficient_for_scale(scale);
    if l < r {
        None
    } else {
        ExactDecimal::from_coefficient(l - r, scale).ok()
    }
}

fn make_verified(
    submission: Submission,
    effects: Vec<FinancialEffect>,
    evidence: Value,
) -> Result<VerifiedEffects, FinError> {
    let mut bundle = VerifiedEffects {
        evidence_version: 1,
        submission,
        effects,
        evidence,
        observed_at: now(),
        proof_digest: String::new(),
    };
    bundle.proof_digest = fin_domain::verified_effect_digest(&bundle)?;
    Ok(bundle)
}
