//! Protected composition points. No secret material enters GoFin operation data.
use async_trait::async_trait;
use gofin_domain::{
    AssetId, AssetPrice, ConnectionMetadata, GoFinError, Quantity, ResourceId, SubmissionJournal,
};
use gofin_exchange::ccxt::{
    HyperliquidActionEnvelope, HyperliquidExplorerRequest, HyperliquidInfoRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidAccount {
    pub name: String,
    pub owner: String,
    pub credential_ref: Option<String>,
    pub signing_enabled: bool,
    /// Human-reviewed address scope for trading. The signer and nonce identity
    /// remain `owner`; the native account queried is `execution_address()`.
    #[serde(default)]
    pub execution_scope: HyperliquidExecutionScope,
}

/// Address on whose book a protected master signer may act. Only protected
/// enrollment can install a non-owner scope after an official relationship read.
#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HyperliquidExecutionScope {
    #[default]
    Owner,
    Subaccount {
        address: String,
    },
    Vault {
        address: String,
    },
}

impl HyperliquidExecutionScope {
    pub fn address(&self) -> Option<&str> {
        match self {
            Self::Owner => None,
            Self::Subaccount { address } | Self::Vault { address } => Some(address),
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Subaccount { .. } => "subaccount",
            Self::Vault { .. } => "vault",
        }
    }
}

impl HyperliquidAccount {
    pub fn validate(&self) -> Result<(), GoFinError> {
        if self.name.is_empty()
            || !valid_address(&self.owner)
            || self.owner.bytes().any(|byte| byte.is_ascii_uppercase())
            || self.owner[2..].bytes().all(|byte| byte == b'0')
            || self.signing_enabled != self.credential_ref.is_some()
            || self.execution_scope.address().is_some_and(|address| {
                !valid_address(address)
                    || address[2..].bytes().all(|byte| byte == b'0')
                    || address != address.to_ascii_lowercase()
                    || address.eq_ignore_ascii_case(&self.owner)
            })
        {
            return Err(GoFinError::validation(
                "Hyperliquid requires an exact owner and a protected signer mapping.",
            ));
        }
        Ok(())
    }
    pub fn resource(&self, wallet: &str, dex: Option<&str>) -> ResourceId {
        ResourceId(format!(
            "hyperliquid:mainnet:{}:{wallet}:{}",
            self.execution_address().to_lowercase(),
            dex.unwrap_or("primary")
        ))
    }
    pub fn execution_address(&self) -> &str {
        self.execution_scope.address().unwrap_or(&self.owner)
    }
    /// CCXT's `vaultAddress` field selects the protected subaccount or vault;
    /// it is never supplied by an operation caller.
    pub fn vault_address(&self) -> Option<&str> {
        self.execution_scope.address()
    }
    pub fn metadata(&self) -> ConnectionMetadata {
        ConnectionMetadata {
            name: self.name.clone(),
            provider: "hyperliquid".into(),
            resource: self.resource("account", None),
            role: Some(self.execution_scope.kind().into()),
            parent: (self.execution_address() != self.owner)
                .then(|| self.owner.to_ascii_lowercase()),
            network: Some("hyperliquid:mainnet".into()),
            address: Some(self.execution_address().to_lowercase()),
            credential_ref: None,
            signing_enabled: self.signing_enabled,
            details: json!({"owner":self.owner.to_lowercase(), "execution_address":self.execution_address().to_lowercase(), "execution_scope":self.execution_scope.kind(), "wallets":["spot","perpetual"], "signer":"protected_owner_key"}),
        }
    }
}

