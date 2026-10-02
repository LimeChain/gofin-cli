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
    pub weth: String,
    pub snapshot_tokens: Vec<String>,
    pub uniswap: UniswapConfig,
    pub aave: AaveConfig,
}
impl Default for EthereumConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            confirmations: 2,
            weth: WETH.into(),
            snapshot_tokens: vec![WETH.into(), USDC.into()],
            uniswap: UniswapConfig {
                factory: "0x1f98431c8ad98523631ae4a59f267346ea31f984".into(),
                quoter: "0x61ffe014ba17989e743c5f6cb21bf9697530b21e".into(),
                router: "0xe592427a0aece92de3edee1f18e0157c05861564".into(),
            },
            aave: AaveConfig {
                pool: "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2".into(),
                oracle: "0x54586be62e3c3580375ae3723c145253060ca0c2".into(),
            },
        }
    }
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerVerifiedNativeDepositTarget {
    pub chain_id: u64,
    pub account_resource: ResourceId,
    pub address: String,
    /// Actual venue holding identity: binance/ETH with 36 atomic decimals.
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
        for value in [
            &mut config.weth,
            &mut config.uniswap.factory,
            &mut config.uniswap.quoter,
            &mut config.uniswap.router,
            &mut config.aave.pool,
            &mut config.aave.oracle,
        ] {
            let a = address(value)?;
            require(
                a != Address::ZERO,
                "ZERO_ADDRESS",
                "Deployment addresses must be nonzero.",
            )?;
            *value = lower_address(a);
        }
        require(
            !config.snapshot_tokens.is_empty() && config.snapshot_tokens.len() <= 64,
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
        self.wallets.iter().map(|w|ConnectionMetadata{name:w.name.clone(),provider:"ethereum".into(),resource:resource(self.config.chain_id,address(&w.address).expect("validated")),role:None,parent:None,network:Some(format!("ethereum:{}",self.config.chain_id)),address:Some(w.address.clone()),credential_ref:None,signing_enabled:w.signing_enabled,details:json!({"chain_id":self.config.chain_id,"confirmations":self.config.confirmations})}).collect()
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
}

#[async_trait]
impl Adapter for EthereumAdapter {
    fn definitions(&self) -> Vec<Definition> {
        operation_definitions()
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
        self.prepare_operation(operation, input, None, &PreparationContext::default())
            .await
    }
    async fn prepare_with_context(
        &self,
        operation: &str,
        input: &Value,
        context: &PreparationContext,
    ) -> Result<PreparedOperation> {
        self.prepare_operation(operation, input, None, context)
            .await
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
        self.prepare_operation(
            &prepared.operation,
            &prepared.input,
            Some(&original),
            &context,
        )
        .await
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
