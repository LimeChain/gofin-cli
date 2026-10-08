use super::*;
mod address_book;
mod boundaries;
mod spot;
mod trading;
mod transfer_rejection;
mod withdrawal;
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
use fin_exchange::{ExchangeAdapter, ExchangeProfile, PlannedSubmission};
use num_bigint::BigUint;
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
pub use withdrawal::binance_evm_chain_id;

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
    /// Optional action-scoped warmup; exact valuation still validates each quote.
    async fn prefetch_usd(&self, _coins: &[String]) -> Result<(), FinError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PayoutRequest {
    pub tx_hash: String,
    pub network: String,
    pub memo: Option<String>,
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
    fn supports_chain(&self, _chain_id: u64) -> bool {
        true
    }
    async fn resolve_asset(
        &self,
        _chain_id: u64,
        _coin: &str,
        _contract: Option<&str>,
    ) -> Result<Option<AssetId>, FinError> {
        Ok(None)
    }
    /// Network observers can extend this without changing Binance submission.
    fn supports_asset(&self, _network: &str, asset: &AssetId) -> bool {
        asset
            .network
            .strip_prefix("ethereum:")
            .and_then(|chain| chain.parse::<u64>().ok())
            .is_some_and(|chain| self.supports_chain(chain))
    }
    async fn resolve_route_asset(
        &self,
        network: &str,
        coin: &str,
        contract: Option<&str>,
    ) -> Result<Option<AssetId>, FinError> {
        if let Some(chain) = binance_evm_chain_id(network) {
            self.resolve_asset(chain, coin, contract).await
        } else {
            Ok(None)
        }
    }
    async fn verify(&self, request: PayoutRequest) -> PayoutResult;
}

fn transport_error(stage: &str, error_value: TransportError) -> FinError {
    match error_value {
        TransportError::Unavailable => {
            error("VENUE_UNAVAILABLE", stage, "Binance could not be reached.")
        }
        TransportError::RateLimited => error(
            "VENUE_RATE_LIMIT",
            stage,
            "Binance rate limited the request.",
        ),
        TransportError::PermissionDenied => error(
            "VENUE_PERMISSION",
            stage,
            "The selected Binance credential was denied.",
        ),
        TransportError::InvalidResponse => error(
            "VENUE_RESPONSE_INVALID",
            stage,
            "Binance returned an invalid response; reconcile any persisted submission before retrying.",
        ),
        TransportError::Timeout => error(
            "VENUE_TIMEOUT",
            stage,
            "Binance timed out; reconcile any persisted submission before retrying.",
        ),
        TransportError::Rejected => error(
            "VENUE_REQUEST_FAILED",
            stage,
            "Binance rejected the request; reconcile any persisted submission before retrying.",
        ),
        TransportError::HttpFailure {
            status,
            binance_code,
            kind,
        } => http_failure_error(stage, status, binance_code, kind),
    }
}

/// Backwards-compatible constructor around the shared exchange lifecycle.
#[derive(Clone)]
pub struct BinanceAdapter(ExchangeAdapter<BinanceProfile>);

impl BinanceAdapter {
    pub fn new(
        config: BinanceConfig,
        transport: Arc<dyn BinanceTransport>,
        price_provider: Option<Arc<dyn BinancePriceProvider>>,
        recipient_verifier: Option<Arc<dyn BinanceRecipientVerifier>>,
    ) -> Result<Self, FinError> {
        Ok(Self(ExchangeAdapter::new(BinanceProfile::new(
            config,
            transport,
            price_provider,
            recipient_verifier,
        )?)))
    }

    pub async fn preview_withdrawal_after_funding(
        &self,
        withdrawal: &Value,
        sources: Vec<ConditionalFundingSource>,
    ) -> Result<PreparedOperation, FinError> {
        self.0
            .profile()
            .preview_withdrawal_after_funding(withdrawal, sources)
            .await
    }
}

