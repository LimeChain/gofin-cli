//! Ethereum wallet, Uniswap v3 and Aave v3 operations over a frozen Alloy envelope.
mod abi;
mod anchors;
mod prepare;
mod proof;
mod reads;
mod request;
mod routes;
mod snapshot;
#[cfg(test)]
mod tests;

use async_trait::async_trait;
use fin_domain::*;
use fin_evm::{
    EvmRuntime, FrozenTransaction, address,
    alloy::primitives::{Address, U256},
    lower_address,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

pub const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
pub const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub fn operation_definitions() -> Vec<Definition> {
    request::definitions()
}
/// Reviewed profile gas ceiling for read-only route financing. Actual writes
/// still simulate, estimate and freeze their own finite gas envelope.
#[doc(hidden)]
pub fn route_gas_ceiling(operation: &str) -> Result<u64> {
    match operation {
        "wallet.wrap" | "wallet.unwrap" | "wallet.approve" => Ok(100_000),
        "wallet.transfer" => Ok(150_000),
        "uniswap.swap" => Ok(350_000),
        "aave.supply" => Ok(500_000),
        "aave.borrow" => Ok(800_000),
        "aave.repay" | "aave.withdraw" => Ok(600_000),
        "aave.collateral" => Ok(200_000),
        _ => Err(error(
            "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE",
            "preflight",
            "This operation has no reviewed profile gas ceiling.",
        )),
    }
}
pub use proof::ExternalCredit;
fn error(code: &str, stage: &str, message: &str) -> FinError {
    FinError::new(
        code,
        stage,
        message,
        "Inspect the exact configured resource and original operation. Reconcile uncertain writes; never replay them.",
    )
}
fn require(condition: bool, code: &str, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(error(code, "validation", message))
    }
}
/// Recheck adapter-owned valuation immediately before entering the signing
/// runtime. The engine may have added origin prices for a bounded run, so
/// compare exact asset identities rather than requiring identical price lists.
fn require_frozen_valuation(fresh: Option<&Valuation>, approved: Option<&Valuation>) -> Result<()> {
    let changed = || {
        error(
            "VALUATION_CHANGED",
            "submission",
            "Current exact valuation differs from the approved envelope or has missing or ambiguous prices.",
        )
    };
    let (fresh, approved) = match (fresh, approved) {
        (Some(fresh), Some(approved)) => (fresh, approved),
        _ => return Err(changed()),
    };
    if fresh.movement_usd_micros != approved.movement_usd_micros
        || fresh.maximum_fee_usd_micros != approved.maximum_fee_usd_micros
        || fresh.additional_allocation_usd_micros != approved.additional_allocation_usd_micros
        || fresh.source != approved.source
        || fresh.source.trim().is_empty()
        || fresh.prices.is_empty()
    {
        return Err(changed());
    }
    for valuation in [fresh, approved] {
        let mut identities = BTreeSet::new();
        for price in &valuation.prices {
            if !identities.insert(&price.asset)
                || price.asset.network.trim().is_empty()
                || price.asset.reference.trim().is_empty()
                || price.source.trim().is_empty()
                || price.unit_atoms.is_zero()
                || price.usd_micros_per_unit.is_zero()
            {
                return Err(changed());
            }
        }
    }
    for price in &fresh.prices {
        let original = approved
            .prices
            .iter()
            .find(|original| original.asset == price.asset)
            .ok_or_else(changed)?;
        if price.usd_micros_per_unit != original.usd_micros_per_unit
            || price.unit_atoms != original.unit_atoms
            || price.source != original.source
        {
            return Err(changed());
        }
    }
    Ok(())
}
fn exact(value: &str, decimals: u8) -> Result<U256> {
    let whole = value.split('.').next().unwrap_or("");
    require(
        value.len() <= 100 && !(whole.len() > 1 && whole.starts_with('0')),
        "INVALID_AMOUNT",
        "Use a canonical exact decimal amount without exponent, sign or leading zeros.",
    )?;
    fin_evm::u256(&Quantity::from_decimal(value, decimals)?)
}
fn add(a: U256, b: U256) -> Result<U256> {
    a.checked_add(b).ok_or_else(|| {
        error(
            "U256_OVERFLOW",
            "validation",
            "Exact addition exceeds uint256.",
        )
    })
}
fn sub(a: U256, b: U256) -> Result<U256> {
    a.checked_sub(b).ok_or_else(|| {
        error(
            "PROOF_MISMATCH",
            "reconciliation",
            "Exact subtraction would be negative.",
        )
    })
}
fn mul(a: U256, b: U256) -> Result<U256> {
    a.checked_mul(b).ok_or_else(|| {
        error(
            "U256_OVERFLOW",
            "validation",
            "Exact multiplication exceeds uint256.",
        )
    })
}
fn pow10(n: u8) -> Result<U256> {
    U256::from(10).checked_pow(U256::from(n)).ok_or_else(|| {
        error(
            "TOKEN_PRECISION_UNSUPPORTED",
            "read",
            "Token precision exceeds exact uint256 arithmetic.",
        )
    })
}
fn amount(asset: &AssetId, atoms: U256) -> Amount {
    Amount {
        asset: asset.clone(),
        atoms: fin_evm::quantity(atoms),
    }
}
fn resource(chain: u64, owner: Address) -> ResourceId {
    ResourceId(format!("ethereum:{chain}:{}", lower_address(owner)))
}
fn position(pool: &str, token: &str, kind: &str) -> String {
    format!("aave:{pool}:{token}:{kind}")
}
fn authority(token: &str, spender: &str) -> String {
    format!("erc20:{token}:allowance:{spender}")
}
fn collateral(pool: &str, token: &str) -> String {
    format!("aave:{pool}:{token}:collateral")
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UniswapConfig {
    pub factory: String,
    pub quoter: String,
    pub router: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AaveConfig {
    pub pool: String,
    pub oracle: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EthereumConfig {
    pub chain_id: u64,
    pub confirmations: u64,
    /// Require RPC finalized inclusion before terminal settlement. Omission
    /// preserves the confirmation-count policy of existing Ethereum configs.
    #[serde(default)]
    pub require_finalized: bool,
    #[serde(default)]
    pub weth: Option<String>,
    pub snapshot_tokens: Vec<String>,
    #[serde(default)]
    pub uniswap: Option<UniswapConfig>,
    #[serde(default)]
    pub aave: Option<AaveConfig>,
}
impl Default for EthereumConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            confirmations: 2,
            require_finalized: false,
            weth: Some(WETH.into()),
            snapshot_tokens: vec![WETH.into(), USDC.into()],
            uniswap: Some(UniswapConfig {
                factory: "0x1f98431c8ad98523631ae4a59f267346ea31f984".into(),
                quoter: "0x61ffe014ba17989e743c5f6cb21bf9697530b21e".into(),
                router: "0xe592427a0aece92de3edee1f18e0157c05861564".into(),
            }),
            aave: Some(AaveConfig {
                pool: "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2".into(),
                oracle: "0x54586be62e3c3580375ae3723c145253060ca0c2".into(),
            }),
        }
    }
}
impl EthereumConfig {
    /// Reviewed Arbitrum and Robinhood Chain operations settle at canonical
    /// sequencer inclusion rather than waiting for Ethereum L1 finality.
    pub const fn external_credit_uses_sequencer(chain_id: u64) -> bool {
        matches!(chain_id, 42161 | 4663)
    }