/// Verify a delegated target against the exact official owner relationship.
/// This consumes typed native responses, never a caller-provided assertion.
pub fn verify_execution_scope(
    owner: &str,
    scope: &HyperliquidExecutionScope,
    subaccounts: Option<&Value>,
    vault_details: Option<&Value>,
) -> Result<(), GoFinError> {
    let Some(address) = scope.address() else {
        return Ok(());
    };
    if !valid_address(owner) || !valid_address(address) {
        return Err(GoFinError::validation(
            "The Hyperliquid scope address is invalid.",
        ));
    }
    let owner = owner.to_ascii_lowercase();
    let address = address.to_ascii_lowercase();
    let verified = match scope {
        HyperliquidExecutionScope::Owner => true,
        HyperliquidExecutionScope::Subaccount { .. } => {
            subaccounts.and_then(Value::as_array).is_some_and(|rows| {
                rows.iter().any(|row| {
                    row.get("master")
                        .and_then(Value::as_str)
                        .is_some_and(|v| v.eq_ignore_ascii_case(&owner))
                        && row
                            .get("subAccountUser")
                            .and_then(Value::as_str)
                            .is_some_and(|v| v.eq_ignore_ascii_case(&address))
                })
            })
        }
        HyperliquidExecutionScope::Vault { .. } => {
            let detail = vault_details.and_then(|v| v.get("data").or(Some(v)));
            detail.is_some_and(|detail| {
                detail
                    .get("vaultAddress")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.eq_ignore_ascii_case(&address))
                    && detail
                        .get("leader")
                        .and_then(Value::as_str)
                        .is_some_and(|v| v.eq_ignore_ascii_case(&owner))
            })
        }
    };
    if verified {
        Ok(())
    } else {
        Err(GoFinError::new(
            "HYPERLIQUID_SCOPE_UNVERIFIED",
            "configuration",
            "The requested Hyperliquid delegated address is not proven to belong to this master signer.",
            "Choose an owner, subaccount or vault whose exact relationship is confirmed by Hyperliquid, then retry protected enrollment.",
        ))
    }
}