#[async_trait]
impl Adapter for BinanceAdapter {
    fn definitions(&self) -> Vec<Definition> {
        self.0.definitions()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.0.connections()
    }
    fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<fin_domain::boundaries::BoundarySpec, FinError> {
        self.0.prepare_boundaries(prepared, bounds)
    }
    fn validate_boundary_candidate(
        &self,
        spec: &fin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<(), FinError> {
        self.0.validate_boundary_candidate(spec, prepared)
    }
    async fn snapshot(&self) -> Result<StateSnapshot, FinError> {
        self.0.snapshot().await
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value, FinError> {
        self.0.read(operation, input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation, FinError> {
        self.0.prepare(operation, input).await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &fin_domain::PreparationContext,
    ) -> Result<PreparedOperation, FinError> {
        self.0.prepare_with_context(operation, input, context).await
    }
    async fn preflight_return_route(
        &self,
        operation: &str,
        input: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, FinError> {
        self.0
            .preflight_return_route(operation, input, context)
            .await
    }
    async fn revalidate(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, FinError> {
        self.0.revalidate(prepared).await
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission, FinError> {
        self.0.submit(prepared, journal).await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation, FinError> {
        self.0.reconcile(record).await
    }
}

/// Venue request mapping only: dispatch timing and journaling belong to fin-exchange.
pub struct BinanceDispatch {
    authority: String,
    request: BinanceRequest,
    noop: Option<Value>,
    receipt_submission: Option<Submission>,
}

#[derive(Clone)]
pub struct BinanceProfile {
    config: Arc<BinanceConfig>,
    transport: Arc<dyn BinanceTransport>,
    price_provider: Option<Arc<dyn BinancePriceProvider>>,
    recipient_verifier: Option<Arc<dyn BinanceRecipientVerifier>>,
}

impl BinanceProfile {
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
                    if account.parent.is_some() != account.email.is_some()
                        || account.email.as_deref().is_some_and(str::is_empty) =>
                {
                    return Err(error(
                        "INVALID_HIERARCHY",
                        "setup",
                        "An optional subaccount parent and Binance account email must be configured together.",
                    ));
                }
                _ => {}
            }
        }
        for account in &config.accounts {
            if let AccountRole::Sub = account.role
                && let Some(parent_name) = &account.parent
            {
                let parent = config
                    .accounts
                    .iter()
                    .find(|candidate| candidate.name == *parent_name);
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
            if dest.chain_id == 0
                || !valid_eth_address(&dest.address)
                || dest.confirmations == 0
                || !destination_names.insert((dest.name.clone(), dest.chain_id))
            {
                return Err(error(
                    "INVALID_DESTINATION",
                    "setup",
                    "EVM destinations require a unique name/chain, valid address, and positive confirmation count.",
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
            .find(|connection| connection.name == name && connection.chain_id == 1)
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
        let endpoint = request.endpoint;
        self.transport
            .request(&account.credential, request)
            .await
            .map_err(|cause| {
                let permission_denied = matches!(
                    &cause,
                    TransportError::PermissionDenied
                        | TransportError::HttpFailure {
                            kind: HttpFailureKind::PermissionDenied,
                            ..
                        }
                );
                let mut mapped = transport_error(stage, cause);
                if permission_denied
                    && matches!(account.role, AccountRole::Sub)
                    && matches!(
                        endpoint,
                        BinanceEndpoint::CapitalConfig
                            | BinanceEndpoint::DepositAddress
                            | BinanceEndpoint::DepositAddressList
                            | BinanceEndpoint::DepositHistory
                    )
                {
                    let path = endpoint.method_path().1;
                    mapped.message = format!(
                        "Binance denied this subaccount's own-account read at {path}. Check this key's read restrictions and the selected account's eligibility for this specific endpoint; this request used the selected account's credential, and master access is not universally required."
                    );
                    mapped.next_action = "Check the selected subaccount key's reported read permission and ask Binance about this endpoint-specific account restriction; retry the same read after resolving it.".into();
                    let previous = mapped.details.take().map(|details| *details);
                    mapped.details = Some(Box::new(json!({
                        "endpoint": path,
                        "accountRole": "sub",
                        "parentCredentialUsed": false,
                        "transport": previous,
                    })));
                }
                mapped
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

    async fn verify_child_transfer_history_access(
        &self,
        child: &BinanceAccount,
        authority: &BinanceAccount,
        asset: &str,
        transfer_type: u64,
    ) -> Result<(), FinError> {
        // The child credential is part of the proof path when the main
        // account's email is not enrolled. Establish both its configured
        // parent/email and immutable physical UID before trusting its history.
        self.verify_subaccount(child, authority).await?;
        self.account_identity(child).await?;
        let end = Utc::now().timestamp_millis().max(0) as u64;
        let start = end.saturating_sub(60_000);
        let request = subaccount_transfer_history_request(asset, transfer_type, start, end, 1)
            .map_err(domain_error)?;
        let response = self.call(child, request, "preview").await?;
        let _: SubAccountTransferHistoryResponse = parse_response(response).map_err(|_| {
            error(
                "VENUE_RESPONSE_INVALID",
                "preview",
                "The selected child credential cannot provide typed transfer history.",
            )
        })?;
        Ok(())
    }

    async fn child_transfer_history_confirms(
        &self,
        child: &BinanceAccount,
        authority: &BinanceAccount,
        row: &TransferHistoryRow,
        asset: &str,
        submitted_at: DateTime<Utc>,
        end_time_ms: u64,
    ) -> Result<Option<Value>, FinError> {
        let child_is_source = row.from_email.as_deref() == child.email.as_deref();
        let transfer_type = if child_is_source { 2 } else { 1 };
        let peer_email = if child_is_source {
            row.to_email.as_deref()
        } else {
            row.from_email.as_deref()
        };
        if child.email.is_none()
            || peer_email.is_none()
            || (!child_is_source && row.to_email.as_deref() != child.email.as_deref())
        {
            return Ok(None);
        }
        self.verify_subaccount(child, authority).await?;
        let (uid, account_type) = self.account_identity(child).await?;
        let start = submitted_at.timestamp_millis().max(0) as u64;
        let request =
            subaccount_transfer_history_request(asset, transfer_type, start, end_time_ms, 200)
                .map_err(domain_error)?;
        let response = self.call(child, request, "reconciliation").await?;
        let history: SubAccountTransferHistoryResponse =
            parse_response(response).map_err(|_| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "reconciliation",
                    "The selected child credential returned invalid transfer history.",
                )
            })?;
        // The endpoint has no offset/page parameter. A full page cannot prove
        // uniqueness because older matching rows may have been truncated.
        if history.len() >= 200 {
            return Ok(None);
        }
        let matches = history
            .iter()
            .filter(|item| item.tran_id.as_str() == row.tran_id.as_str())
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Ok(None);
        }
        let child_row = matches[0];
        if child_row.counter_party != "master"
            || child_row.email != peer_email.unwrap_or_default()
            || child_row.transfer_type != transfer_type
            || child_row.asset != row.asset
            || !child_row.quantity.checked_cmp(&row.amount).is_eq()
            || child_row.from_account_type != row.from_account_type
            || child_row.to_account_type != row.to_account_type
            || child_row.status != row.status
            || child_row.timestamp != row.timestamp
            || child_row.timestamp < start
            || child_row.timestamp > end_time_ms
            || account_type != "SPOT"
        {
            return Ok(None);
        }
        Ok(Some(json!({
            "childResource": child.resource,
            "authenticatedUid": uid,
            "accountType": account_type,
            "parentEmailVerified": true,
            "counterParty": child_row.counter_party,
            "peerEmailMatchedMasterHistory": true,
            "directionType": child_row.transfer_type,
            "venueId": child_row.tran_id.as_str(),
            "asset": child_row.asset,
            "quantity": child_row.quantity.canonical(),
            "fromAccountType": child_row.from_account_type,
            "toAccountType": child_row.to_account_type,
            "status": child_row.status,
            "timestamp": child_row.timestamp,
            "timestampWithinSubmissionWindow": true
        })))
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
        let identity = AssetId {
            network: "binance".into(),
            reference: coin.into(),
            decimals: 36,
        };
        if (price.is_zero() && !fin_domain::unpriced::is_source(&identity, &quote.source))
            || quote.source.is_empty()
        {
            return Err(error(
                "VALUATION_INVALID",
                "preview",
                "USD price source and positive price are required.",
            ));
        }
        if !fin_domain::price_freshness::is_fresh(&quote.observed_at, &quote.source, Utc::now()) {
            return Err(error(
                "STALE_VALUATION",
                "preview",
                "USD price evidence has expired; spot and conversion legs use separate freshness bounds.",
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

    /// Resolve either the historical main-authorized route or the Binance
    /// `subToMaster` route. The latter is intentionally narrow: the selected
    /// authority is the source child itself and the destination is its one
    /// configured parent.
    fn transfer_accounts(
        &self,
        input: &TransferInput,
    ) -> Result<
        (
            &BinanceAccount,
            &BinanceAccount,
            &BinanceAccount,
            &BinanceAccount,
            bool,
        ),
        FinError,
    > {
        let selected = self.account(&input.authority)?;
        if matches!(selected.role, AccountRole::Main) {
            let (main, from, to) = self.validate_transfer_route(input)?;
            return Ok((main, from, to, main, false));
        }
        let from = self.account(&input.from)?;
        let to = self.account(&input.to)?;
        if from.name != selected.name || !matches!(from.role, AccountRole::Sub) {
            return Err(error(
                "WRONG_AUTHORITY",
                "validation",
                "Subaccount authority is supported only when it is the transfer source.",
            ));
        }
        let parent_name = from.parent.as_deref().ok_or_else(|| {
            error(
                "INVALID_HIERARCHY",
                "validation",
                "Source subaccount has no configured parent.",
            )
        })?;
        let main = self.require_main(parent_name)?;
        if to.name != main.name {
            return Err(error(
                "UNSUPPORTED_ROUTE",
                "validation",
                "A subaccount credential can authorize only a SPOT transfer to its configured master.",
            ));
        }
        if from.name == to.name {
            return Err(error(
                "SAME_ACCOUNT",
                "validation",
                "Transfer source and destination must differ.",
            ));
        }
        Ok((main, from, to, selected, true))
    }

    async fn transfer_route(
        &self,
        input: &TransferInput,
    ) -> Result<(BinanceRequest, String, String, String, bool), FinError> {
        let (authority, from, to, signer, sub_to_master) = self.transfer_accounts(input)?;
        if input.from_wallet != "SPOT" || input.to_wallet != "SPOT" {
            return Err(error(
                "UNSUPPORTED_WALLET",
                "validation",
                "Only Binance SPOT wallet transfers are implemented.",
            ));
        }
        let (child, child_transfer_type) = if from.name == authority.name {
            (to, 1)
        } else {
            (from, 2)
        };
        self.verify_child_transfer_history_access(
            child,
            authority,
            &input.coin,
            child_transfer_type,
        )
        .await?;
        let permissions = self.permissions(signer).await?;
        if (!sub_to_master
            && (permissions.enable_internal_transfer != Some(true)
                || permissions.permits_universal_transfer != Some(true)))
            || (sub_to_master && permissions.enable_spot_and_margin_trading != Some(true))
        {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                if sub_to_master {
                    "Source subaccount credential must report SPOT and margin trading permission."
                } else {
                    "Main credential must report internal-transfer and universal-transfer permissions."
                },
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
            amount: amount.clone(),
            from_email: from.email.clone().filter(|_| from.name != authority.name),
            to_email: to.email.clone().filter(|_| to.name != authority.name),
            client_id: ClientIdentity::transfer(&input.request_id).map_err(domain_error)?,
        };
        let request = if sub_to_master {
            SubToMasterTransferIntent {
                asset: input.coin.clone(),
                amount: amount.clone(),
            }
            .request()
            .map_err(domain_error)?
            .push("recvWindow", "5000")
        } else {
            intent.request().map_err(domain_error)?
        };
        let source_free = Self::free(&source_balances, &input.coin)
            .expect("checked balance")
            .canonical();
        let destination_balances = self.spot_balances(to, Some(authority)).await?;
        let destination_free = Self::free(&destination_balances, &input.coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero)
            .canonical();
        Ok((
            request,
            source_free,
            destination_free,
            signer.name.clone(),
            sub_to_master,
        ))
    }

    async fn prepare_transfer(&self, input: TransferInput) -> Result<PreparedOperation, FinError> {
        let (authority, from, to, signer, sub_to_master) = self.transfer_accounts(&input)?;
        let (request, _, _, _, _) = self.transfer_route(&input).await?;
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
        let binding = if sub_to_master {
            json!({
                "authority": signer.name, "parentAuthority": authority.name, "from": from.name, "to": to.name,
                "coin": input.coin, "amount": value.canonical(), "fromWallet": "SPOT", "toWallet": "SPOT",
                "localSubmissionId": ClientIdentity::transfer(&input.request_id).map_err(domain_error)?.as_str(),
                "route": "sub_to_master", "signer": signer.name,
                "asset": AssetId { network: "binance".into(), reference: input.coin.clone(), decimals: venue_decimals },
                "fromEmail": from.email, "toEmail": to.email, "params": request,
            })
        } else {
            json!({
                "authority": authority.name, "from": from.name, "to": to.name,
                "coin": input.coin, "amount": value.canonical(), "fromWallet": "SPOT", "toWallet": "SPOT",
                "clientTranId": request.get("clientTranId").unwrap_or_default(),
                "asset": AssetId { network: "binance".into(), reference: input.coin.clone(), decimals: venue_decimals },
                "fromEmail": request.get("fromEmail"), "toEmail": request.get("toEmail"),
                "params": request,
            })
        };
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
        let summary = if sub_to_master {
            format!(
                "Transfer {} {} from {} SPOT to {} SPOT using source subaccount authority; internal transfer fee 0.",
                value.canonical(),
                input.coin,
                from.name,
                to.name
            )
        } else {
            format!(
                "Transfer {} {} from {} SPOT to {} SPOT using main authority {}; internal transfer fee 0.",
                value.canonical(),
                input.coin,
                from.name,
                to.name,
                authority.name
            )
        };
        Ok(prepared(
            "binance.transfer",
            serde_json::to_value(&input).unwrap_or(Value::Null),
            binding,
            summary,
            vec![
                Self::check(
                    "hierarchy",
                    CheckStatus::Passed,
                    if sub_to_master {
                        "Configured source child identity and unfrozen status were observed under its selected parent."
                    } else {
                        "Configured child identities and unfrozen status were observed under the selected main authority."
                    },
                ),
                Self::check(
                    "permission",
                    CheckStatus::Passed,
                    if sub_to_master {
                        "SPOT and margin trading permission was observed on the selected source child credential."
                    } else {
                        "Internal-transfer and universal-transfer permission were observed on the selected authority."
                    },
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
        let account = self.account(&input.account)?;
        self.validate_withdrawal_selector(&input)?;
        if input.wallet_type != "SPOT" {
            return Err(error(
                "UNSUPPORTED_WALLET",
                "preview",
                "Select the Binance SPOT wallet.",
            ));
        }
        let permissions = self.permissions(account).await?;
        if permissions.enable_withdrawals != Some(true) {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "Selected credential must report withdrawal permission.",
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
        let network = self.network(&currency, &input.network)?;
        let destination = self.withdrawal_destination(&input, network).await?;
        if currency.withdraw_all_enable != Some(true)
            || network.withdraw_enable != Some(true)
            || network.busy == Some(true)
        {
            return Err(error(
                "WITHDRAWAL_DISABLED",
                "preview",
                "Selected Binance withdrawal route is disabled or busy.",
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
        let max_fee = observed_decimal(&input.max_fee)?;
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
                "ASSET_IDENTITY_MISMATCH",
                "preview",
                "Destination asset identity is unavailable.",
            )
        })?;
        let receipt_available = self.receipt_available(&destination, &input.coin, &input.network);
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
        let client_id = ClientIdentity::withdrawal(&input.request_id).map_err(domain_error)?;
        let mut prepared_request = StandardWithdrawalIntent {
            coin: input.coin.clone(),
            network: input.network.clone(),
            memo: input.memo.clone().filter(|m| !m.is_empty()),
            amount: quantity.clone(),
            address: destination.address.clone(),
            client_id: client_id.clone(),
        }
        .request()
        .map_err(domain_error)?;
        prepared_request
            .params
            .iter_mut()
            .find(|param| param.key == "network")
            .unwrap()
            .value = input.network.clone();
        prepared_request.validate().map_err(domain_error)?;
        let mut valuation = self
            .valuation(&input.coin, &quantity.canonical(), &max_fee.canonical())
            .await?;
        if let Some(valuation) = valuation.as_mut()
            && let Some(exchange_price) = valuation.prices.first().cloned()
            && !fin_domain::unpriced::is_estimate(&exchange_price)
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
            "account": account.name, "destination": destination.name, "chainId": destination.chain_id,
            "asset": asset, "debitAsset": debit_asset, "params": prepared_request, "maximumFee": max_fee.canonical(),
            "maximumGrossDebit": gross.canonical(), "minimumReceive": min_receive.canonical(),
            "maximumReceive": quantity.canonical(), "confirmations": destination.confirmations,
            "observedFree": free.canonical(), "publishedFee": fee.canonical(),
            "withdrawOrderId": client_id.as_str(), "network": input.network,
            "memo": input.memo, "receiptResource": destination.resource, "receiptAddress": destination.address,
            "receiptMode": if receipt_available { "chain" } else { "unverified" }
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
                "Coin/network metadata and the exact destination were checked.",
            ),
            Self::check(
                "precision_balance_limits",
                CheckStatus::Passed,
                "Exact Binance funding increment, available free balance, and advertised limits were checked.",
            ),
            Self::check(
                "recipient_receipt",
                CheckStatus::Unchecked,
                if receipt_available {
                    "Destination receipt will be independently verified after submission."
                } else {
                    "Binance submission is supported; independent destination receipt verification is unavailable for this route. Venue completion cannot settle the financial record."
                },
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
        if !receipt_available {
            warnings.push("Destination receipt verification is unavailable. This withdrawal remains unresolved after venue completion; a tracked restoration run requires independently observable holdings and a return route.".into());
        }
        Ok(prepared(
            "binance.withdraw",
            serde_json::to_value(&input).unwrap_or(Value::Null),
            binding,
            format!(
                "Withdraw API quantity {} {} from {} SPOT to {} ({}) on {}; published fee {}, approved maximum fee {}, maximum gross debit {}, minimum net receipt {}.",
                quantity.canonical(),
                input.coin,
                account.name,
                destination.name,
                destination.address,
                input.network,
                fee.canonical(),
                max_fee.canonical(),
                gross.canonical(),
                min_receive.canonical()
            ) + &input
                .memo
                .as_deref()
                .filter(|m| !m.is_empty())
                .map(|memo| format!(" Memo/tag: {memo}."))
                .unwrap_or_default(),
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
    /// Use a configured main connection for existing universal transfers; for
    /// child-to-parent, select the source child to sign Binance subToMaster.
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    destination: String,
    /// Public destination address; use either this or a named destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    /// Exact secondary address identifier where Binance requires one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memo: Option<String>,
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
            "Current Binance asset/network metadata and deposit availability for the selected account using its own credential, including subaccount-only connections.",
            OperationKind::Read,
            schema_for!(NetworksInput),
            "Current funding metadata observed.",
        ),
        (
            "binance.deposit-address",
            "Selected account's own Binance deposit address and memo/tag, read with that account's credential. Subaccount deposit-address restrictions, if any, apply to this endpoint specifically.",
            OperationKind::Read,
            schema_for!(DepositAddressInput),
            "Address observed; credit requires a matching Binance deposit record.",
        ),
        (
            "binance.deposit-history",
            "Paginated own-account Binance deposit history with pending, locked, and usable credit kept distinct, read using the selected account's credential including a subaccount-only connection.",
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
            "binance.spot-order",
            "Bounded LIMIT FOK spot BUY or SELL using existing holdings; purchased assets may be retained. Exact quantity, limit price, maximum receipt-asset fee and minimum net receipt are approved.",
            OperationKind::Write,
            schema_for!(spot::SpotInput),
            "Matching terminal order plus complete authenticated fills and commissions establish the conversion; submission alone does not.",
        ),
        (
            "binance.transfer",
            "SPOT transfer between a configured main account and one configured subaccount. Main authority uses universalTransfer with clientTranId; a source-child authority can return SPOT to its configured parent through subToMaster, whose receipt is persisted by Fin.",
            OperationKind::Write,
            schema_for!(TransferInput),
            "One matching successful history row plus observed source debit and destination credit.",
        ),
        (
            "binance.withdraw",
            "Binance SPOT withdrawal on any advertised coin/network to a named wallet or explicit public address, with exact memo/tag, limits and fees.",
            OperationKind::Write,
            schema_for!(WithdrawInput),
            "Matching venue withdrawal plus bounded source debit/fee and independently verified destination receipt. Unobserved routes remain pending after venue completion.",
        ),
    ];
    let mut result: Vec<_> = defs.into_iter().map(|(name, description, kind, input, completion)| Definition {
        name: name.into(), description: description.into(), kind,
        execution_actor: ExecutionActor::AgentInvocable,
        input_schema: serde_json::to_value(input).unwrap_or(Value::Null),
        output_schema: json!({"type":"object"}),
        error_schema: schema_for!(FinError).into(),
        examples: if name == "binance.spot-order" { vec![json!({"requestId":"buy-eth","account":"binance-sub","symbol":"ETHUSDC","side":"BUY","quantity":"0.01","limitPrice":"2000","maxFee":"0.00002","minReceiveAmount":"0.00998"})] } else { vec![] }, coverage: "Endpoint fixtures and synthetic transport; production account permissions and live financial acceptance remain unverified.".into(),
        completion: completion.into(), semantics: json!({"walletType":"SPOT", "amount":"exact decimal string", "credentialValues":"never exposed"}),
    }).collect();
    for definition in &mut result {
        if matches!(
            definition.name.as_str(),
            "binance.networks" | "binance.deposit-address" | "binance.deposit-history"
        ) {
            definition.coverage = "Own-account Binance USER_DATA read uses the selected connection's credential, including a subaccount-only connection; no parent/master lookup is made. A Binance denial applies to this endpoint/capability and should be reported specifically. Synthetic tests; live account eligibility remains account-dependent.".into();
        }
    }
    if let Some(definition) = result.iter_mut().find(|d| d.name == "binance.transfer") {
        definition.semantics["authority"] = json!(
            "For main→child or legacy main-authorized child→main, select the main account. For source-child→parent only, select the child as authority; Fin uses subToMaster and sends no clientTranId or recipient parameter."
        );
        definition.semantics["subToMasterReceipt"] = json!(
            "Fin persists Binance txnId against the immutable local submission before returning. If the response is lost before the receipt is persisted, the result remains unknown and history is not inferred from amount or time."
        );
        definition.examples = vec![
            json!({"requestId":"return-eth-to-master","authority":"binance-child","from":"binance-child","to":"binance-main","coin":"ETH","amount":"0.01","fromWallet":"SPOT","toWallet":"SPOT"}),
            json!({"requestId":"fund-child","authority":"binance-main","from":"binance-main","to":"binance-child","coin":"ETH","amount":"0.01","fromWallet":"SPOT","toWallet":"SPOT"}),
        ];
    }
    if let Some(definition) = result.iter_mut().find(|d| d.name == "binance.withdraw") {
        definition.input_schema["oneOf"] = json!([
            {"required":["destination"],"properties":{"destination":{"type":"string","minLength":1}},"not":{"required":["address"]}},
            {"required":["address"],"properties":{"destination":{"const":""}}}
        ]);
        definition.input_schema["properties"]["destination"]["minLength"] = json!(0);
        definition.input_schema["properties"]["address"] =
            json!({"type":"string","minLength":1,"maxLength":256});
        definition.input_schema["properties"]["memo"] = json!({"type":"string","maxLength":256});
        definition.semantics["coinNetworkSelection"] =
            json!("live Binance metadata; no coin/network allowlist");
        definition.semantics["destinationReceipt"] = json!(
            "independent observer required for terminal financial settlement; otherwise explicitly pending. The configured Arbitrum One and Robinhood Chain observers accept successful canonical sequencer inclusion with exact recipient and amount proof, without waiting for parent-chain finality. Venue route availability remains separate."
        );
        definition.semantics["ordinaryExecution"] = json!(
            "no run or restoration plan required; exact bounds, fresh approval, reservations and independent receipt checks remain enforced"
        );
        definition.examples = vec![
            json!({"requestId":"example-arbitrum-withdraw","account":"master","destination":"wallet","coin":"ETH","amount":"0.001","network":"ARBITRUM","walletType":"SPOT","maxFee":"0.00005","minReceiveAmount":"0.00095"}),
            json!({"requestId":"example-tagged-withdraw","account":"master","address":"rEXAMPLE_SYNTHETIC_ADDRESS","memo":"000123","coin":"XRP","amount":"10","network":"XRP","walletType":"SPOT","maxFee":"0.2","minReceiveAmount":"9.8"}),
        ];
    }
    result.extend(crate::trading_reads::trading_read_definitions());
    result.extend(trading::definitions());
    result.extend(address_book::definitions());
    result
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
    let mut result = PreparedOperation {
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
    };
    let unpriced = result
        .valuation
        .as_ref()
        .map(|value| {
            value
                .prices
                .iter()
                .filter(|price| fin_domain::unpriced::is_estimate(price))
                .map(|price| price.asset.reference.as_str())
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    if !unpriced.is_empty() {
        let names = unpriced.into_iter().collect::<Vec<_>>().join(", ");
        result
            .warnings
            .push(format!("{names}: {}", fin_domain::unpriced::WARNING));
        result.summary.push_str(&format!(
            " Unpriced {names}: $0 estimate, excluded from USD limits."
        ));
    }
    fin_domain::value_bounds::bind(&mut result);
    result
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
    error.with_details(json!({
        "transport": "binance-ccxt",
        "http_status": status,
        "binance_code": binance_code,
    }))
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
impl ExchangeProfile for BinanceProfile {
    fn prepare_boundaries(
        &self,
        prepared: &PreparedOperation,
        bounds: &Value,
    ) -> Result<fin_domain::boundaries::BoundarySpec, FinError> {
        boundaries::prepare(prepared, bounds)
    }
    fn validate_boundary_candidate(
        &self,
        spec: &fin_domain::boundaries::BoundarySpec,
        prepared: &PreparedOperation,
    ) -> Result<(), FinError> {
        boundaries::validate(spec, prepared)
    }
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
                signing_enabled: true,
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
        if let Err(error) = self
            .trading_snapshot(&mut state, &mut coverage, &mut pending)
            .await
        {
            observer_complete = false;
            pending.push(format!("Trading observation unavailable ({})", error.code));
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
        if trading::is_write(operation) {
            return self.trading_preflight(operation, raw, context).await;
        }
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
            if !context.baseline.coverage.contains(&account.resource.0) {
                return Err(error(
                    "PLAN_BASELINE_INCOMPLETE",
                    "preflight",
                    "Every physical Binance transfer resource must be captured in the baseline.",
                ));
            }
        }
        let (child, child_transfer_type) = if from.name == authority.name {
            (to, 1)
        } else {
            (from, 2)
        };
        self.verify_child_transfer_history_access(
            child,
            authority,
            &input.coin,
            child_transfer_type,
        )
        .await?;
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
        if matches!(operation, "binance.deposit-addresses" | "binance.whitelist") {
            return self.read_address_book(operation, raw).await;
        }
        if operation.starts_with("binance.spot.") || operation.starts_with("binance.futures.") {
            return crate::trading_reads::trading_read(
                operation,
                raw,
                &self.config,
                self.transport.as_ref(),
            )
            .await;
        }
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
                        "ipRestrict": unknown(permission.ip_restrict), "enableSpotAndMarginTrading": unknown(permission.enable_spot_and_margin_trading), "enableFutures": unknown(permission.enable_futures),
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
                        "withdrawTag":network.withdraw_tag,"busy":network.busy,"contractAddress":network.contract_address,"denomination":network.denomination,"addressRegex":network.address_regex,"memoRegex":network.memo_regex
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
                if currency.deposit_all_enable != Some(true) || network.deposit_enable != Some(true)
                {
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
        if trading::is_write(operation) {
            return self.trading_prepare(operation, input, None).await;
        }
        match operation {
            "binance.spot-order" => self.prepare_spot(self.parse_input(input).await?).await,
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

    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &fin_domain::PreparationContext,
    ) -> Result<PreparedOperation, FinError> {
        if operation == "binance.futures.order-place" {
            return self
                .trading_prepare_with_anchor(operation, input, context)
                .await;
        }
        self.prepare(operation, input).await
    }

    async fn refresh_prepared(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation, FinError> {
        let mut current = if prepared.operation == "binance.futures.order-place" {
            let mut context = fin_domain::PreparationContext::default();
            if let Some(anchor) = prepared.binding.get("priorAnchor") {
                context
                    .positions
                    .push(serde_json::from_value(anchor.clone()).map_err(|_| {
                        error(
                            "PREVIEW_INVALID",
                            "submission",
                            "The trusted derivative anchor is invalid.",
                        )
                    })?);
            }
            self.trading_prepare_with_anchor(&prepared.operation, &prepared.input, &context)
                .await?
        } else {
            self.prepare(&prepared.operation, &prepared.input).await?
        };
        if let Some(metadata) = prepared
            .binding
            .get(fin_domain::boundaries::BOUNDARY_METADATA_KEY)
        {
            let binding = current
                .binding
                .as_object_mut()
                .ok_or_else(|| FinError::validation("Invalid prepared binding."))?;
            binding.remove(fin_domain::value_bounds::KEY);
            binding.insert(
                fin_domain::boundaries::BOUNDARY_METADATA_KEY.into(),
                metadata.clone(),
            );
        } else {
            fin_domain::value_bounds::carry(prepared, &mut current).map_err(|failure| {
                if failure.code == "STALE_VALUATION" { failure } else {
                    error("PREVIEW_CHANGED", "submission", "Fresh USD pricing exceeds the approved ceiling or changes identity; obtain fresh authorization.")
                }
            })?;
        }
        Ok(current)
    }

    type Request = BinanceDispatch;

    fn submission_kind(&self) -> &str {
        "binance"
    }

    fn valuation_matches(
        &self,
        approved: &PreparedOperation,
        refreshed: &PreparedOperation,
    ) -> bool {
        if approved
            .binding
            .get(fin_domain::boundaries::BOUNDARY_METADATA_KEY)
            .is_some()
        {
            return refreshed.valuation.as_ref().is_some_and(|value| {
                !value.prices.is_empty()
                    && value.prices.iter().all(|price| {
                        fin_domain::price_freshness::is_fresh(
                            &price.observed_at,
                            &price.source,
                            Utc::now(),
                        ) && (!price.usd_micros_per_unit.is_zero()
                            || fin_domain::unpriced::is_estimate(price))
                    })
            });
        }
        if approved
            .binding
            .get(fin_domain::value_bounds::KEY)
            .is_some()
        {
            return approved.binding.get(fin_domain::value_bounds::KEY)
                == refreshed.binding.get(fin_domain::value_bounds::KEY)
                && fin_domain::value_bounds::validate(refreshed).is_ok();
        }
        if trading::is_write(&approved.operation) {
            return trading::valuation_matches(approved, refreshed);
        }
        approved_native_valuation_matches(approved, refreshed)
    }

    async fn plan_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<Self::Request>, FinError> {
        if trading::is_write(&current.operation) {
            return self.trading_submission(current).await;
        }
        if current.operation == "binance.spot-order" {
            return self.plan_spot_submission(current).await;
        }
        let input: Value = current.input.clone();
        let submission;
        let authority;
        let request;
        match current.operation.as_str() {
            "binance.transfer" => {
                let input: TransferInput = self.parse_input(&input).await?;
                let (main, from, to, signer, sub_to_master) = self.transfer_accounts(&input)?;
                let (r, source_before, dest_before, _, _) = self.transfer_route(&input).await?;
                request = r;
                if sub_to_master {
                    let local_id = current.binding["localSubmissionId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| {
                            error(
                                "PREVIEW_INVALID",
                                "submission",
                                "Subaccount transfer identity is missing.",
                            )
                        })?;
                    submission = Submission {
                        kind: "binance".into(),
                        reference: local_id.into(),
                        data: json!({
                            "operation":"binance.transfer","route":"sub_to_master","binding":current.binding,
                            "submittedAt":now(),"sourceFreeBefore":source_before,"destinationFreeBefore":dest_before,
                            "authority":main.name,"signer":signer.name,"from":from.name,"to":to.name,
                            "coin":input.coin,"amount":input.amount
                        }),
                    };
                    // Binance's subToMaster endpoint returns only txnId. Carry
                    // the exact already-journaled Submission into dispatch so
                    // the venue receipt can be atomically linked before return.
                    let dispatch_submission = submission.clone();
                    return Ok(PlannedSubmission {
                        submission,
                        request: BinanceDispatch {
                            authority: signer.name.clone(),
                            request,
                            noop: None,
                            receipt_submission: Some(dispatch_submission),
                        },
                    });
                }
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
                let main = self.account(&input.account)?;
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
                        "account":main.name,"destination":current.binding["destination"],"coin":input.coin,"amount":input.amount,"address":request.get("address"),"network":request.get("network"),"memo":request.get("addressTag"),
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
        Ok(PlannedSubmission {
            submission,
            request: BinanceDispatch {
                authority: authority.name.clone(),
                request,
                noop: None,
                receipt_submission: None,
            },
        })
    }

    async fn dispatch(
        &self,
        request: &BinanceDispatch,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value, FinError> {
        if request.request.endpoint == BinanceEndpoint::SubToMasterTransfer {
            let submission = request.receipt_submission.as_ref().ok_or_else(|| {
                error(
                    "SUBMISSION_INVALID",
                    "submission",
                    "Subaccount transfer receipt identity is unavailable.",
                )
            })?;
            journal.assert_authorized()?;
            request
                .request
                .validate()
                .map_err(|e| error("INVALID_ENDPOINT_REQUEST", "submission", e.to_string()))?;
            let response = self
                .transport
                .request_authorized(
                    &self.account(&request.authority)?.credential,
                    request.request.clone(),
                    journal.clone(),
                )
                .await
                .map_err(|e| transport_error("submission", e))?;
            let txn_id = response.get("txnId").ok_or_else(|| {
                error(
                    "SUBMISSION_RESPONSE_INVALID",
                    "submission",
                    "Binance subToMaster response lacks txnId; keep the original request unknown.",
                )
            })?;
            let venue_id = VenueIdentifier::parse(txn_id).map_err(domain_error)?;
            journal
                .persist_dispatch_evidence(
                    submission,
                    &json!({"route":"sub_to_master","venueId":venue_id.as_str()}),
                )
                .map_err(|_| error("DISPATCH_EVIDENCE_UNAVAILABLE", "submission", "Binance accepted the transfer, but its venue receipt could not be durably linked. Reconcile the original submission."))?;
            return Ok(response);
        }
        if let Some(response) = &request.noop {
            journal.assert_authorized()?;
            return Ok(response.clone());
        }
        request
            .request
            .validate()
            .map_err(|e| error("INVALID_ENDPOINT_REQUEST", "submission", e.to_string()))?;
        self.transport
            .request_authorized(
                &self.account(&request.authority)?.credential,
                request.request.clone(),
                journal,
            )
            .await
            .map_err(|e| transport_error("submission", e))
    }

    fn validate_response(&self, submission: &Submission, response: &Value) -> Result<(), FinError> {
        if trading::is_write(submission.data["operation"].as_str().unwrap_or_default()) {
            return trading::validate_response(submission, response);
        }
        match submission.data["operation"].as_str().unwrap_or_default() {
            "binance.spot-order" => {
                if response["clientOrderId"] != submission.reference {
                    return Err(error(
                        "SUBMISSION_ID_MISMATCH",
                        "submission",
                        "Reconcile the original journaled spot order identity.",
                    ));
                }
            }
            "binance.transfer" => {
                if submission.data.get("route").and_then(Value::as_str) == Some("sub_to_master") {
                    let txn_id = response.get("txnId").ok_or_else(|| error("SUBMISSION_RESPONSE_INVALID", "submission", "Binance subToMaster response lacks txnId; reconcile the original submission."))?;
                    let venue_id = VenueIdentifier::parse(txn_id).map_err(domain_error)?;
                    let _ = venue_id;
                    return Ok(());
                }
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
        Ok(())
    }

    async fn reconcile_submission(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, FinError> {
        if trading::is_write(&record.operation) {
            return self.trading_reconcile(record, submission).await;
        }
        match record.operation.as_str() {
            "binance.spot-order" => self.reconcile_spot(record, submission).await,
            "binance.transfer"
                if submission.data.get("route").and_then(Value::as_str)
                    == Some("sub_to_master") =>
            {
                self.reconcile_sub_to_master(record, submission).await
            }
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

impl BinanceProfile {
    async fn preflight_withdrawal(
        &self,
        raw: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof, FinError> {
        let input: WithdrawInput = self.parse_input(raw).await?;
        let account = self.account(&input.account)?;
        self.validate_withdrawal_selector(&input)?;
        if input.wallet_type != "SPOT" {
            return Err(error(
                "UNSUPPORTED_WALLET",
                "preflight",
                "Select the Binance SPOT wallet.",
            ));
        }
        let currency = self.currency(account, &input.coin).await?;
        let network = self.network(&currency, &input.network)?;
        let destination = self.withdrawal_destination(&input, network).await?;
        if !self.receipt_available(&destination, &input.coin, &input.network) {
            return Err(error(
                "RECEIPT_VERIFIER_MISSING",
                "preflight",
                "Tracked restoration requires independently observable destination holdings and a return route.",
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
        if currency.withdraw_all_enable != Some(true)
            || network.withdraw_enable != Some(true)
            || network.busy == Some(true)
        {
            return Err(error(
                "WITHDRAWAL_DISABLED",
                "preflight",
                "The selected Binance withdrawal route is disabled or busy.",
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
        let max_fee = observed_decimal(&input.max_fee)?;
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
            evidence: json!({"method":"binance-conditional-withdrawal-v2","account":account.resource,"destination":destination.resource,"address":destination.address,"network":input.network,"memo":input.memo,"publishedFee":fee.canonical(),"maximumFee":max_fee.canonical(),"minimumReceive":min_receive.canonical(),"maximumReceive":quantity.canonical(),"precisionIncrement":increment.canonical(),"travelRule":"NIL","conditional":"Actual execution still needs a fresh ordinary Binance preview, Touch ID, original withdrawOrderId journaling, and canonical Ethereum receipt verification."}),
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
            if complete
                && let Some(rejected) = self
                    .reconcile_signature_rejection(
                        record, submission, from, to, authority, &requested, complete,
                    )
                    .await?
            {
                return Ok(rejected);
            }
            return Ok(unknown(
                json!({"clientTranId":client_id,"historyComplete":complete,"reason":"No matching transfer history row proves whether submission occurred; do not retry."}),
            ));
        }
        if matches.len() != 1 {
            return Ok(unknown(
                json!({"clientTranId":client_id,"reason":"Multiple matching transfer history rows require operator reconciliation."}),
            ));
        }
        if !complete {
            return Ok(unknown(json!({
                "clientTranId": client_id,
                "venueId": matches[0].tran_id.as_str(),
                "historyComplete": false,
                "reason": "The matching transfer row appeared in an incomplete master history window; terminal proof requires complete bounded pagination."
            })));
        }
        let row = matches.remove(0);
        let (child, child_row_email, master_row_email) = if from.name == authority.name {
            (&to, row.to_email.as_deref(), row.from_email.as_deref())
        } else {
            (&from, row.from_email.as_deref(), row.to_email.as_deref())
        };
        let child_email_matches = child_row_email == child.email.as_deref();
        let configured_master_email_matches = master_row_email == authority.email.as_deref();
        let identity_matches = row.client_tran_id.as_deref() == Some(client_id)
            && row.asset == coin
            && row.amount.checked_cmp(&requested).is_eq()
            && row.from_account_type == "SPOT"
            && row.to_account_type == "SPOT"
            && child_email_matches
            && data
                .get("venueId")
                .and_then(Value::as_str)
                .is_none_or(|id| row.tran_id.as_str() == id);
        if !identity_matches {
            return Ok(unknown(
                json!({"clientTranId":client_id,"historyComplete":complete,"reason":"Transfer history identity, account route, amount, wallet type or persisted venue ID mismatch."}),
            ));
        }
        let child_history_proof = if configured_master_email_matches {
            Value::Null
        } else if authority.email.is_none() && master_row_email.is_some() {
            let proof = self
                .child_transfer_history_confirms(
                    child,
                    authority,
                    &row,
                    coin,
                    started,
                    Utc::now().timestamp_millis().max(0) as u64,
                )
                .await
                .unwrap_or(None);
            let Some(proof) = proof else {
                return Ok(unknown(json!({
                    "clientTranId": client_id,
                    "venueId": row.tran_id.as_str(),
                    "reason": "Configured master email is unavailable and the selected child credential did not prove the matching SPOT transfer counterpart."
                })));
            };
            proof
        } else {
            return Ok(unknown(json!({
                "clientTranId": client_id,
                "venueId": row.tran_id.as_str(),
                "reason": "Transfer history master-side email differs from the configured identity and cannot be safely corroborated."
            })));
        };
        let child_history_verified = !child_history_proof.is_null();
        let evidence_base = json!({"clientTranId":client_id,"venueId":row.tran_id.as_str(),"from":from.name,"to":to.name,"asset":coin,"amount":row.amount.canonical(),"venueStatus":row.status,"historyComplete":complete,"childHistoryVerified":child_history_verified,"childHistoryProof":child_history_proof});
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

    async fn reconcile_sub_to_master(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, FinError> {
        let unknown = |evidence| Reconciliation {
            status: Status::Unknown,
            evidence,
            verified: None,
        };
        let data = &submission.data;
        let binding = data.get("binding").ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted subToMaster binding is missing.",
            )
        })?;
        let main_name = data
            .get("authority")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted master identity is missing.",
                )
            })?;
        let signer_name = data.get("signer").and_then(Value::as_str).ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted child signer is missing.",
            )
        })?;
        let main = self.require_main(main_name)?;
        let child = self.account(signer_name)?;
        let from_name = binding
            .get("from")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let to_name = binding
            .get("to")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let coin = binding
            .get("coin")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let qty_text = binding
            .get("amount")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let requested = amount(qty_text)?;
        let params: BinanceRequest = serde_json::from_value(
            binding.get("params").cloned().unwrap_or(Value::Null),
        )
        .map_err(|_| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted subToMaster request is invalid.",
            )
        })?;
        if record.dispatch_evidence.is_none() {
            return Ok(unknown(
                json!({"route":"sub_to_master","localSubmissionId":submission.reference,"reason":"The response receipt was not durably linked before a possible response loss; no transfer is inferred from amount or time."}),
            ));
        }
        let dispatch_evidence = record.dispatch_evidence.as_ref().expect("checked above");
        let persisted_venue_id = dispatch_evidence
            .get("venueId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted subToMaster venue receipt is malformed.",
                )
            })?;
        let venue_id = VenueIdentifier::parse(&Value::String(persisted_venue_id.to_owned()))
            .map_err(domain_error)?;
        if dispatch_evidence != &json!({"route":"sub_to_master","venueId":venue_id.as_str()})
            || data.get("route").and_then(Value::as_str) != Some("sub_to_master")
            || record.operation != "binance.transfer"
            || !matches!(child.role, AccountRole::Sub)
            || from_name != child.name
            || to_name != main.name
            || child.parent.as_deref() != Some(main.name.as_str())
            || binding.get("authority").and_then(Value::as_str) != Some(child.name.as_str())
            || binding.get("parentAuthority").and_then(Value::as_str) != Some(main.name.as_str())
            || data.get("signer").and_then(Value::as_str) != Some(child.name.as_str())
            || data.get("authority").and_then(Value::as_str) != Some(main.name.as_str())
            || binding.get("route").and_then(Value::as_str) != Some("sub_to_master")
            || binding.get("localSubmissionId").and_then(Value::as_str)
                != Some(submission.reference.as_str())
            || binding.get("from").and_then(Value::as_str)
                != data.get("from").and_then(Value::as_str)
            || binding.get("to").and_then(Value::as_str) != data.get("to").and_then(Value::as_str)
            || binding.get("coin").and_then(Value::as_str)
                != data.get("coin").and_then(Value::as_str)
            || binding.get("coin").and_then(Value::as_str) != Some(coin)
            || binding.get("amount").and_then(Value::as_str)
                != data.get("amount").and_then(Value::as_str)
            || binding.get("amount").and_then(Value::as_str) != Some(qty_text)
            || binding.get("fromWallet").and_then(Value::as_str) != Some("SPOT")
            || binding.get("toWallet").and_then(Value::as_str) != Some("SPOT")
            || params.endpoint != BinanceEndpoint::SubToMasterTransfer
            || params.validate().is_err()
            || params.params.len() != 3
            || params.get("asset") != Some(coin)
            || params
                .get("amount")
                .and_then(|v| amount(v).ok())
                .is_none_or(|v| !v.checked_cmp(&requested).is_eq())
            || params.get("recvWindow").is_none()
        {
            return Err(error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted subToMaster route, signer, receipt or immutable request does not match.",
            ));
        }
        if from_name != child.name || to_name != main.name || coin.is_empty() {
            return Err(error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted child-to-master transfer route is invalid.",
            ));
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
                json!({"route":"sub_to_master","venueId":venue_id.as_str(),"historyComplete":false,"reason":"Child transfer history is outside the supported reconciliation window."}),
            ));
        }
        // Re-establish both ends using the configured hierarchy and physical
        // UIDs. The master remains the only read authority for child assets.
        if self.verify_subaccount(child, main).await.is_err()
            || self.account_identity(child).await.is_err()
            || self.account_identity(main).await.is_err()
        {
            return Ok(unknown(
                json!({"route":"sub_to_master","venueId":venue_id.as_str(),"reason":"Configured child/master identity could not be revalidated."}),
            ));
        }
        let end_ms = Utc::now().timestamp_millis().max(0) as u64;
        let history_request = subaccount_transfer_history_request(
            coin,
            2,
            started.timestamp_millis().max(0) as u64,
            end_ms,
            200,
        )
        .map_err(domain_error)?;
        let history_response = match self.call(child, history_request, "reconciliation").await {
            Ok(value) => value,
            Err(_) => {
                return Ok(unknown(
                    json!({"route":"sub_to_master","venueId":venue_id.as_str(),"historyComplete":false,"reason":"Child transfer history is unavailable; reconcile the same submission."}),
                ));
            }
        };
        let history: SubAccountTransferHistoryResponse = match parse_response(history_response) {
            Ok(rows) => rows,
            Err(_) => {
                return Ok(unknown(
                    json!({"route":"sub_to_master","venueId":venue_id.as_str(),"historyComplete":false,"reason":"Child transfer history response is invalid."}),
                ));
            }
        };
        // This endpoint has no pagination. A capped result cannot establish a
        // unique venue receipt, even when one matching txnId is present.
        if history.len() >= 200 {
            return Ok(unknown(
                json!({"route":"sub_to_master","venueId":venue_id.as_str(),"historyComplete":false,"reason":"Child transfer history reached its response cap."}),
            ));
        }
        let matches = history
            .iter()
            .filter(|row| row.tran_id == venue_id)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Ok(unknown(
                json!({"route":"sub_to_master","venueId":venue_id.as_str(),"historyComplete":true,"reason":"No unique child history row matches the persisted venue receipt."}),
            ));
        }
        let row = matches[0];
        if row.counter_party != "master"
            || row.transfer_type != 2
            || row.asset != coin
            || !row.quantity.checked_cmp(&requested).is_eq()
            || row.from_account_type != "SPOT"
            || row.to_account_type != "SPOT"
            || row.status != "SUCCESS"
            || main
                .email
                .as_deref()
                .is_some_and(|email| row.email != email)
            || row.timestamp < started.timestamp_millis().max(0) as u64
            || row.timestamp > end_ms
        {
            return Ok(unknown(
                json!({"route":"sub_to_master","venueId":venue_id.as_str(),"historyComplete":true,"reason":"Child history row does not match the exact master, asset, quantity, SPOT wallets, SUCCESS status, or time window."}),
            ));
        }
        let source_before =
            observed_decimal(data["sourceFreeBefore"].as_str().unwrap_or_default())?;
        let destination_before =
            observed_decimal(data["destinationFreeBefore"].as_str().unwrap_or_default())?;
        let source_now = Self::free(&self.spot_balances(child, Some(main)).await?, coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        let destination_now = Self::free(&self.spot_balances(main, None).await?, coin)
            .cloned()
            .unwrap_or_else(ExactDecimal::zero);
        let source_debit = decimal_delta(&source_before, &source_now);
        let destination_credit = decimal_delta(&destination_now, &destination_before);
        if source_debit
            .as_ref()
            .is_none_or(|value| !value.checked_cmp(&requested).is_eq())
            || destination_credit
                .as_ref()
                .is_none_or(|value| !value.checked_cmp(&requested).is_eq())
        {
            return Ok(unknown(
                json!({"route":"sub_to_master","venueId":venue_id.as_str(),"sourceFreeBefore":source_before.canonical(),"sourceFree":source_now.canonical(),"destinationFreeBefore":destination_before.canonical(),"destinationFree":destination_now.canonical(),"reason":"Exact source debit and destination credit do not match the transfer; preserve the unresolved record."}),
            ));
        }
        let asset: AssetId = serde_json::from_value(binding["asset"].clone()).map_err(|_| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted transfer asset identity is invalid.",
            )
        })?;
        if asset.network != "binance" || asset.reference != coin || asset.decimals != 36 {
            return Ok(unknown(
                json!({"route":"sub_to_master","venueId":venue_id.as_str(),"reason":"Persisted asset identity does not match the exact Binance transfer asset."}),
            ));
        }
        let atoms = decimal_to_atoms(&requested, asset.decimals)?;
        let effects = vec![FinancialEffect::Transfer {
            from: child.resource.clone(),
            to: main.resource.clone(),
            debit: Amount {
                asset: asset.clone(),
                atoms: atoms.clone(),
            },
            credit: Amount { asset, atoms },
        }];
        let evidence = json!({"route":"sub_to_master","localSubmissionId":submission.reference,"venueId":venue_id.as_str(),"historyComplete":true,"childHistoryVerified":true,"counterParty":"master","transferType":2,"status":"SUCCESS","sourceFreeBefore":source_before.canonical(),"sourceFree":source_now.canonical(),"destinationFreeBefore":destination_before.canonical(),"destinationFree":destination_now.canonical(),"balanceCorrelation":"exact persisted venue receipt, child type-2 master history and exact net SPOT balance deltas"});
        let verified = make_verified(submission.clone(), effects, evidence.clone())?;
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
        let account = self.account(binding["account"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted Binance account is invalid.",
            )
        })?)?;
        let network = data["network"]
            .as_str()
            .or_else(|| binding["network"].as_str())
            .or_else(|| {
                binding["params"]["params"]
                    .as_array()
                    .and_then(|params| params.iter().find(|p| p["key"] == "network"))
                    .and_then(|p| p["value"].as_str())
            })
            .unwrap_or("ETH");
        // New submissions freeze recipient identities; older records retain
        // their original named destination and selected-chain fallback.
        let destination = if let (Some(address), Some(resource)) = (
            binding["receiptAddress"].as_str(),
            binding["receiptResource"].as_str(),
        ) {
            EthereumDestination {
                name: binding["destination"].as_str().unwrap_or(address).into(),
                resource: ResourceId(resource.into()),
                address: address.into(),
                chain_id: binding["chainId"].as_u64().unwrap_or(0),
                assets: BTreeMap::new(),
                confirmations: binding["confirmations"]
                    .as_u64()
                    .and_then(|n| n.try_into().ok())
                    .unwrap_or(1),
            }
        } else {
            let name = binding["destination"].as_str().ok_or_else(|| {
                error(
                    "RECONCILIATION_INVALID",
                    "reconciliation",
                    "Persisted destination is invalid.",
                )
            })?;
            let chain_id = binding["chainId"]
                .as_u64()
                .or_else(|| binance_evm_chain_id(network))
                .unwrap_or(1);
            if chain_id == 1 {
                self.destination(name)?.clone()
            } else {
                self.config
                    .destinations
                    .iter()
                    .find(|destination| {
                        destination.name == name && destination.chain_id == chain_id
                    })
                    .ok_or_else(|| {
                        error(
                            "UNKNOWN_CONNECTION",
                            "reconciliation",
                            "The original selected-chain withdrawal destination is not configured.",
                        )
                    })?
                    .clone()
            }
        };
        let memo = data["memo"]
            .as_str()
            .or_else(|| binding["memo"].as_str())
            .unwrap_or("");
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
            && row.network.as_deref() == Some(network)
            && row.address.as_deref().is_some_and(|address| {
                withdrawal::same_address(network, address, &destination.address)
            })
            && row.address_tag.as_deref().unwrap_or("") == memo
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
        let max_fee = observed_decimal(data["maximumFee"].as_str().ok_or_else(|| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted maximum fee is invalid.",
            )
        })?)?;
        let lower_bound =
            subtract_decimal(&amount_requested, &max_fee).unwrap_or_else(ExactDecimal::zero);
        let upper_bound = add_decimal(&amount_requested, &max_fee)?;
        if fee.checked_cmp(&max_fee).is_gt()
            || debit.checked_cmp(&lower_bound).is_lt()
            || debit.checked_cmp(&upper_bound).is_gt()
        {
            return Ok(unknown(
                json!({"withdrawal":base_evidence,"actualFee":fee.canonical(),"sourceDebitObserved":debit.canonical(),"reason":"Observed Binance fee or source debit exceeds the approved exact bounds."}),
            ));
        }
        let asset: AssetId = serde_json::from_value(binding["asset"].clone()).map_err(|_| {
            error(
                "RECONCILIATION_INVALID",
                "reconciliation",
                "Persisted payout asset identity is invalid.",
            )
        })?;
        let verifier = self
            .recipient_verifier
            .as_ref()
            .filter(|v| v.supports_asset(network, &asset));
        let Some(verifier) = verifier else {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"withdrawal":base_evidence,"venueState":"completed","destinationReceipt":"unverified","sourceDebitObserved":debit.canonical(),"actualFee":fee.canonical(),"reason":"Binance reports completion; an independent receipt observer for this destination asset/network is unavailable. Reconcile this original request; do not resubmit."}),
                verified: None,
            });
        };
        let tx_hash = row.tx_id.as_deref().filter(|hash| {
            !hash.is_empty() && hash.len() <= 256 && !hash.chars().any(char::is_control)
        });
        let Some(tx_hash) = tx_hash else {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"withdrawal":base_evidence,"venueState":"completed","destinationReceipt":"unverified","reason":"Binance reports completion but no usable transaction identifier is available."}),
                verified: None,
            });
        };
        let receipt = verifier
            .verify(PayoutRequest {
                tx_hash: tx_hash.into(),
                network: network.into(),
                memo: (!memo.is_empty()).then(|| memo.to_owned()),
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
                evidence: json!({"withdrawal":base_evidence,"recipient":receipt.evidence,"reason":"Selected-chain receipt is unverified or reverted; retain the original Binance withdrawal identity."}),
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
        let evidence = json!({"withdrawal":base_evidence,"historyComplete":complete,"withdrawOrderId":reference,"sourceFreeBefore":source_before.canonical(),"sourceFree":current.canonical(),"sourceDebitObserved":debit.canonical(),"actualFee":fee.canonical(),"recipient":receipt.evidence,"balanceCorrelation":"matching withdrawal identity plus venue fee and canonical destination chain receipt"});
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
    if approved.operation == "binance.spot-order" {
        for name in ["spentAsset", "receivedAsset"] {
            if let Some(coin) = approved.binding[name].as_str() {
                adapter_assets.insert(AssetId {
                    network: "binance".into(),
                    reference: coin.into(),
                    decimals: 36,
                });
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