    pub fn wallet_only(chain_id: u64, confirmations: u64, snapshot_tokens: Vec<String>) -> Self {
        let sequencer_inclusion = Self::external_credit_uses_sequencer(chain_id);
        Self {
            chain_id,
            // Legacy protected configs may still say two confirmations. The
            // reviewed L2 wallet policy is one sequencer-produced block.
            confirmations: if sequencer_inclusion {
                1
            } else {
                confirmations
            },
            require_finalized: !sequencer_inclusion,
            snapshot_tokens,
            weth: None,
            uniswap: None,
            aave: None,
        }
    }
}

#[async_trait]
pub trait WalletValuation: Send + Sync {
    async fn quote_usd(&self, asset: &AssetId, units: Quantity) -> Result<(Quantity, String)>;
    /// Providers with an external observation timestamp override this method.
    /// The default supports isolated synthetic providers that observe on call.
    async fn quote_observation(
        &self,
        asset: &AssetId,
        units: Quantity,
    ) -> Result<(Quantity, String, String)> {
        let (value, source) = self.quote_usd(asset, units).await?;
        Ok((value, source, now()))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TokenDiscoveryResult {
    pub candidates: Vec<String>,
    pub complete: bool,
    pub source: String,
    pub warnings: Vec<String>,
}
#[async_trait]
pub trait TokenDiscovery: Send + Sync {
    async fn tokens(&self, chain_id: u64, address: &str) -> Result<TokenDiscoveryResult>;
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WalletConnection {
    pub name: String,
    pub address: String,
    pub signing_enabled: bool,
}

/// Protected-broker evidence from a fresh Binance deposit-address/network read.
/// This is an internal composition seam, never an agent operation or request
/// field. The broker must authenticate the account and hash its full observed
/// eligibility evidence before constructing this target.
/// The historical type name and serialized fields are retained for recovery;
/// generalized deposits also carry a separately verified exact source asset.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerVerifiedNativeDepositTarget {
    pub chain_id: u64,
    pub account_resource: ResourceId,
    pub address: String,
    /// Actual venue holding identity with 36 atomic decimals.
    pub asset: AssetId,
    /// Usable-credit minimum in the venue asset's 36-decimal atomic units.
    pub minimum_deposit: Quantity,
    pub network: String,
    pub observed_at: String,
    pub expires_at: String,
    pub evidence_digest: String,
}

#[async_trait]
pub trait OracleObservationClock: Send + Sync {
    async fn unix_seconds(&self) -> Result<u64>;
}
struct SystemOracleClock;
#[async_trait]
impl OracleObservationClock for SystemOracleClock {
    async fn unix_seconds(&self) -> Result<u64> {
        chrono::Utc::now().timestamp().try_into().map_err(|_| {
            error(
                "CLOCK_INVALID",
                "valuation",
                "System observation clock is invalid.",
            )
        })
    }
}
pub struct EthereumAdapter {
    pub config: EthereumConfig,
    pub wallets: Vec<WalletConnection>,
    pub runtime: Arc<EvmRuntime>,
    oracle_clock: Arc<dyn OracleObservationClock>,
    wallet_valuation: Option<Arc<dyn WalletValuation>>,
    token_discovery: Option<Arc<dyn TokenDiscovery>>,
}
impl EthereumAdapter {
    /// Internal dependency injection for a guarded isolated fork. Production
    /// broker composition always calls new(), whose clock is fixed system UTC.
    /// This affects oracle heartbeat observation only, never authorization expiry.
    #[doc(hidden)]
    pub fn with_isolated_oracle_clock(mut self, clock: Arc<dyn OracleObservationClock>) -> Self {
        self.oracle_clock = clock;
        self
    }

    pub fn new(
        mut config: EthereumConfig,
        mut wallets: Vec<WalletConnection>,
        runtime: Arc<EvmRuntime>,
    ) -> Result<Self> {
        require(
            config.chain_id > 0 && config.chain_id == runtime.chain_id && config.confirmations > 0,
            "CHAIN_CONFIGURATION_INVALID",
            "Runtime chain and configured chain/finality must agree.",
        )?;
        let mut deployments = Vec::new();
        if let Some(weth) = &mut config.weth {
            deployments.push(weth);
        }
        if let Some(uniswap) = &mut config.uniswap {
            deployments.extend([
                &mut uniswap.factory,
                &mut uniswap.quoter,
                &mut uniswap.router,
            ]);
        }
        if let Some(aave) = &mut config.aave {
            deployments.extend([&mut aave.pool, &mut aave.oracle]);
        }
        for value in deployments {
            let a = address(value)?;
            require(
                a != Address::ZERO,
                "ZERO_ADDRESS",
                "Deployment addresses must be nonzero.",
            )?;
            *value = lower_address(a);
        }
        require(
            config.snapshot_tokens.len() <= 64,
            "SNAPSHOT_SCOPE_INVALID",
            "Configure a bounded explicit token holding scope.",
        )?;
        let mut tokens = BTreeSet::new();
        for t in &mut config.snapshot_tokens {
            *t = lower_address(address(t)?);
            require(
                tokens.insert(t.clone()),
                "DUPLICATE_TOKEN",
                "Duplicate snapshot token identity.",
            )?;
        }
        let mut names = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for w in &mut wallets {
            require(
                !w.name.is_empty() && w.name.len() <= 80 && names.insert(w.name.clone()),
                "DUPLICATE_WALLET",
                "Wallet names must be unique and nonempty.",
            )?;
            let a = address(&w.address)?;
            require(
                a != Address::ZERO && identities.insert(a),
                "DUPLICATE_RESOURCE",
                "Configured wallet aliases must not duplicate physical custody.",
            )?;
            w.address = lower_address(a);
        }
        require(
            !wallets.is_empty(),
            "WALLET_REQUIRED",
            "At least one named wallet is required.",
        )?;
        Ok(Self {
            config,
            wallets,
            runtime,
            oracle_clock: Arc::new(SystemOracleClock),
            wallet_valuation: None,
            token_discovery: None,
        })
    }
    pub fn with_wallet_valuation(mut self, provider: Arc<dyn WalletValuation>) -> Self {
        self.wallet_valuation = Some(provider);
        self
    }
    pub fn with_token_discovery(mut self, provider: Arc<dyn TokenDiscovery>) -> Self {
        self.token_discovery = Some(provider);
        self
    }
    pub fn supports_operation(&self, operation: &str) -> bool {
        match operation {
            "wallet.balances" | "wallet.token" | "wallet.allowance" | "wallet.receipt"
            | "wallet.transfer" => true,
            "wallet.wrap" | "wallet.unwrap" => self.config.weth.is_some(),
            "wallet.approve" => self.config.aave.is_some() || self.config.uniswap.is_some(),
            name if name.starts_with("aave.") => self.config.aave.is_some(),
            name if name.starts_with("uniswap.") => self.config.uniswap.is_some(),
            _ => false,
        }
    }
    fn require_operation(&self, operation: &str) -> Result<()> {
        require(
            self.supports_operation(operation),
            "OPERATION_UNAVAILABLE",
            "This operation has no configured deployment on the selected network.",
        )
    }
    fn aave(&self) -> Result<&AaveConfig> {
        self.config.aave.as_ref().ok_or_else(|| {
            error(
                "OPERATION_UNAVAILABLE",
                "validation",
                "Aave is not configured on this network.",
            )
        })
    }
    fn uniswap(&self) -> Result<&UniswapConfig> {
        self.config.uniswap.as_ref().ok_or_else(|| {
            error(
                "OPERATION_UNAVAILABLE",
                "validation",
                "Uniswap is not configured on this network.",
            )
        })
    }
    fn weth(&self) -> Result<&str> {
        self.config.weth.as_deref().ok_or_else(|| {
            error(
                "OPERATION_UNAVAILABLE",
                "validation",
                "Wrapped native coin is not configured on this network.",
            )
        })
    }
    fn wallet(&self, name: &str) -> Result<&WalletConnection> {
        self.wallets.iter().find(|w| w.name == name).ok_or_else(|| {
            error(
                "WALLET_UNKNOWN",
                "validation",
                "Named wallet is not configured.",
            )
        })
    }
    fn resolve(&self, r: &request::Request, write: bool) -> Result<&WalletConnection> {
        require(
            r.chain_id == self.config.chain_id,
            "WRONG_CHAIN",
            "Request chain differs from configured immutable chain.",
        )?;
        let w = self.wallet(&r.wallet)?;
        require(
            !write || w.signing_enabled,
            "PROTECTED_SIGNER_REQUIRED",
            "This named wallet has no enabled protected signing authority.",
        )?;
        Ok(w)
    }
    fn asset(&self, token: &str, decimals: u8) -> AssetId {
        AssetId {
            network: format!("ethereum:{}", self.config.chain_id),
            reference: token.into(),
            decimals,
        }
    }
    fn native_asset(&self) -> AssetId {
        self.asset("native", 18)
    }
    fn metadata(&self) -> Vec<ConnectionMetadata> {
        self.wallets.iter().map(|w|ConnectionMetadata{name:w.name.clone(),provider:"evm".into(),resource:resource(self.config.chain_id,address(&w.address).expect("validated")),role:None,parent:None,network:Some(format!("ethereum:{}",self.config.chain_id)),address:Some(w.address.clone()),credential_ref:None,signing_enabled:w.signing_enabled,details:json!({"chain_id":self.config.chain_id,"confirmations":self.config.confirmations})}).collect()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    pub transaction: FrozenTransaction,
    pub resource: ResourceId,
    pub token: AssetId,
    pub units: Quantity,
    pub recipient: Option<String>,
    pub spender: Option<String>,
    pub output: Option<AssetId>,
    pub minimum_output: Option<Quantity>,
    pub deadline: Option<u64>,
    pub pool: Option<String>,
    pub fee_tier: Option<u32>,
    pub allowance_before: Option<Quantity>,
    pub supplied_before: Option<Quantity>,
    pub debt_before: Option<Quantity>,
    pub collateral_before: Option<bool>,
    pub enabled: Option<bool>,
    pub a_token: Option<String>,
    pub debt_token: Option<String>,
    pub scaled_before: Option<Quantity>,
    pub repay_all: bool,
    pub all_amount: bool,
    pub supply_anchor: Option<PositionAnchor>,
    pub debt_anchor: Option<PositionAnchor>,
    pub debt_rounding_maximum: Option<Quantity>,
    #[serde(
        default,
        rename = "_fin_usd_ceiling_v1",
        skip_serializing_if = "Option::is_none"
    )]
    pub value_bounds: Option<Value>,
}

#[async_trait]
impl Adapter for EthereumAdapter {
    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
            .into_iter()
            .filter(|definition| self.supports_operation(&definition.name))
            .map(|mut definition| {
                for example in &mut definition.examples { example["chain_id"] = json!(self.config.chain_id); }
                if !self.config.require_finalized && self.config.confirmations == 1 && EthereumConfig::external_credit_uses_sequencer(self.config.chain_id) {
                    definition.completion = "Writes require the original canonical envelope and receipt, one sequencer-produced L2 block, bounded actual fees and verified transfer effects; no RPC finalized-tag wait is required.".into();
                } else if self.config.require_finalized {
                    definition.completion = "Writes require the original canonical envelope and receipt, bounded actual fees, verified transfer effects, and RPC finalized inclusion; sequencer confirmation alone remains pending.".into();
                }
                definition
            })
            .collect()
    }
    fn connections(&self) -> Vec<ConnectionMetadata> {
        self.metadata()
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.read_snapshot().await
    }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        self.read_operation(operation, input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        let mut prepared = self
            .prepare_operation(operation, input, None, &PreparationContext::default())
            .await?;
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        let mut prepared = self
            .prepare_operation(operation, input, None, context)
            .await?;
        fin_domain::value_bounds::bind(&mut prepared);
        Ok(prepared)
    }
    async fn preflight_return_route(
        &self,
        operation: &str,
        input: &Value,
        context: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof> {
        self.preflight_route(operation, input, context).await
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        let original: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "BINDING_INVALID",
                "submission",
                "Original Ethereum binding is malformed.",
            )
        })?;
        require(
            prepared.expires_at > now(),
            "PREVIEW_EXPIRED",
            "Original preview has expired.",
        )?;
        let context = PreparationContext {
            positions: [original.supply_anchor.clone(), original.debt_anchor.clone()]
                .into_iter()
                .flatten()
                .collect(),
        };
        let mut refreshed = self
            .prepare_operation(
                &prepared.operation,
                &prepared.input,
                Some(&original),
                &context,
            )
            .await?;
        fin_domain::value_bounds::carry(prepared, &mut refreshed)?;
        Ok(refreshed)
    }
    async fn submit(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let fresh = self.revalidate(prepared).await?;
        require(
            fresh.binding == prepared.binding,
            "BINDING_CHANGED",
            "Current executable fields differ from approved envelope.",
        )?;
        if prepared
            .binding
            .get(fin_domain::value_bounds::KEY)
            .is_some()
        {
            fin_domain::value_bounds::validate(&fresh)?;
        } else {
            require_frozen_valuation(fresh.valuation.as_ref(), prepared.valuation.as_ref())?;
        }
        let binding: Binding = serde_json::from_value(prepared.binding.clone()).map_err(|_| {
            error(
                "BINDING_INVALID",
                "submission",
                "Original binding is malformed.",
            )
        })?;
        self.runtime
            .submit(
                &binding.transaction,
                json!({"operation":prepared.operation,"resource":binding.resource}),
                journal,
            )
            .await
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        self.reconcile_operation(record).await
    }
}