pub fn valid_address(address: &str) -> bool {
    address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Exact native Hyperliquid USDC spot-token identity from `spotMeta`.
pub const NATIVE_USDC_TOKEN_ID: &str = "0x6d1e7cde53ba9467b783cb7c530ce054";
pub const NATIVE_USDC_TOKEN_INDEX: u32 = 0;
pub const NATIVE_USDC_WEI_DECIMALS: u8 = 8;

fn raw_asset(reference: &str) -> AssetId {
    // Exchange observations retain exact decimal precision, independent of
    // an Arbitrum token's on-chain units and the market's order-size increments.
    AssetId {
        network: "hyperliquid:mainnet".into(),
        reference: reference.into(),
        decimals: 36,
    }
}
pub fn asset(reference: &str) -> AssetId {
    if reference.eq_ignore_ascii_case(NATIVE_USDC_TOKEN_ID) {
        usdc()
    } else {
        raw_asset(reference)
    }
}
pub fn usdc() -> AssetId {
    raw_asset("USDC")
}

/// Resolves a spot token for a balance snapshot. All GoFin Hyperliquid balances
/// use scale-36 atoms; only the exact native USDC metadata tuple aliases USDC.
pub fn spot_token_asset(reference: &str, token_index: Option<u32>, decimals: u8) -> AssetId {
    if reference.eq_ignore_ascii_case(NATIVE_USDC_TOKEN_ID)
        && token_index == Some(NATIVE_USDC_TOKEN_INDEX)
        && decimals == NATIVE_USDC_WEI_DECIMALS
    {
        usdc()
    } else {
        raw_asset(reference)
    }
}

/// Confirms that a market's quote is the built-in core USDC or the exact
/// canonical spot token used by spot and HIP-3 collateral metadata.
pub fn is_canonical_usdc_quote(market: &crate::types::NativeMarket) -> bool {
    let quote = &market.quote_asset;
    match market.kind {
        crate::types::MarketKind::Perpetual => {
            quote.reference == "USDC"
                && quote.symbol == "USDC"
                && quote.decimals == 6
                && quote.token_index.is_none()
        }
        crate::types::MarketKind::Spot => {
            quote.reference.eq_ignore_ascii_case(NATIVE_USDC_TOKEN_ID)
                && quote.symbol == "USDC"
                && quote.decimals == NATIVE_USDC_WEI_DECIMALS
                && quote.token_index == Some(NATIVE_USDC_TOKEN_INDEX)
                && market
                    .spot_tokens
                    .is_some_and(|tokens| tokens[1] == NATIVE_USDC_TOKEN_INDEX)
        }
        crate::types::MarketKind::Hip3Perpetual => {
            quote.reference.eq_ignore_ascii_case(NATIVE_USDC_TOKEN_ID)
                && quote.symbol == "USDC"
                && quote.decimals == NATIVE_USDC_WEI_DECIMALS
                && quote.token_index == Some(NATIVE_USDC_TOKEN_INDEX)
        }
    }
}

#[async_trait]
pub trait HyperliquidTransport: Send + Sync {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value, GoFinError>;
    async fn explorer(&self, _request: HyperliquidExplorerRequest) -> Result<Value, GoFinError> {
        Err(GoFinError::new(
            "HYPERLIQUID_EXPLORER_UNAVAILABLE",
            "reconciliation",
            "Typed Hyperliquid transaction evidence is unavailable.",
            "Keep the original submission unresolved until the explorer transport is configured.",
        ))
    }
    async fn dispatch(
        &self,
        account: &HyperliquidAccount,
        envelope: &HyperliquidActionEnvelope,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value, GoFinError>;
}

/// Supplies the actual USD rate for USDC; stablecoin parity is never assumed.
#[async_trait]
pub trait HyperliquidPrices: Send + Sync {
    async fn usdc_price(&self) -> Result<AssetPrice, GoFinError>;
}

/// Broker-owned monotonic signer identity storage, shared across account scopes.
pub trait HyperliquidNonces: Send + Sync {
    fn allocate(&self, owner: &str, now_ms: u64) -> Result<u64, GoFinError>;
    fn attach(
        &self,
        _state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        Ok(())
    }
    fn operation(&self, _id: &str) -> Result<Option<gofin_domain::OperationRecord>, GoFinError> {
        Ok(None)
    }
    fn recovery_child(
        &self,
        _parent_operation_id: &str,
    ) -> Result<Option<gofin_domain::OperationRecord>, GoFinError> {
        Ok(None)
    }
}

#[derive(Default)]
pub struct DeferredExecutionState(
    std::sync::Mutex<Option<Arc<dyn gofin_domain::execution_state::ExecutionState>>>,
);
impl DeferredExecutionState {
    fn state(&self) -> Result<Arc<dyn gofin_domain::execution_state::ExecutionState>, GoFinError> {
        self.0
            .lock()
            .map_err(|_| GoFinError::validation("Execution state lock failed."))?
            .clone()
            .ok_or_else(|| {
                GoFinError::new(
                    "EXECUTION_STATE_UNAVAILABLE",
                    "journal",
                    "The adapter is not attached to its durable journal.",
                    "Start the matching GoFin broker before writes.",
                )
            })
    }
}
impl HyperliquidNonces for DeferredExecutionState {
    fn allocate(&self, owner: &str, now_ms: u64) -> Result<u64, GoFinError> {
        self.state()?.allocate_nonce(
            &format!("hyperliquid:mainnet:{}", owner.to_lowercase()),
            now_ms,
        )
    }
    fn attach(
        &self,
        state: Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<(), GoFinError> {
        let mut saved = self
            .0
            .lock()
            .map_err(|_| GoFinError::validation("Execution state lock failed."))?;
        if saved.is_some() {
            return Err(GoFinError::validation(
                "Execution state is already attached.",
            ));
        }
        *saved = Some(state);
        Ok(())
    }
    fn operation(&self, id: &str) -> Result<Option<gofin_domain::OperationRecord>, GoFinError> {
        self.state()?.operation(id)
    }
    fn recovery_child(
        &self,
        parent_operation_id: &str,
    ) -> Result<Option<gofin_domain::OperationRecord>, GoFinError> {
        self.state()?.recovery_child(parent_operation_id)
    }
}

#[derive(Clone)]
pub struct HyperliquidProfile {
    pub accounts: Vec<HyperliquidAccount>,
    pub transport: Arc<dyn HyperliquidTransport>,
    pub prices: Arc<dyn HyperliquidPrices>,
    pub nonces: Arc<dyn HyperliquidNonces>,
    pub withdrawal_proof: Option<Arc<dyn crate::account_actions::HyperliquidWithdrawalProof>>,
    pub(crate) excluded_operations: std::collections::BTreeSet<String>,
}
impl HyperliquidProfile {
    pub fn new(
        accounts: Vec<HyperliquidAccount>,
        transport: Arc<dyn HyperliquidTransport>,
        prices: Arc<dyn HyperliquidPrices>,
        nonces: Arc<dyn HyperliquidNonces>,
    ) -> Result<Self, GoFinError> {
        let mut names = std::collections::BTreeSet::new();
        for account in &accounts {
            account.validate()?;
            if !names.insert(&account.name) {
                return Err(GoFinError::validation(
                    "Duplicate Hyperliquid account alias.",
                ));
            }
        }
        Ok(Self {
            accounts,
            transport,
            prices,
            nonces,
            withdrawal_proof: None,
            excluded_operations: Default::default(),
        })
    }
    pub fn with_withdrawal_proof(
        mut self,
        proof: Arc<dyn crate::account_actions::HyperliquidWithdrawalProof>,
    ) -> Self {
        self.withdrawal_proof = Some(proof);
        self
    }
    pub fn without_operation(&self, operation: &str) -> Self {
        let mut profile = self.clone();
        profile.excluded_operations.insert(operation.into());
        profile
    }
    pub fn account(&self, name: &str) -> Result<&HyperliquidAccount, GoFinError> {
        self.accounts
            .iter()
            .find(|account| account.name == name)
            .ok_or_else(|| GoFinError::validation("Select an enrolled Hyperliquid account."))
    }
    /// Rechecks the delegated address against the live official relationship
    /// endpoint before a write is prepared or dispatched.
    pub async fn verify_account_scope(
        &self,
        account: &HyperliquidAccount,
    ) -> Result<(), GoFinError> {
        match &account.execution_scope {
            HyperliquidExecutionScope::Owner => Ok(()),
            HyperliquidExecutionScope::Subaccount { .. } => {
                let response = self
                    .transport
                    .info(HyperliquidInfoRequest::SubAccounts {
                        owner: account.owner.clone(),
                    })
                    .await?;
                verify_execution_scope(
                    &account.owner,
                    &account.execution_scope,
                    Some(&response),
                    None,
                )
            }
            HyperliquidExecutionScope::Vault { address } => {
                let response = self
                    .transport
                    .info(HyperliquidInfoRequest::VaultDetails {
                        vault_address: address.clone(),
                        user: Some(account.owner.clone()),
                    })
                    .await?;
                verify_execution_scope(
                    &account.owner,
                    &account.execution_scope,
                    None,
                    Some(&response),
                )
            }
        }
    }
    pub async fn quote_usdc(&self) -> Result<AssetPrice, GoFinError> {
        let quote = self.prices.usdc_price().await?;
        if quote.asset != usdc()
            || quote.usd_micros_per_unit.is_zero()
            || quote.unit_atoms.is_zero()
            || quote.source.is_empty()
            || !gofin_domain::price_freshness::is_fresh(
                &quote.observed_at,
                &quote.source,
                chrono::Utc::now(),
            )
        {
            return Err(GoFinError::new(
                "PRICE_DATA_INVALID",
                "valuation",
                "A fresh exact USDC/USD quote is required.",
                "Retry after obtaining valid pricing.",
            ));
        }
        Ok(quote)
    }
    pub fn value_usdc(amount: &Quantity, price: &AssetPrice) -> Result<Quantity, GoFinError> {
        amount.mul_div_ceil(&price.usd_micros_per_unit, &price.unit_atoms)
    }
}

#[cfg(test)]
mod canonical_usdc_tests {
    use super::*;
    use crate::types::{AssetReference, MarketKind, NativeMarket};

    fn market(
        kind: MarketKind,
        reference: &str,
        token_index: Option<u32>,
        decimals: u8,
    ) -> NativeMarket {
        let spot_tokens = (kind == MarketKind::Spot).then_some([1, 0]);
        NativeMarket {
            symbol: "BASE/USDC".into(),
            native_coin: "@0".into(),
            base_asset: AssetReference {
                reference: "base-token".into(),
                symbol: "BASE".into(),
                decimals: 8,
                token_index: Some(1),
            },
            quote_asset: AssetReference {
                reference: reference.into(),
                symbol: "USDC".into(),
                decimals,
                token_index,
            },
            kind,
            observed_at_ms: 1,
            dex: None,
            universe_index: 0,
            dex_index: None,
            asset_id: 10_000,
            sz_decimals: 2,
            max_leverage: None,
            is_delisted: false,
            context: None,
            spot_tokens,
        }
    }

    #[test]
    fn canonical_native_token_maps_to_shared_usdc_identity_without_rewriting_metadata() {
        assert_eq!(asset(NATIVE_USDC_TOKEN_ID), usdc());
        assert_eq!(
            spot_token_asset(
                NATIVE_USDC_TOKEN_ID,
                Some(NATIVE_USDC_TOKEN_INDEX),
                NATIVE_USDC_WEI_DECIMALS,
            ),
            usdc()
        );
        let wrong_index = spot_token_asset(NATIVE_USDC_TOKEN_ID, Some(1), 8);
        assert_eq!(wrong_index.reference, NATIVE_USDC_TOKEN_ID);
        assert_ne!(wrong_index, usdc());
        let wrong_precision = spot_token_asset(NATIVE_USDC_TOKEN_ID, Some(0), 6);
        assert_eq!(wrong_precision.reference, NATIVE_USDC_TOKEN_ID);
        assert_ne!(wrong_precision, usdc());
        assert_eq!(spot_token_asset("0xbase", Some(1), 8), asset("0xbase"));
        assert_eq!(asset("BTC").decimals, 36);
    }

    #[test]
    fn quote_gate_requires_exact_builtin_or_native_usdc_identity() {
        assert!(is_canonical_usdc_quote(&market(
            MarketKind::Perpetual,
            "USDC",
            None,
            6,
        )));
        assert!(is_canonical_usdc_quote(&market(
            MarketKind::Spot,
            NATIVE_USDC_TOKEN_ID,
            Some(0),
            8,
        )));
        assert!(is_canonical_usdc_quote(&market(
            MarketKind::Hip3Perpetual,
            NATIVE_USDC_TOKEN_ID,
            Some(0),
            8,
        )));

        // A familiar display name is not sufficient to establish identity.
        assert!(!is_canonical_usdc_quote(&market(
            MarketKind::Spot,
            "0xnot-canonical",
            Some(0),
            8,
        )));
        assert!(!is_canonical_usdc_quote(&market(
            MarketKind::Hip3Perpetual,
            NATIVE_USDC_TOKEN_ID,
            Some(1),
            8,
        )));
        assert!(!is_canonical_usdc_quote(&market(
            MarketKind::Spot,
            NATIVE_USDC_TOKEN_ID,
            Some(0),
            6,
        )));
        assert!(!is_canonical_usdc_quote(&market(
            MarketKind::Perpetual,
            "USDC",
            Some(0),
            6,
        )));
    }
}

#[cfg(test)]
mod execution_scope_tests {
    use super::*;
    const MASTER: &str = "0x1111111111111111111111111111111111111111";
    const TARGET: &str = "0x2222222222222222222222222222222222222222";

    fn account(scope: HyperliquidExecutionScope) -> HyperliquidAccount {
        HyperliquidAccount {
            name: "scope-test".into(),
            owner: MASTER.into(),
            credential_ref: Some("synthetic-key".into()),
            signing_enabled: true,
            execution_scope: scope,
        }
    }

    #[test]
    fn owner_identity_and_delegated_resource_are_separate() {
        let account = account(HyperliquidExecutionScope::Subaccount {
            address: TARGET.into(),
        });
        assert_eq!(account.execution_address(), TARGET);
        assert_eq!(account.vault_address(), Some(TARGET));
        assert_eq!(
            account.resource("perpetual", None).0,
            format!("hyperliquid:mainnet:{TARGET}:perpetual:primary")
        );
        assert_eq!(account.metadata().address.as_deref(), Some(TARGET));
        assert_eq!(account.metadata().parent.as_deref(), Some(MASTER));
        assert!(account.validate().is_ok());
    }

    #[test]
    fn relationship_proofs_require_exact_parent_target_and_leader() {
        let scope = HyperliquidExecutionScope::Subaccount {
            address: TARGET.into(),
        };
        let rows = json!([{"master":MASTER,"subAccountUser":TARGET}]);
        assert!(verify_execution_scope(MASTER, &scope, Some(&rows), None).is_ok());
        assert!(verify_execution_scope(TARGET, &scope, Some(&rows), None).is_err());
        let wrong_target = json!([{"master":MASTER,"subAccountUser":MASTER}]);
        assert!(verify_execution_scope(MASTER, &scope, Some(&wrong_target), None).is_err());

        let vault = HyperliquidExecutionScope::Vault {
            address: TARGET.into(),
        };
        let details = json!({"vaultAddress":TARGET,"leader":MASTER});
        assert!(verify_execution_scope(MASTER, &vault, None, Some(&details)).is_ok());
        let wrong_leader = json!({"vaultAddress":TARGET,"leader":TARGET});
        assert!(verify_execution_scope(MASTER, &vault, None, Some(&wrong_leader)).is_err());
        let wrong_vault = json!({"vaultAddress":MASTER,"leader":MASTER});
        assert!(verify_execution_scope(MASTER, &vault, None, Some(&wrong_vault)).is_err());
    }

    #[test]
    fn legacy_account_config_defaults_to_owner_scope() {
        let value =
            json!({"name":"legacy","owner":MASTER,"credential_ref":null,"signing_enabled":false});
        let parsed: HyperliquidAccount = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.execution_scope, HyperliquidExecutionScope::Owner);
        assert_eq!(parsed.execution_address(), MASTER);
        assert_eq!(parsed.vault_address(), None);
    }
}
