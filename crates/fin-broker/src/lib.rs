//! Protected composition and local protocol endpoint for Fin.
//!
//! Financial callers can access only typed protocol methods. Production startup
//! is rooted at the installed paths; the separately named composition entry
//! point exists for synthetic adapter/credential harnesses.

pub mod deposit;
pub mod enrollment;
mod evm_public_data;
mod evm_router;
mod policy;
mod pricing;
mod pricing_cache;
pub use deposit::BinanceEthereumDepositAdapter;
pub use pricing::SharedUsdValuator;

use async_trait::async_trait;
use chrono::Utc;
use fin_authorization::{MacOsAuthorizer, validate_installation, validate_metadata};
use fin_domain::{
    Adapter, Amount, AssetId, AssetPrice, Authorizer, Definition, ExecutionActor, FinError,
    FinancialState, OperationKind, OperationRecord, PreparedOperation, Reconciliation, ResourceId,
    StateSnapshot, Submission, SubmissionJournal, now,
};
use fin_engine::{Engine, EngineOptions};
#[cfg(test)]
use fin_engine::{RunAllocation, RunPlan, RunResult};
#[cfg(test)]
use fin_protocol::RunBeginRequest;
use fin_protocol::{BrokerRequest, BrokerResponse, encode_response, read_request};
use fin_store::Store;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

pub const CONFIG_PATH: &str = "/Library/Fin/private/config.json";
pub const OPERATIONS_PATH: &str = "/Library/Fin/private/operations.sqlite";
pub const SOCKET_PATH: &str = "/Library/Fin/run/broker.sock";
pub const MAINTENANCE_LOCK_PATH: &str = "/Library/Fin/private/maintenance.lock";
pub const MAX_CONFIG_BYTES: usize = 1_048_576;
const IPC_FRAME_READ_TIMEOUT: Duration = Duration::from_secs(10);
const IPC_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const IPC_CONFIGURATION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

fn error(code: &str, stage: &str, message: &str, action: &str) -> FinError {
    FinError::new(code, stage, message, action)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfigV2 {
    pub version: u32,
    pub networks: Vec<NetworkConfig>,
    pub deployments: Vec<DeploymentConfig>,
    pub connections: Vec<ConnectionConfig>,
    pub policy: BrokerPolicy,
    pub approver_uid: u32,
    #[serde(default)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    pub id: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub confirmations: u64,
    #[serde(default)]
    pub snapshot_tokens: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    pub network: String,
    pub weth: String,
    pub snapshot_tokens: Vec<String>,
    pub uniswap: fin_ethereum::UniswapConfig,
    pub aave: fin_ethereum::AaveConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectionConfig {
    Binance {
        name: String,
        resource: ResourceId,
        credential_ref: String,
        role: String,
        parent: Option<String>,
        email: Option<String>,
        wallet_type: String,
        /// UID observed from the fixed signed `/api/v3/account` endpoint and
        /// matched to the canonical SPOT resource identity.
        physical_identity: String,
        physical_identity_verified: bool,
    },
    /// Legacy chain-scoped connection, retained for journals and old installations.
    Ethereum {
        name: String,
        resource: ResourceId,
        credential_ref: Option<String>,
        network: String,
        address: String,
        signing_enabled: bool,
        assets: Vec<AssetId>,
    },
    /// One protected signer/address, reusable across the configured EVM networks.
    Evm {
        name: String,
        resource: ResourceId,
        credential_ref: Option<String>,
        address: String,
        signing_enabled: bool,
    },
    #[serde(rename = "coingecko")]
    CoinGecko {
        name: String,
        resource: ResourceId,
        credential_ref: String,
        api_plan: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrokerPolicy {
    /// Legacy serialized setting, retained for configuration compatibility; ignored at runtime.
    #[serde(default)]
    pub require_run_for_writes: bool,
    pub max_movement_usd_micros: String,
    pub max_run_allocation_usd_micros: String,
    pub preview_ttl_seconds: u64,
    /// Human-approved exception; absent legacy configuration remains disabled.
    #[serde(default)]
    pub allow_unpriced_exchange_assets: bool,
}

impl NetworkConfig {
    pub fn ethereum_mainnet() -> Self {
        Self {
            id: "ethereum:1".into(),
            chain_id: 1,
            rpc_urls: vec![
                "https://ethereum-rpc.publicnode.com".into(),
                "https://eth.drpc.org".into(),
            ],
            confirmations: 2,
            snapshot_tokens: vec![fin_ethereum::WETH.into(), fin_ethereum::USDC.into()],
        }
    }
    pub fn reviewed_evm_networks() -> Vec<Self> {
        vec![
            Self::ethereum_mainnet(),
            Self {
                id: "ethereum:42161".into(),
                chain_id: 42161,
                rpc_urls: vec!["https://arb1.arbitrum.io/rpc".into()],
                confirmations: 2,
                snapshot_tokens: vec![
                    "0x82af49447d8a07e3bd95bd0d56f35241523fbab1".into(),
                    "0xaf88d065e77c8cc2239327c5edb3a432268e5831".into(),
                ],
            },
            Self {
                id: "ethereum:4663".into(),
                chain_id: 4663,
                rpc_urls: vec!["https://rpc.mainnet.chain.robinhood.com".into()],
                confirmations: 2,
                snapshot_tokens: vec![
                    "0x0bd7d308f8e1639fab988df18a8011f41eacad73".into(),
                    "0x5fc5360d0400a0fd4f2af552add042d716f1d168".into(),
                ],
            },
        ]
    }
}

impl DeploymentConfig {
    pub fn ethereum_mainnet() -> Self {
        let defaults = fin_ethereum::EthereumConfig::default();
        Self {
            network: "ethereum:1".into(),
            weth: defaults.weth.expect("mainnet WETH"),
            snapshot_tokens: defaults.snapshot_tokens,
            uniswap: defaults.uniswap.expect("mainnet Uniswap"),
            aave: defaults.aave.expect("mainnet Aave"),
        }
    }
}

impl BrokerPolicy {
    pub fn beta() -> Self {
        Self {
            require_run_for_writes: false,
            max_movement_usd_micros: fin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS
                .to_string(),
            max_run_allocation_usd_micros: fin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS
                .to_string(),
            preview_ttl_seconds: 90,
            allow_unpriced_exchange_assets: false,
        }
    }
}

impl BrokerConfigV2 {
    pub fn validate(&self) -> fin_domain::Result<()> {
        if self.version != 2 || self.approver_uid < 501 {
            return Err(error(
                "CONFIG_VERSION_OR_APPROVER_INVALID",
                "configuration",
                "Configuration version 2 and a human approver UID are required.",
                "Use the protected human enrollment flow to repair broker configuration.",
            ));
        }
        if self.networks.is_empty() {
            return Err(error(
                "CONFIG_NETWORKS_REQUIRED",
                "configuration",
                "At least one reviewed EVM network is required.",
                "Configure a supported Ethereum network and reviewed deployments.",
            ));
        }
        if self.policy.preview_ttl_seconds == 0 || self.policy.preview_ttl_seconds > 90 {
            return Err(error(
                "CONFIG_POLICY_INVALID",
                "configuration",
                "Preview lifetime must be between 1 and 90 seconds.",
                "Repair the policy through protected human enrollment.",
            ));
        }
        let mut networks = BTreeSet::new();
        let mut chain_ids = BTreeSet::new();
        for network in &self.networks {
            if network.id.is_empty()
                || network.chain_id == 0
                || network.confirmations == 0
                || network.rpc_urls.is_empty()
                || !networks.insert(network.id.clone())
                || network.id != format!("ethereum:{}", network.chain_id)
                || !chain_ids.insert(network.chain_id)
                || ![1, 42161, 4663].contains(&network.chain_id)
                || network.snapshot_tokens.len() > 64
                || network
                    .snapshot_tokens
                    .iter()
                    .any(|token| !valid_eth_address(token))
                || network.rpc_urls.iter().any(|url| !valid_public_rpc(url))
            {
                return Err(error(
                    "CONFIG_NETWORK_INVALID",
                    "configuration",
                    "Network identities, HTTPS RPC endpoints, chain IDs and confirmations must be unique and valid.",
                    "Use public HTTPS RPC endpoints without embedded credentials, query strings or fragments.",
                ));
            }
        }
        let mut deployments = BTreeSet::new();
        for deployment in &self.deployments {
            if !networks.contains(&deployment.network)
                || !deployments.insert(deployment.network.clone())
            {
                return Err(error(
                    "CONFIG_DEPLOYMENT_INVALID",
                    "configuration",
                    "Each configured deployment must target a unique configured network.",
                    "Repair network and deployment references through protected human enrollment.",
                ));
            }
        }
        let mut names = BTreeSet::new();
        let mut resources = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for connection in &self.connections {
            let (name, resource) = match connection {
                ConnectionConfig::CoinGecko {
                    name,
                    resource,
                    credential_ref,
                    api_plan,
                } => {
                    if resource.0 != "market-data:coingecko"
                        || !matches!(api_plan.as_str(), "demo" | "pro")
                        || fin_credentials::CredentialRef::new(credential_ref).is_err()
                    {
                        return Err(error(
                            "CONFIG_PRICING_CONNECTION_INVALID",
                            "configuration",
                            "CoinGecko API plan or protected credential mapping is invalid.",
                            "Repair pricing through protected connection enrollment.",
                        ));
                    }
                    (name, resource)
                }
                ConnectionConfig::Binance {
                    name,
                    resource,
                    credential_ref,
                    role,
                    parent,
                    email,
                    wallet_type,
                    physical_identity,
                    physical_identity_verified,
                    ..
                } => {
                    if wallet_type != "SPOT"
                        || !matches!(role.as_str(), "main" | "sub")
                        || (*role == "main" && (parent.is_some() || email.is_some()))
                        || (*role == "sub"
                            && (parent.is_none() || email.as_deref().unwrap_or("").is_empty()))
                        || !canonical_uid(physical_identity)
                        || !physical_identity_verified
                        || resource.0 != format!("binance:uid:{physical_identity}:SPOT")
                        || fin_credentials::CredentialRef::new(credential_ref).is_err()
                    {
                        return Err(error(
                            "CONFIG_BINANCE_CONNECTION_INVALID",
                            "configuration",
                            "Binance SPOT connection hierarchy or verified physical identity is invalid.",
                            "Verify the account mapping and enroll the main key through the human terminal.",
                        ));
                    }
                    (name, resource)
                }
                ConnectionConfig::Ethereum {
                    name,
                    resource,
                    network,
                    address,
                    signing_enabled,
                    credential_ref,
                    assets,
                } => {
                    if !networks.contains(network)
                        || !valid_eth_address(address)
                        || (*signing_enabled != credential_ref.is_some())
                        || assets.is_empty()
                        || credential_ref.as_ref().is_some_and(|reference| {
                            fin_credentials::CredentialRef::new(reference).is_err()
                        })
                    {
                        return Err(error(
                            "CONFIG_ETHEREUM_CONNECTION_INVALID",
                            "configuration",
                            "Wallet network, address, signing credential or asset scope is invalid.",
                            "Repair the wallet mapping through protected human enrollment.",
                        ));
                    }
                    (name, resource)
                }
                ConnectionConfig::Evm {
                    name,
                    resource,
                    address,
                    signing_enabled,
                    credential_ref,
                } => {
                    if !valid_eth_address(address)
                        || resource.0 != format!("evm:{}", address.to_ascii_lowercase())
                        || (*signing_enabled != credential_ref.is_some())
                        || credential_ref.as_ref().is_some_and(|value| {
                            fin_credentials::CredentialRef::new(value).is_err()
                        })
                    {
                        return Err(error(
                            "CONFIG_EVM_CONNECTION_INVALID",
                            "configuration",
                            "EVM address or protected signer mapping is invalid.",
                            "Repair the wallet through protected enrollment.",
                        ));
                    }
                    (name, resource)
                }
            };
            if name.is_empty() || !names.insert(name.clone()) || !resources.insert(resource.clone())
            {
                return Err(error(
                    "CONFIG_CONNECTION_DUPLICATE",
                    "configuration",
                    "Connection names and physical resources must be unique.",
                    "Remove duplicate aliases or physical account mappings.",
                ));
            }
            if let ConnectionConfig::Binance { role, parent: Some(parent), .. } = connection
                && role == "sub"
                && !self.connections.iter().any(|candidate| matches!(candidate, ConnectionConfig::Binance { name, role, .. } if name == parent && role == "main"))
            {
                return Err(error("CONFIG_BINANCE_PARENT_MISSING", "configuration", "A Binance subaccount must refer to a configured main account.", "Enroll the main connection before adding its subaccount."));
            }
            if let ConnectionConfig::Binance {
                physical_identity, ..
            } = connection
                && !identities.insert(physical_identity)
            {
                return Err(error(
                    "CONFIG_DUPLICATE_PHYSICAL_ACCOUNT",
                    "configuration",
                    "A Binance physical account cannot be registered twice under distinct aliases.",
                    "Remove duplicate main-key mappings.",
                ));
            }
        }
        let mut physical_wallets = BTreeSet::new();
        for connection in &self.connections {
            let (address, chains) = match connection {
                ConnectionConfig::Evm { address, .. } => (
                    address,
                    self.networks
                        .iter()
                        .map(|network| network.chain_id)
                        .collect::<Vec<_>>(),
                ),
                ConnectionConfig::Ethereum {
                    address, network, ..
                } => (
                    address,
                    vec![
                        self.networks
                            .iter()
                            .find(|profile| &profile.id == network)
                            .expect("validated network")
                            .chain_id,
                    ],
                ),
                _ => continue,
            };
            for chain in chains {
                if !physical_wallets.insert((chain, address.to_ascii_lowercase())) {
                    return Err(error(
                        "CONFIG_DUPLICATE_PHYSICAL_WALLET",
                        "configuration",
                        "The same EVM wallet is registered more than once on a network.",
                        "Reuse the existing EVM connection instead of creating an alias.",
                    ));
                }
            }
        }
        for value in [
            &self.policy.max_movement_usd_micros,
            &self.policy.max_run_allocation_usd_micros,
        ] {
            if fin_domain::policy::policy_micros(value).is_err() {
                return Err(error(
                    "CONFIG_POLICY_INVALID",
                    "configuration",
                    "Financial policy bounds must be unsigned 64-bit integer USD micros; zero means no personal limit.",
                    "Repair policy bounds through protected human enrollment.",
                ));
            }
        }
        Ok(())
    }
}

/// Static operation catalog. It is safe to call without configuration,
/// credentials, network access, root privileges, or an initialized store.
pub fn operation_definitions() -> Vec<Definition> {
    let mut definitions = fin_binance::operation_definitions();
    definitions.extend(fin_exchange::DefaultExchangeProfile::definitions());
    definitions.extend(fin_ethereum::operation_definitions());
    definitions.push(BinanceEthereumDepositAdapter::operation_definition());
    definitions.push(snapshot_definition());
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    definitions
}

fn snapshot_definition() -> Definition {
    Definition {
        name: "fin.snapshot".into(),
        description: "Capture a broker-combined financial snapshot for every configured resource and unresolved journal identity.".into(),
        kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable,
        input_schema: json!({"type":"object","additionalProperties":false}),
        output_schema: json!({"type":"object","required":["state","observed_at","complete","coverage","pending"]}),
        error_schema: json!({"$ref":"FinError"}),
        examples: vec![json!({"input":{}})],
        coverage: "Combined snapshots from all configured reviewed adapters and nonterminal durable operations.".into(),
        completion: "complete is true only when all observers completed and the operation journal has no unresolved records.".into(),
        semantics: json!({"clientSuppliedState":false,"scope":"all-configured-resources","pendingJournalIncluded":true}),
    }
}

fn valid_public_rpc(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn valid_eth_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn canonical_uid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && !value.starts_with('0')
        && value.parse::<u64>().is_ok_and(|uid| uid > 0)
}

pub trait BrokerCredentialProvider: fin_exchange::ccxt::CredentialProvider + Send + Sync {
    fn usd_price_source(
        &self,
        _credential: &fin_credentials::CredentialRef,
        _plan: &str,
    ) -> fin_domain::Result<Arc<dyn fin_pricing::UsdPriceSource>> {
        Err(error(
            "PRICING_CREDENTIAL_UNAVAILABLE",
            "configuration",
            "This credential provider has no protected pricing connection.",
            "Enroll the CoinGecko key in your own Terminal.",
        ))
    }
    fn envelope_signer(
        &self,
        credential: &fin_credentials::CredentialRef,
    ) -> fin_domain::Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)>;
}

/// Root-owned credentials are supplied only to protected signing components.
/// CCXT holds transient in-process copies; no private bytes are logged or
/// returned through the public protocol.
pub struct ProtectedCredentialProvider {
    store: Arc<fin_credentials::TrustedCredentialStore>,
}

impl ProtectedCredentialProvider {
    pub fn open() -> fin_domain::Result<Self> {
        Ok(Self {
            store: Arc::new(fin_credentials::TrustedCredentialStore::open()?),
        })
    }

    pub fn from_store(store: Arc<fin_credentials::TrustedCredentialStore>) -> Self {
        Self { store }
    }
}

impl fin_exchange::ccxt::CredentialProvider for ProtectedCredentialProvider {
    fn credentials(
        &self,
        reference: &str,
    ) -> Result<fin_exchange::ccxt::Credentials, fin_exchange::ccxt::CcxtError> {
        use fin_exchange::ccxt::{CcxtError, CcxtErrorKind, Credentials};
        let denied = || CcxtError::new(CcxtErrorKind::PermissionDenied);
        let reference = fin_credentials::CredentialRef::new(reference).map_err(|_| denied())?;
        self.store
            .with_material(&reference, |material| {
                let (api_key, secret) = match material {
                    fin_credentials::SecretMaterial::BinanceHmac { api_key, secret } => {
                        (api_key, secret)
                    }
                    fin_credentials::SecretMaterial::BinanceRsa {
                        api_key,
                        private_key,
                    }
                    | fin_credentials::SecretMaterial::BinanceEd25519 {
                        api_key,
                        private_key,
                    } => (api_key, private_key),
                    _ => {
                        return Err(FinError::validation(
                            "Credential is not an exchange API key.",
                        ));
                    }
                };
                Ok(Credentials {
                    api_key: zeroize::Zeroizing::new(api_key.clone()),
                    secret: zeroize::Zeroizing::new(secret.clone()),
                })
            })
            .map_err(|_| denied())
    }
}

impl BrokerCredentialProvider for ProtectedCredentialProvider {
    fn usd_price_source(
        &self,
        credential: &fin_credentials::CredentialRef,
        plan: &str,
    ) -> fin_domain::Result<Arc<dyn fin_pricing::UsdPriceSource>> {
        self.store
            .with_material(credential, |material| match material {
                fin_credentials::SecretMaterial::CoinGeckoApiKey {
                    api_key,
                    plan: stored_plan,
                } if stored_plan == plan => {
                    let plan = match plan {
                        "demo" => fin_pricing::ApiPlan::Demo,
                        "pro" => fin_pricing::ApiPlan::Pro,
                        _ => return Err(FinError::validation("The CoinGecko plan is invalid.")),
                    };
                    Ok(Arc::new(fin_pricing::ExchangeSpotPrices::new(
                        pricing_cache::attach(fin_pricing::CoinGeckoPrices::with_api_key(
                            plan, api_key,
                        )?)?,
                    )?)
                        as Arc<dyn fin_pricing::UsdPriceSource>)
                }
                _ => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "credentials",
                    "Credential is not the configured CoinGecko API key.",
                    "Repair pricing through protected enrollment.",
                )),
            })
    }

    fn envelope_signer(
        &self,
        credential: &fin_credentials::CredentialRef,
    ) -> fin_domain::Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)> {
        self.store
            .with_material(credential, |material| match material {
                fin_credentials::SecretMaterial::EvmPrivateKey { private_key } => {
                    let signer =
                        Arc::new(fin_evm::LocalEnvelopeSigner::from_private_key(private_key)?);
                    let address = format!("{:#x}", signer.address());
                    Ok((address, signer as Arc<dyn fin_evm::EnvelopeSigner>))
                }
                fin_credentials::SecretMaterial::BinanceHmac { .. }
                | fin_credentials::SecretMaterial::BinanceRsa { .. }
                | fin_credentials::SecretMaterial::BinanceEd25519 { .. }
                | fin_credentials::SecretMaterial::CoinGeckoApiKey { .. } => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "credentials",
                    "Credential is not an Ethereum wallet key.",
                    "Select the enrolled Ethereum signer reference.",
                )),
            })
    }
}

/// Ethereum runtime has one signer seam; this map routes only an already
/// frozen sender to its matching protected credential signer.
pub struct MappedEnvelopeSigner {
    signers: BTreeMap<String, Arc<dyn fin_evm::EnvelopeSigner>>,
}

impl MappedEnvelopeSigner {
    pub fn new(
        signers: BTreeMap<String, Arc<dyn fin_evm::EnvelopeSigner>>,
    ) -> fin_domain::Result<Self> {
        if signers.is_empty() || signers.keys().any(|address| !valid_eth_address(address)) {
            return Err(error(
                "SIGNER_MAP_INVALID",
                "credentials",
                "Ethereum signer map requires valid configured sender addresses.",
                "Enroll each protected sender mapping explicitly.",
            ));
        }
        let normalized = signers
            .into_iter()
            .map(|(address, signer)| (address.to_ascii_lowercase(), signer))
            .collect();
        Ok(Self {
            signers: normalized,
        })
    }
}

#[async_trait]
impl fin_evm::EnvelopeSigner for MappedEnvelopeSigner {
    async fn sign_prepared(
        &self,
        transaction: &fin_evm::FrozenTransaction,
    ) -> fin_domain::Result<fin_evm::alloy::primitives::Bytes> {
        let signer = self
            .signers
            .get(&transaction.sender.to_ascii_lowercase())
            .ok_or_else(|| {
                error(
                    "SIGNER_MISMATCH",
                    "signing",
                    "No protected signer is mapped to the frozen Ethereum sender.",
                    "Use an enrolled wallet with the exact configured physical address.",
                )
            })?;
        signer.sign_prepared(transaction).await
    }
}

#[async_trait]
pub trait AssetValuator: Send + Sync {
    async fn usd_micros(&self, amount: &Amount) -> fin_domain::Result<(String, String, String)>;

    async fn asset_price(&self, asset: &AssetId) -> fin_domain::Result<AssetPrice> {
        if asset.decimals > 36 {
            return Err(error(
                "PRICE_ASSET_UNSUPPORTED",
                "valuation",
                "Asset precision exceeds the reviewed valuation boundary.",
                "Use an asset with configured exact decimals.",
            ));
        }
        let unit_atoms = pow10(asset.decimals as u32);
        let amount = Amount {
            asset: asset.clone(),
            atoms: fin_domain::Quantity::new(unit_atoms.to_string())?,
        };
        let (usd_micros_per_unit, observed_at, source) = self.usd_micros(&amount).await?;
        Ok(AssetPrice {
            asset: asset.clone(),
            usd_micros_per_unit: fin_domain::Quantity::new(usd_micros_per_unit)?,
            unit_atoms: fin_domain::Quantity::new(unit_atoms.to_string())?,
            observed_at,
            source,
        })
    }
}

pub struct EthereumPayoutVerifier {
    adapters: BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>,
}

pub struct ProtectedApplications {
    pub adapters: Vec<Arc<dyn Adapter>>,
    pub valuator: Arc<dyn AssetValuator>,
}

pub fn build_protected_applications(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
) -> fin_domain::Result<ProtectedApplications> {
    config.validate()?;
    let mut eth_rpcs = BTreeMap::<u64, Arc<dyn fin_evm::EvmRpc>>::new();
    for network in &config.networks {
        let rpc = Arc::new(fin_evm::HttpRpc::new(
            network.rpc_urls.clone(),
            network.chain_id,
        )?);
        eth_rpcs.insert(network.chain_id, rpc.clone());
    }
    let source: Arc<dyn fin_pricing::UsdPriceSource> = if let Some(ConnectionConfig::CoinGecko {
        credential_ref,
        api_plan,
        ..
    }) = config
        .connections
        .iter()
        .find(|connection| matches!(connection, ConnectionConfig::CoinGecko { .. }))
    {
        credentials.usd_price_source(
            &fin_credentials::CredentialRef::new(credential_ref)?,
            api_plan,
        )?
    } else {
        Arc::new(fin_pricing::ExchangeSpotPrices::new(
            pricing_cache::attach(fin_pricing::CoinGeckoPrices::new()?)?,
        )?)
    };
    let binance_prices = Arc::new(SharedUsdValuator::with_source(source));
    let wallet_prices = binance_prices.clone();
    let token_discovery = Arc::new(evm_public_data::PublicTokenDiscovery::new()?);
    let mut ethereum_adapters = BTreeMap::<String, Arc<fin_ethereum::EthereumAdapter>>::new();
    for network in &config.networks {
        let mut wallets = Vec::new();
        let mut signer_map = BTreeMap::new();
        for connection in &config.connections {
            let (name, address, signing_enabled, credential_ref) = match connection {
                ConnectionConfig::Evm {
                    name,
                    address,
                    signing_enabled,
                    credential_ref,
                    ..
                } => (name, address, signing_enabled, credential_ref),
                ConnectionConfig::Ethereum {
                    name,
                    network: wallet_network,
                    address,
                    signing_enabled,
                    credential_ref,
                    ..
                } if wallet_network == &network.id => {
                    (name, address, signing_enabled, credential_ref)
                }
                _ => continue,
            };
            wallets.push(fin_ethereum::WalletConnection {
                name: name.clone(),
                address: address.clone(),
                signing_enabled: *signing_enabled,
            });
            if let Some(reference) = credential_ref {
                let credential = fin_credentials::CredentialRef::new(reference)?;
                let (derived_address, signer) = credentials.envelope_signer(&credential)?;
                if !derived_address.eq_ignore_ascii_case(address) {
                    return Err(error(
                        "SIGNER_ADDRESS_MISMATCH",
                        "configuration",
                        "Protected wallet key does not derive the configured physical address.",
                        "Correct the wallet enrollment mapping before startup.",
                    ));
                }
                signer_map.insert(address.clone(), signer);
            }
        }
        if wallets.is_empty() {
            continue;
        }
        let rpc = eth_rpcs
            .get(&network.chain_id)
            .expect("RPC built above")
            .clone();
        let signer: Option<Arc<dyn fin_evm::EnvelopeSigner>> = if signer_map.is_empty() {
            None
        } else {
            Some(Arc::new(MappedEnvelopeSigner::new(signer_map)?))
        };
        let runtime = Arc::new(fin_evm::EvmRuntime::new(rpc, network.chain_id, signer));
        let ethereum_config = if let Some(deployment) = config
            .deployments
            .iter()
            .find(|deployment| deployment.network == network.id)
        {
            fin_ethereum::EthereumConfig {
                chain_id: network.chain_id,
                confirmations: network.confirmations,
                require_finalized: false,
                weth: Some(deployment.weth.clone()),
                snapshot_tokens: deployment.snapshot_tokens.clone(),
                uniswap: Some(deployment.uniswap.clone()),
                aave: Some(deployment.aave.clone()),
            }
        } else {
            fin_ethereum::EthereumConfig::wallet_only(
                network.chain_id,
                network.confirmations,
                network.snapshot_tokens.clone(),
            )
        };
        let adapter = Arc::new(
            fin_ethereum::EthereumAdapter::new(ethereum_config, wallets, runtime)?
                .with_wallet_valuation(wallet_prices.clone())
                .with_token_discovery(token_discovery.clone()),
        );
        ethereum_adapters.insert(network.id.clone(), adapter);
    }
    let mut adapters: Vec<Arc<dyn Adapter>> = Vec::new();
    if !ethereum_adapters.is_empty() {
        adapters.push(Arc::new(evm_router::EvmNetworkRouter::new(
            ethereum_adapters
                .values()
                .map(|adapter| (adapter.config.chain_id, adapter.clone()))
                .collect(),
        )));
    }
    let exchange_client = Arc::new(fin_exchange::ccxt::CcxtClient::new().map_err(|_| {
        error(
            "EXCHANGE_TRANSPORT_UNAVAILABLE",
            "startup",
            "Exchange transport could not be initialized.",
            "Check the protected broker installation.",
        )
    })?);
    adapters.push(Arc::new(fin_exchange::ExchangeAdapter::new(
        fin_exchange::DefaultExchangeProfile::new(exchange_client.clone()),
    )));
    if config
        .connections
        .iter()
        .any(|connection| matches!(connection, ConnectionConfig::Binance { .. }))
    {
        let accounts = config
            .connections
            .iter()
            .filter_map(|connection| {
                if let ConnectionConfig::Binance {
                    name,
                    resource,
                    credential_ref,
                    role,
                    parent,
                    email,
                    wallet_type,
                    ..
                } = connection
                {
                    Some(fin_binance::BinanceAccount {
                        name: name.clone(),
                        resource: resource.clone(),
                        credential: fin_binance::CredentialHandle::new(credential_ref).ok()?,
                        role: if role == "main" {
                            fin_binance::AccountRole::Main
                        } else {
                            fin_binance::AccountRole::Sub
                        },
                        parent: parent.clone(),
                        email: email.clone(),
                        wallet_type: wallet_type.clone(),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let destinations = configured_binance_destinations(config)?;
        if accounts.is_empty() {
            return Err(error(
                "BINANCE_CONNECTIONS_INVALID",
                "configuration",
                "Configured Binance connections could not be mapped to protected account handles.",
                "Repair credential references through protected enrollment.",
            ));
        }
        let exchange_credentials: Arc<dyn fin_exchange::ccxt::CredentialProvider> =
            credentials.clone();
        let transport = Arc::new(fin_binance::BinanceCcxtTransport::new(
            exchange_client.clone(),
            exchange_credentials,
        ));
        // Binance reads and enrollment do not require a wallet observer.
        // Payout preparation checks the selected configured chain separately.
        let payout_verifier: Option<Arc<dyn fin_binance::BinanceRecipientVerifier>> =
            if ethereum_adapters.is_empty() {
                None
            } else {
                Some(Arc::new(EthereumPayoutVerifier::new(
                    ethereum_adapters
                        .values()
                        .map(|adapter| (adapter.config.chain_id, adapter.clone()))
                        .collect(),
                )?))
            };
        let adapter = Arc::new(fin_binance::BinanceAdapter::new(
            fin_binance::BinanceConfig {
                accounts,
                destinations,
            },
            transport,
            Some(binance_prices.clone()),
            payout_verifier,
        )?);
        adapters.push(adapter.clone());
        if let Some(ethereum) = ethereum_adapters
            .values()
            .find(|adapter| adapter.config.chain_id == 1)
        {
            adapters.push(Arc::new(BinanceEthereumDepositAdapter::new(
                adapter,
                ethereum.clone(),
            )));
        }
    }
    let valuator: Arc<dyn AssetValuator> = binance_prices;
    Ok(ProtectedApplications { adapters, valuator })
}

/// A reusable wallet has one payout destination per configured chain. Legacy
/// connections retain their original custody and configured asset identities.
fn configured_binance_destinations(
    config: &BrokerConfigV2,
) -> fin_domain::Result<Vec<fin_binance::EthereumDestination>> {
    let mut destinations = Vec::new();
    for connection in &config.connections {
        for network in &config.networks {
            let (name, address, resource, assets) = match connection {
                ConnectionConfig::Ethereum {
                    name,
                    address,
                    resource,
                    network: wallet_network,
                    assets,
                    ..
                } if wallet_network == &network.id => {
                    (name, address, resource.clone(), assets.clone())
                }
                ConnectionConfig::Evm { name, address, .. } => (
                    name,
                    address,
                    ResourceId(format!("{}:{}", network.id, address.to_ascii_lowercase())),
                    vec![AssetId {
                        network: network.id.clone(),
                        reference: "native".into(),
                        decimals: 18,
                    }],
                ),
                _ => continue,
            };
            let confirmations = u32::try_from(network.confirmations).map_err(|_| {
                error(
                    "CONFIG_CONFIRMATIONS_INVALID",
                    "configuration",
                    "Payout confirmations exceed the supported count.",
                    "Configure a confirmation count representable by the payout verifier.",
                )
            })?;
            destinations.push(fin_binance::EthereumDestination {
                name: name.clone(),
                resource,
                address: address.clone(),
                chain_id: network.chain_id,
                assets: assets
                    .into_iter()
                    .map(|asset| {
                        let key = if asset.reference == "native"
                            && matches!(network.chain_id, 1 | 42161 | 4663)
                        {
                            "ETH".to_owned()
                        } else {
                            asset.reference.clone()
                        };
                        (key, asset)
                    })
                    .collect(),
                confirmations,
            });
        }
    }
    Ok(destinations)
}

fin_evm::alloy::sol! {
    interface PayoutErc20 {
        function decimals() external view returns (uint8 value);
    }
}

fn pow10(power: u32) -> num_bigint::BigUint {
    num_bigint::BigUint::from(10u8).pow(power)
}

impl EthereumPayoutVerifier {
    pub fn new(
        adapters: BTreeMap<u64, Arc<fin_ethereum::EthereumAdapter>>,
    ) -> fin_domain::Result<Self> {
        if adapters.is_empty() {
            return Err(error(
                "PAYOUT_RPC_MISSING",
                "configuration",
                "No configured Ethereum observer is available for payout verification.",
                "Configure a chain-checked public HTTPS RPC.",
            ));
        }
        Ok(Self { adapters })
    }
}

#[async_trait]
impl fin_binance::BinanceRecipientVerifier for EthereumPayoutVerifier {
    fn supports_chain(&self, chain_id: u64) -> bool {
        self.adapters.contains_key(&chain_id)
    }

    async fn resolve_asset(
        &self,
        chain_id: u64,
        coin: &str,
        contract: Option<&str>,
    ) -> fin_domain::Result<Option<AssetId>> {
        let Some(adapter) = self.adapters.get(&chain_id) else {
            return Ok(None);
        };
        if !matches!(chain_id, 1 | 42161 | 4663)
            || adapter.config.chain_id != chain_id
            || adapter.runtime.chain_id != chain_id
        {
            return Ok(None);
        }
        let contract = contract.unwrap_or("");
        let (reference, decimals) = if contract.is_empty() {
            if coin != "ETH" {
                return Ok(None);
            }
            adapter.runtime.assert_chain().await?;
            ("native".to_owned(), 18)
        } else {
            if !contract.starts_with("0x") || !valid_eth_address(contract) {
                return Ok(None);
            }
            let token = fin_evm::address(contract)?;
            if token == fin_evm::alloy::primitives::Address::ZERO {
                return Ok(None);
            }
            adapter.runtime.assert_chain().await?;
            // Deployment and decimals are observed at one pinned block, rather
            // than accepting exchange symbols as an onchain asset identity.
            let block = fin_evm::rpc_u64(
                &adapter
                    .runtime
                    .rpc
                    .request("eth_blockNumber", json!([]))
                    .await?,
            )?;
            adapter.runtime.code_at(token, Some(block)).await?;
            let decimals = adapter
                .runtime
                .call(token, &PayoutErc20::decimalsCall {}, Some(block))
                .await?;
            if u32::from(decimals) > fin_binance::MAX_DECIMAL_SCALE {
                return Ok(None);
            }
            (fin_evm::lower_address(token), decimals)
        };
        Ok(Some(AssetId {
            network: format!("ethereum:{chain_id}"),
            reference,
            decimals,
        }))
    }

    async fn verify(&self, request: fin_binance::PayoutRequest) -> fin_binance::PayoutResult {
        let Some(adapter) = self.adapters.get(&request.chain_id) else {
            return fin_binance::PayoutResult {
                status: fin_domain::Status::Unknown,
                amount_atoms: None,
                evidence: json!({"verification":"failed-closed","reason":"configured chain observer missing"}),
            };
        };
        match adapter
            .verify_external_credit(
                &request.tx_hash,
                &request.recipient,
                &request.asset,
                &request.minimum_amount,
                &request.maximum_amount,
                request
                    .confirmations
                    .max(adapter.config.confirmations as u32),
            )
            .await
        {
            Ok(proof) => fin_binance::PayoutResult {
                status: proof.status,
                amount_atoms: proof.amount_atoms,
                evidence: proof.evidence,
            },
            Err(error) => fin_binance::PayoutResult {
                status: fin_domain::Status::Unknown,
                amount_atoms: None,
                evidence: json!({"verification":"failed-closed","code":error.code}),
            },
        }
    }
}

pub struct RuntimeDependencies {
    pub store: Store,
    pub adapters: Vec<Arc<dyn Adapter>>,
    pub authorizer: Arc<dyn Authorizer>,
    pub valuator: Arc<dyn AssetValuator>,
}

pub struct BrokerRuntime {
    pub engine: Arc<Engine>,
    config: Arc<BrokerConfigV2>,
    adapters: Vec<Arc<dyn Adapter>>,
    observer: Arc<CompositeObserver>,
    #[cfg(test)]
    valuator: Arc<dyn AssetValuator>,
    accepting: AtomicBool,
    protected_config_check: AtomicBool,
    account_identity_check_pending: AtomicBool,
    daemon_marker_path: Mutex<PathBuf>,
    daemon_marker_owner_uid: u32,
}

struct ValuatorPriceProvider {
    valuator: Arc<dyn AssetValuator>,
}

#[async_trait]
impl fin_engine::TrustedAssetPriceProvider for ValuatorPriceProvider {
    async fn asset_price(&self, asset: &AssetId) -> fin_domain::Result<AssetPrice> {
        self.valuator.asset_price(asset).await
    }
}

pub fn compose_runtime(
    config: BrokerConfigV2,
    dependencies: RuntimeDependencies,
) -> fin_domain::Result<Arc<BrokerRuntime>> {
    config.validate()?;
    let options = EngineOptions {
        max_movement_and_cost_usd_micros: config.policy.max_movement_usd_micros.clone(),
        max_run_allocation_usd_micros: config.policy.max_run_allocation_usd_micros.clone(),
        allow_unpriced_exchange_assets: config.policy.allow_unpriced_exchange_assets,
    };
    if dependencies.adapters.iter().any(|adapter| {
        adapter
            .definitions()
            .iter()
            .any(|definition| definition.name == "fin.snapshot")
    }) {
        return Err(error(
            "DUPLICATE_SNAPSHOT_OBSERVER",
            "startup",
            "The reserved composite snapshot operation is already registered.",
            "Remove the conflicting operation from the injected adapter set.",
        ));
    }
    let observer = Arc::new(CompositeObserver::new(dependencies.adapters.clone()));
    let mut engine_adapters = dependencies.adapters.clone();
    engine_adapters.push(observer.clone());
    let price_provider = Arc::new(ValuatorPriceProvider {
        valuator: dependencies.valuator.clone(),
    });
    let engine = Arc::new(
        Engine::new(
            dependencies.store,
            engine_adapters,
            dependencies.authorizer,
            options,
        )?
        .with_cost_origin_price_provider(price_provider),
    );
    observer.attach(Arc::downgrade(&engine));
    Ok(Arc::new(BrokerRuntime {
        engine,
        config: Arc::new(config),
        adapters: dependencies.adapters,
        observer,
        #[cfg(test)]
        valuator: dependencies.valuator,
        accepting: AtomicBool::new(true),
        protected_config_check: AtomicBool::new(false),
        account_identity_check_pending: AtomicBool::new(false),
        daemon_marker_path: Mutex::new(PathBuf::from(fin_protocol::daemon::STOP_MARKER_PATH)),
        daemon_marker_owner_uid: unsafe { libc::geteuid() },
    }))
}

/// Compose the installed runtime only after fixed-path root installation,
/// protected store/config validation, and trusted credential setup succeed.
pub async fn protected_runtime() -> fin_domain::Result<Arc<BrokerRuntime>> {
    validate_installation()?;
    let config = load_protected_config()?;
    protected_runtime_from_config(config).await
}

async fn protected_runtime_from_config(
    config: BrokerConfigV2,
) -> fin_domain::Result<Arc<BrokerRuntime>> {
    validate_installation()?;
    let store = open_protected_store()?;
    let credentials = Arc::new(ProtectedCredentialProvider::open()?);
    let applications = build_protected_applications(&config, credentials)?;
    let authorizer = Arc::new(MacOsAuthorizer::protected(config.approver_uid)?);
    let runtime = compose_runtime(
        config,
        RuntimeDependencies {
            store,
            adapters: applications.adapters,
            authorizer,
            valuator: applications.valuator,
        },
    )?;
    runtime.protected_config_check.store(true, Ordering::SeqCst);
    runtime
        .account_identity_check_pending
        .store(true, Ordering::SeqCst);
    Ok(runtime)
}

impl BrokerRuntime {
    async fn verify_account_identities(&self) -> fin_domain::Result<()> {
        if !self.account_identity_check_pending.load(Ordering::SeqCst) {
            return Ok(());
        }
        for connection in &self.config.connections {
            let ConnectionConfig::Binance { name, .. } = connection else {
                continue;
            };
            let adapter = self
                .adapters
                .iter()
                .find(|adapter| {
                    adapter
                        .definitions()
                        .iter()
                        .any(|definition| definition.name == "binance.account-identity")
                })
                .ok_or_else(|| {
                    error(
                        "BINANCE_IDENTITY_OBSERVER_MISSING",
                        "startup",
                        "The fixed Binance account identity observer is unavailable.",
                        "Upgrade the protected adapter and restart.",
                    )
                })?;
            adapter
                .read("binance.account-identity", &json!({"account":name}))
                .await?;
        }
        self.account_identity_check_pending
            .store(false, Ordering::SeqCst);
        Ok(())
    }

    pub fn configuration(&self) -> &BrokerConfigV2 {
        &self.config
    }

    pub async fn handle(&self, request: BrokerRequest) -> BrokerResponse {
        match self.handle_result(request).await {
            Ok(value) => BrokerResponse::success(value),
            Err(error) => BrokerResponse::failure(error),
        }
    }

    async fn handle_result(&self, request: BrokerRequest) -> fin_domain::Result<Value> {
        use BrokerRequest as R;
        let lifecycle_request = matches!(&request, R::DaemonStart | R::DaemonStop);
        if self.protected_config_check.load(Ordering::SeqCst) {
            let stopped = fin_protocol::daemon::stop_requested_at(
                &self.daemon_marker_path.lock().unwrap(),
                self.daemon_marker_owner_uid,
            )?;
            if stopped && !lifecycle_request {
                self.accepting.store(false, Ordering::SeqCst);
                return Err(error(
                    "DAEMON_STOPPED",
                    "daemon",
                    "The protected broker is stopped and accepts lifecycle requests only.",
                    "Explicitly start the Fin broker service before sending other requests.",
                ));
            }
        }
        // Local management stays available during venue outages. All account
        // operations retain the identity gate that previously blocked startup.
        let local_management = matches!(
            &request,
            R::Capabilities
                | R::PolicyShow
                | R::Connections
                | R::Doctor
                | R::Operations
                | R::DaemonStart
                | R::DaemonStop
                | R::RevokeBoundary { .. }
                | R::BoundaryStatus { .. }
        );
        let public_market_read = matches!(&request, R::Read { operation, .. }
            if matches!(operation.as_str(), "exchange.order-book" | "exchange.markets" | "exchange.ticker"));
        if !local_management && !public_market_read {
            self.verify_account_identities().await?;
        }
        match request {
            R::Capabilities => Ok(
                json!({"version":2,"methods":protocol_methods(),"maxFrameBytes":fin_protocol::MAX_FRAME_BYTES,"maxResponseFrameBytes":fin_protocol::MAX_RESPONSE_FRAME_BYTES,"exchanges":fin_discovery::exchange_capabilities()}),
            ),
            R::PolicyShow => {
                let mut view = fin_discovery::PolicyView::new(
                    &self.config.policy.max_movement_usd_micros,
                    &self.config.policy.max_run_allocation_usd_micros,
                )?;
                view.set_unpriced_policy(self.config.policy.allow_unpriced_exchange_assets);
                match self.engine.allocation_unpriced_operations() {
                    Ok(count) => {
                        view.allocation_unpriced_operations = Some(count);
                        if count > 0 {
                            view.unpriced_assets_warning =
                                Some(fin_domain::unpriced::WARNING.into());
                        }
                    }
                    Err(error) => view.allocation_usage_error = Some(error),
                }
                match self.engine.allocation_used_usd_micros() {
                    Ok(value) => view.allocation_used_usd = Some(format_micros_total(&value)),
                    Err(error) => view.allocation_usage_error = Some(error),
                }
                serde_json::to_value(view).map_err(|_| internal_error())
            }
            R::Connections => {
                let mut connections = self
                    .adapters
                    .iter()
                    .flat_map(|adapter| adapter.connections())
                    .collect::<Vec<_>>();
                for connection in &self.config.connections {
                    if let ConnectionConfig::CoinGecko {
                        name,
                        resource,
                        api_plan,
                        ..
                    } = connection
                    {
                        connections.push(fin_domain::ConnectionMetadata { name:name.clone(), provider:"coingecko".into(), resource:resource.clone(), role:None, parent:None, network:None, address:None, credential_ref:None, signing_enabled:false, details:json!({"api_plan":api_plan,"purpose":"USD pricing reads","financial_access":false}) });
                    }
                }
                serde_json::to_value(connections).map_err(|_| internal_error())
            }
            R::Doctor => Ok(
                json!({"configurationVersion":self.config.version,"approverConfigured":true,"adapters":self.adapters.iter().flat_map(|a|a.definitions()).map(|d| d.name).collect::<Vec<_>>(),"protectedInstallationRequired":true,"financialReadiness":"requires-live-probes"}),
            ),
            R::Read { operation, input } => {
                serde_json::to_value(self.engine.read(&operation, &input).await?)
                    .map_err(|_| internal_error())
            }
            R::Preview { operation, input } => {
                serde_json::to_value(self.engine.preview(&operation, &input).await?)
                    .map_err(|_| internal_error())
            }
            R::PreviewBoundaryAuthorization { request } => {
                self.engine.preview_boundary_authorization(request).await
            }
            R::AuthorizeBoundary { request } => {
                serde_json::to_value(self.engine.authorize_boundary(request).await?)
                    .map_err(|_| internal_error())
            }
            R::BoundaryStatus { id } => {
                serde_json::to_value(self.engine.boundary_status(&id).await?)
                    .map_err(|_| internal_error())
            }
            R::RevokeBoundary { id } => {
                serde_json::to_value(self.engine.revoke_boundary(&id).await?)
                    .map_err(|_| internal_error())
            }
            R::PreviewBoundary {
                id,
                operation,
                input,
            } => serde_json::to_value(
                self.engine
                    .preview_boundary(&id, &operation, &input)
                    .await?,
            )
            .map_err(|_| internal_error()),
            R::ExecuteBoundary {
                authorization_id,
                request,
            } => serde_json::to_value(
                self.engine
                    .execute_boundary(&authorization_id, request)
                    .await?,
            )
            .map_err(|_| internal_error()),
            R::ReplaceBoundaryCandidate {
                authorization_id,
                previous_attempt_id,
                request,
            } => serde_json::to_value(
                self.engine
                    .execute_boundary_replacing(&authorization_id, request, &previous_attempt_id)
                    .await?,
            )
            .map_err(|_| internal_error()),
            R::Execute { request } => serde_json::to_value(self.engine.execute(request).await?)
                .map_err(|_| internal_error()),
            R::Status { id } => serde_json::to_value(self.engine.status(&id, true).await?)
                .map_err(|_| internal_error()),
            R::Operations => {
                serde_json::to_value(self.engine.operations()?).map_err(|_| internal_error())
            }
            R::Snapshot => {
                serde_json::to_value(self.snapshot().await?).map_err(|_| internal_error())
            }
            R::RunBegin { .. } | R::RunReplan { .. } | R::RunFinish { .. } => Err(FinError::new(
                "RUN_RETIRED",
                "validation",
                "Verification run mutations are retired.",
                "Execute ordinary operations directly; inspect operations with fin operations and reconcile them with fin status.",
            )),
            R::DaemonStart => {
                if self.protected_config_check.load(Ordering::SeqCst) {
                    fin_protocol::daemon::resume_at(
                        &self.daemon_marker_path.lock().unwrap(),
                        self.daemon_marker_owner_uid,
                    )?;
                }
                self.accepting.store(true, Ordering::SeqCst);
                Ok(json!({"status":"running","pid":std::process::id()}))
            }
            R::DaemonStop => {
                if self.protected_config_check.load(Ordering::SeqCst) {
                    fin_protocol::daemon::request_stop_at(
                        &self.daemon_marker_path.lock().unwrap(),
                        self.daemon_marker_owner_uid,
                    )?;
                }
                self.accepting.store(false, Ordering::SeqCst);
                Ok(json!({"status":"stopping"}))
            }
        }
    }

    pub async fn snapshot(&self) -> fin_domain::Result<StateSnapshot> {
        self.observer.snapshot().await
    }

    #[cfg(test)]
    async fn begin_run(&self, request: RunBeginRequest) -> fin_domain::Result<RunResult> {
        if request.allocations.is_empty()
            || request.planned_operations.is_empty()
            || request.unwind.is_empty()
            || request.planned_operations.len() + request.unwind.len() > 64
        {
            return Err(error(
                "RETURN_ROUTE_NOT_READY",
                "budget",
                "Run needs selected holdings, forward operations and a complete return path.",
                "Supply exact selected holdings and every forward and return step before starting.",
            ));
        }
        // The engine owns the authoritative observer capture. Keep its opaque
        // token while the broker validates allocations and obtains trusted
        // prices, then consume that same capture instead of taking a second
        // complete Ethereum snapshot after the price reads.
        let observed = self.engine.observe_run_baseline("fin.snapshot").await?;
        let baseline = observed.snapshot().clone();
        if !baseline.complete
            || !baseline.pending.is_empty()
            || !baseline.state.in_flight.is_empty()
        {
            return Err(error(
                "INCOMPLETE_BASELINE",
                "budget",
                "Broker observers did not capture complete quiet state.",
                "Reconcile pending activity and retry the broker-captured snapshot.",
            ));
        }
        let definitions = self.engine.definitions()?;
        let mut allocations = Vec::with_capacity(request.allocations.len());
        let mut allocation_value = num_bigint::BigUint::default();
        for selected in &request.allocations {
            let observed = baseline
                .state
                .holdings
                .iter()
                .find(|holding| {
                    holding.resource == selected.resource
                        && holding.amount.asset == selected.amount.asset
                })
                .ok_or_else(|| {
                    error(
                        "ALLOCATION_NOT_IN_BASELINE",
                        "budget",
                        "Selected holding is absent from the broker-captured baseline.",
                        "Select an exact holding reported by the broker.",
                    )
                })?;
            if selected.amount.atoms.integer() > observed.amount.atoms.integer() {
                return Err(error(
                    "ALLOCATION_NOT_IN_BASELINE",
                    "budget",
                    "Selected quantity exceeds the broker-observed holding.",
                    "Reduce the selected quantity to the exact observed available allocation.",
                ));
            }
            let (value_usd_micros, observed_at, source) =
                self.valuator.usd_micros(&selected.amount).await?;
            allocation_value += parse_atoms(&value_usd_micros)?;
            allocations.push(RunAllocation {
                holding: selected.clone(),
                value_usd_micros,
                observed_at,
                source,
            });
        }
        if fin_domain::policy::limit(&self.config.policy.max_run_allocation_usd_micros)?
            .is_some_and(|limit| allocation_value > num_bigint::BigUint::from(limit))
        {
            return Err(error(
                "RUN_ALLOCATION_LIMIT",
                "budget",
                "Selected allocations exceed the configured run limit.",
                "Reduce selected holdings to stay within the protected run policy.",
            ));
        }
        let selected_assets = request
            .allocations
            .iter()
            .map(|allocation| allocation.amount.asset.clone())
            .collect::<Vec<_>>();
        let route_prices = self
            .trusted_asset_prices(&baseline, &selected_assets)
            .await?;
        let mut fee_caps = num_bigint::BigUint::default();
        let steps = request.planned_operations.iter().chain(&request.unwind);
        for (index, step) in steps.enumerate() {
            if !definitions.iter().any(|definition| {
                definition.name == step.operation && definition.kind == OperationKind::Write
            }) || step.description.is_empty()
                || step.description.len() > 1024
                || step
                    .input
                    .as_object()
                    .is_none_or(|object| object.len() > 128)
                || step.maximum_fee_usd_micros.is_empty()
                || !step
                    .maximum_fee_usd_micros
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                || index >= 64
            {
                return Err(error(
                    "RETURN_ROUTE_INVALID",
                    "budget",
                    "A route step is unsupported, incomplete or exceeds the bounded plan length.",
                    "Provide a reviewed plan operation, strict input, finite fee cap and short purpose.",
                ));
            }
            fee_caps += parse_atoms(&step.maximum_fee_usd_micros)?;
        }
        let return_cost_cap = parse_atoms(&request.return_cost_reserve_usd_micros)?;
        if fee_caps > return_cost_cap {
            return Err(error(
                "RETURN_COST_BUDGET_EXCEEDED",
                "budget",
                "Forward and return fee caps exceed the reserved total.",
                "Increase the total cost reserve or reduce planned fee caps.",
            ));
        }
        let plan = RunPlan {
            id: uuid::Uuid::new_v4().to_string(),
            baseline,
            allocations,
            borrowed_proceeds_reserve_usd_micros: request.borrowed_proceeds_reserve_usd_micros,
            return_cost_reserve_usd_micros: request.return_cost_reserve_usd_micros,
            planned_operations: request.planned_operations,
            unwind: request.unwind,
            route_proofs: vec![],
            asset_prices: vec![],
        };
        self.engine
            .begin_run_from_observed_baseline_with_asset_prices(
                observed,
                plan,
                route_prices,
                Utc::now(),
            )
            .await
    }

    #[cfg(test)]
    async fn replan_run(
        &self,
        id: String,
        remaining: Vec<fin_domain::PlanStep>,
    ) -> fin_domain::Result<RunResult> {
        if remaining.is_empty() || remaining.len() > 64 {
            return Err(error(
                "RETURN_ROUTE_INVALID",
                "budget",
                "A replacement route must contain a bounded list of remaining steps.",
                "Provide every remaining forward and cleanup step in order.",
            ));
        }
        let definitions = self.engine.definitions()?;
        for step in &remaining {
            if !definitions.iter().any(|definition| {
                definition.name == step.operation && definition.kind == OperationKind::Write
            }) || step.description.is_empty()
                || step.description.len() > 1024
                || step
                    .input
                    .as_object()
                    .is_none_or(|object| object.len() > 128)
                || step.maximum_fee_usd_micros.is_empty()
                || !step
                    .maximum_fee_usd_micros
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
            {
                return Err(error(
                    "RETURN_ROUTE_INVALID",
                    "budget",
                    "A replacement route step is unsupported or incomplete.",
                    "Use reviewed writes with strict inputs, finite fee caps and short purposes.",
                ));
            }
            parse_atoms(&step.maximum_fee_usd_micros)?;
        }
        let current = self.snapshot().await?;
        if !current.complete || !current.pending.is_empty() || !current.state.in_flight.is_empty() {
            return Err(error(
                "INCOMPLETE_BASELINE",
                "budget",
                "Fresh complete and quiet state is required to revise a return route.",
                "Reconcile pending activity and retry with a broker-captured snapshot.",
            ));
        }
        // Original allocation assets remain relevant to origin/cost accounting
        // when their current local holding is zero after a custody conversion.
        let original_assets = self.engine.run_allocation_assets(&id)?;
        let route_prices = self
            .trusted_asset_prices(&current, &original_assets)
            .await?;
        self.engine
            .replan_remaining_from_observer(
                &id,
                "fin.snapshot",
                remaining,
                route_prices,
                Utc::now(),
            )
            .await
    }

    #[cfg(test)]
    async fn trusted_asset_prices(
        &self,
        snapshot: &StateSnapshot,
        additional_assets: &[AssetId],
    ) -> fin_domain::Result<Vec<AssetPrice>> {
        let mut assets = Vec::<AssetId>::new();
        for asset in snapshot
            .state
            .holdings
            .iter()
            .filter(|holding| !holding.amount.atoms.is_zero())
            .map(|holding| &holding.amount.asset)
            .chain(
                snapshot
                    .state
                    .positions
                    .iter()
                    .filter(|position| !position.amount.atoms.is_zero())
                    .map(|position| &position.amount.asset),
            )
            .chain(additional_assets.iter())
        {
            if !assets.contains(asset) {
                assets.push(asset.clone());
            }
        }
        for connection in &self.config.connections {
            if let ConnectionConfig::Ethereum {
                assets: configured, ..
            } = connection
            {
                for asset in configured {
                    if !assets.contains(asset) {
                        assets.push(asset.clone());
                    }
                }
            }
            if let ConnectionConfig::Evm { address, .. } = connection {
                // Preserve the reviewed protocol scope needed to value planned
                // intermediate assets, even before their balances become positive.
                // Read-only discovery candidates do not expand this trusted scope.
                for deployment in &self.config.deployments {
                    let network = self
                        .config
                        .networks
                        .iter()
                        .find(|network| network.id == deployment.network)
                        .expect("validated deployment");
                    let resource = ResourceId(format!(
                        "ethereum:{}:{}",
                        network.chain_id,
                        address.to_ascii_lowercase()
                    ));
                    for holding in snapshot.state.holdings.iter().filter(|holding| {
                        holding.resource == resource
                            && holding.amount.asset.network == deployment.network
                            && (holding.amount.asset.reference == "native"
                                || deployment
                                    .snapshot_tokens
                                    .contains(&holding.amount.asset.reference))
                    }) {
                        if !assets.contains(&holding.amount.asset) {
                            assets.push(holding.amount.asset.clone());
                        }
                    }
                }
            }
        }
        let mut prices = Vec::with_capacity(assets.len());
        for asset in &assets {
            prices.push(self.valuator.asset_price(asset).await?);
        }
        Ok(prices)
    }
}

struct CompositeObserver {
    adapters: Vec<Arc<dyn Adapter>>,
    engine: Mutex<Option<Weak<Engine>>>,
}

impl CompositeObserver {
    fn new(adapters: Vec<Arc<dyn Adapter>>) -> Self {
        Self {
            adapters,
            engine: Mutex::new(None),
        }
    }
    fn attach(&self, engine: Weak<Engine>) {
        if let Ok(mut slot) = self.engine.lock() {
            *slot = Some(engine);
        }
    }
}

#[async_trait]
impl Adapter for CompositeObserver {
    fn definitions(&self) -> Vec<Definition> {
        vec![snapshot_definition()]
    }

    fn connections(&self) -> Vec<fin_domain::ConnectionMetadata> {
        self.adapters
            .iter()
            .flat_map(|adapter| adapter.connections())
            .collect()
    }

    async fn snapshot(&self) -> fin_domain::Result<StateSnapshot> {
        let mut combined = StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![],
            pending: vec![],
        };
        for adapter in &self.adapters {
            match adapter.snapshot().await {
                Ok(snapshot) => {
                    combined.complete &= snapshot.complete;
                    combined.coverage.extend(snapshot.coverage);
                    combined.pending.extend(snapshot.pending);
                    combined.state.holdings.extend(snapshot.state.holdings);
                    combined.state.positions.extend(snapshot.state.positions);
                    combined
                        .state
                        .authorities
                        .extend(snapshot.state.authorities);
                    combined.state.in_flight.extend(snapshot.state.in_flight);
                }
                Err(error) => {
                    combined.complete = false;
                    combined.pending.push(format!("observer:{}", error.code));
                }
            }
        }
        if let Some(engine) = self
            .engine
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            .and_then(Weak::upgrade)
        {
            for record in engine.operations()? {
                if !record.status.terminal() {
                    combined.complete = false;
                    combined.state.in_flight.push(record.id.clone());
                    combined
                        .pending
                        .push(format!("operation:{}:{:?}", record.id, record.status));
                }
            }
        } else {
            combined.complete = false;
            combined
                .pending
                .push("operation-journal-unavailable".into());
        }
        combined.coverage.sort();
        combined.coverage.dedup();
        combined.state.in_flight.sort();
        combined.state.in_flight.dedup();
        combined.pending.sort();
        combined.pending.dedup();
        Ok(combined)
    }

    async fn read(&self, operation: &str, _input: &Value) -> fin_domain::Result<Value> {
        if operation != "fin.snapshot" {
            return Err(FinError::validation(
                "Unknown composite observer operation.",
            ));
        }
        serde_json::to_value(self.snapshot().await?).map_err(|_| internal_error())
    }
    async fn prepare(
        &self,
        _operation: &str,
        _input: &Value,
    ) -> fin_domain::Result<PreparedOperation> {
        Err(error(
            "READ_ONLY_OPERATION",
            "validation",
            "Composite observer has no write operations.",
            "Use supported application write previews.",
        ))
    }
    async fn revalidate(
        &self,
        _prepared: &PreparedOperation,
    ) -> fin_domain::Result<PreparedOperation> {
        Err(error(
            "READ_ONLY_OPERATION",
            "validation",
            "Composite observer has no write operations.",
            "Use supported application write previews.",
        ))
    }
    async fn submit(
        &self,
        _prepared: &PreparedOperation,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> fin_domain::Result<Submission> {
        Err(error(
            "READ_ONLY_OPERATION",
            "validation",
            "Composite observer has no write operations.",
            "Use supported application write previews.",
        ))
    }
    async fn reconcile(&self, _record: &OperationRecord) -> fin_domain::Result<Reconciliation> {
        Err(error(
            "READ_ONLY_OPERATION",
            "reconciliation",
            "Composite observer has no write operations.",
            "Reconcile through the originating application adapter.",
        ))
    }
}

#[cfg(test)]
fn parse_atoms(value: &str) -> fin_domain::Result<num_bigint::BigUint> {
    value.parse().map_err(|_| {
        error(
            "INVALID_AMOUNT",
            "budget",
            "Run allocation quantity is not an exact integer atom count.",
            "Select a canonical positive base-unit quantity.",
        )
    })
}

fn format_micros_total(micros: &str) -> String {
    let padded = format!("{:0>7}", micros);
    let (whole, fraction) = padded.split_at(padded.len() - 6);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.into()
    } else {
        format!("{whole}.{fraction}")
    }
}

fn protocol_methods() -> Vec<&'static str> {
    vec![
        "capabilities",
        "connections",
        "policy_show",
        "doctor",
        "read",
        "preview",
        "preview_boundary_authorization",
        "authorize_boundary",
        "boundary_status",
        "revoke_boundary",
        "preview_boundary",
        "execute",
        "execute_boundary",
        "replace_boundary_candidate",
        "status",
        "operations",
        "snapshot",
        "daemon_start",
        "daemon_stop",
    ]
}

fn internal_error() -> FinError {
    error(
        "BROKER_INTERNAL",
        "broker",
        "The broker could not encode an internal result.",
        "Inspect the durable operation record and retry only an idempotent read.",
    )
}

pub async fn handle_request(runtime: &BrokerRuntime, request: BrokerRequest) -> BrokerResponse {
    runtime.handle(request).await
}

/// Production entry point validates the installed root process and uses only
/// fixed config, database, and socket locations. Adapter runtime wiring is
/// deliberately composed separately so automated tests cannot open live keys.
pub fn open_protected_store() -> fin_domain::Result<Store> {
    validate_installation()?;
    validate_private_file_parent(Path::new(OPERATIONS_PATH))?;
    Store::open(OPERATIONS_PATH).map_err(|_| {
        error(
            "STORE_UNAVAILABLE",
            "startup",
            "The protected operation store is unavailable.",
            "Repair the protected installation and preserve the existing journal.",
        )
    })
}

pub fn load_protected_config() -> fin_domain::Result<BrokerConfigV2> {
    validate_installation()?;
    let path = Path::new(CONFIG_PATH);
    validate_metadata(path, true)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        error(
            "CONFIG_UNAVAILABLE",
            "configuration",
            "Protected broker configuration is unavailable.",
            "Run human connection enrollment.",
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(error(
            "CONFIG_INVALID",
            "configuration",
            "Protected configuration is not a bounded regular file.",
            "Repair it through human enrollment.",
        ));
    }
    let file = OpenOptions::new().read(true).open(path).map_err(|_| {
        error(
            "CONFIG_UNAVAILABLE",
            "configuration",
            "Protected broker configuration could not be opened.",
            "Repair the protected installation.",
        )
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            error(
                "CONFIG_UNAVAILABLE",
                "configuration",
                "Protected broker configuration could not be read.",
                "Repair the protected installation.",
            )
        })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(error(
            "CONFIG_INVALID",
            "configuration",
            "Protected configuration exceeds its size limit.",
            "Repair it through human enrollment.",
        ));
    }
    let config: BrokerConfigV2 = serde_json::from_slice(&bytes).map_err(|_| {
        error(
            "CONFIG_INVALID",
            "configuration",
            "Protected configuration schema is invalid.",
            "Repair it through human enrollment.",
        )
    })?;
    config.validate()?;
    Ok(config)
}

/// Enrollment helper. It is not exposed over the agent protocol and never
/// accepts financial actions; root's human terminal owns calls to this API.
pub fn save_protected_config(config: &BrokerConfigV2) -> fin_domain::Result<()> {
    validate_installation()?;
    config.validate()?;
    let path = Path::new(CONFIG_PATH);
    let parent = path.parent().expect("fixed config parent");
    validate_metadata(parent, false)?;
    let bytes = serde_json::to_vec(config).map_err(|_| {
        error(
            "CONFIG_INVALID",
            "configuration",
            "Configuration could not be encoded.",
            "Repair it in the human enrollment flow.",
        )
    })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(error(
            "CONFIG_INVALID",
            "configuration",
            "Configuration exceeds its size limit.",
            "Reduce the configured connection set.",
        ));
    }
    atomic_replace(parent, path, &bytes)
}

fn atomic_replace(parent: &Path, destination: &Path, bytes: &[u8]) -> fin_domain::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let temp = parent.join(format!(".config.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temp)
        .map_err(|_| {
            error(
                "CONFIG_SAVE_FAILED",
                "configuration",
                "Temporary protected configuration could not be created.",
                "Retry protected enrollment.",
            )
        })?;
    let result = (|| {
        file.write_all(bytes).map_err(|_| {
            error(
                "CONFIG_SAVE_FAILED",
                "configuration",
                "Protected configuration could not be written.",
                "Retry protected enrollment.",
            )
        })?;
        file.sync_all().map_err(|_| {
            error(
                "CONFIG_SAVE_FAILED",
                "configuration",
                "Protected configuration could not be synchronized.",
                "Retry protected enrollment.",
            )
        })?;
        fs::rename(&temp, destination).map_err(|_| {
            error(
                "CONFIG_SAVE_FAILED",
                "configuration",
                "Protected configuration could not be atomically replaced.",
                "Preserve the prior config and retry protected enrollment.",
            )
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                error(
                    "CONFIG_SAVE_FAILED",
                    "configuration",
                    "Protected configuration directory could not be synchronized.",
                    "Inspect installed configuration before retrying.",
                )
            })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub struct ConfigurationLock(File);

impl Drop for ConfigurationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn lock_protected_configuration(exclusive: bool) -> fin_domain::Result<ConfigurationLock> {
    let file = protected_lock_file()?;
    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        return Err(error(
            "MAINTENANCE_LOCK_UNAVAILABLE",
            "configuration",
            "The protected configuration could not be locked.",
            "Retry after protected maintenance finishes.",
        ));
    }
    Ok(ConfigurationLock(file))
}

fn try_lock_protected_configuration(
    exclusive: bool,
) -> fin_domain::Result<Option<ConfigurationLock>> {
    let file = protected_lock_file()?;
    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    } | libc::LOCK_NB;
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        let lock_error = std::io::Error::last_os_error();
        if lock_error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error(
            "MAINTENANCE_LOCK_UNAVAILABLE",
            "configuration",
            "The protected configuration could not be locked.",
            "Repair the protected installation before serving requests.",
        ));
    }
    Ok(Some(ConfigurationLock(file)))
}

fn protected_lock_file() -> fin_domain::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    validate_private_file_parent(Path::new(MAINTENANCE_LOCK_PATH))?;
    validate_metadata(Path::new(MAINTENANCE_LOCK_PATH), true)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(MAINTENANCE_LOCK_PATH)
        .map_err(|_| {
            error(
                "MAINTENANCE_LOCK_UNAVAILABLE",
                "configuration",
                "The protected maintenance lock is unavailable.",
                "Repair the protected installation before serving requests.",
            )
        })
}

fn validate_private_file_parent(path: &Path) -> fin_domain::Result<()> {
    if let Some(parent) = path.parent() {
        validate_metadata(parent, false)?;
    }
    Ok(())
}

/// Serve one bounded request per peer-authenticated local socket connection.
/// This function is exposed for synthetic harnesses; production calls it only
/// from `serve_protected`, whose peer UID and process identity are fixed.
pub async fn serve_connection(
    stream: UnixStream,
    runtime: &Arc<BrokerRuntime>,
    expected_uid: u32,
) -> fin_domain::Result<()> {
    serve_connection_with_timeouts(
        stream,
        &mut runtime.clone(),
        expected_uid,
        IpcTimeouts::PRODUCTION,
    )
    .await
}

#[async_trait]
trait RuntimeLoader: Send + Sync {
    async fn load(&self, config: BrokerConfigV2) -> fin_domain::Result<Arc<BrokerRuntime>>;
}

struct ProtectedRuntimeLoader;
#[async_trait]
impl RuntimeLoader for ProtectedRuntimeLoader {
    async fn load(&self, config: BrokerConfigV2) -> fin_domain::Result<Arc<BrokerRuntime>> {
        protected_runtime_from_config(config).await
    }
}

async fn refresh_runtime(
    runtime: &mut Arc<BrokerRuntime>,
    current: BrokerConfigV2,
    expected_uid: u32,
    loader: &dyn RuntimeLoader,
) -> fin_domain::Result<()> {
    current.validate()?;
    if current.approver_uid != expected_uid {
        return Err(error(
            "APPROVER_UID_MISMATCH",
            "configuration",
            "The configured human account changed; this service cannot accept the old account.",
            "Repair the service socket through protected setup.",
        ));
    }
    if serde_json::to_value(runtime.configuration()).map_err(|_| internal_error())?
        != serde_json::to_value(&current).map_err(|_| internal_error())?
    {
        // Replace only after complete protected composition succeeds. The same
        // journal is reopened; no request, approval or submission is replayed.
        *runtime = loader.load(current).await?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct IpcTimeouts {
    frame_read: Duration,
    response_write: Duration,
    configuration_lock: Duration,
}

impl IpcTimeouts {
    const PRODUCTION: Self = Self {
        frame_read: IPC_FRAME_READ_TIMEOUT,
        response_write: IPC_RESPONSE_WRITE_TIMEOUT,
        configuration_lock: IPC_CONFIGURATION_LOCK_TIMEOUT,
    };
}

async fn serve_connection_with_timeouts(
    mut stream: UnixStream,
    runtime: &mut Arc<BrokerRuntime>,
    expected_uid: u32,
    timeouts: IpcTimeouts,
) -> fin_domain::Result<()> {
    let peer_uid = peer_uid(&stream)?;
    if peer_uid != expected_uid {
        return Err(error(
            "IPC_PEER_DENIED",
            "protocol",
            "Local broker peer UID is not the configured human approver.",
            "Connect from the enrolled human account.",
        ));
    }
    let request = {
        let mut reader = BufReader::new(&mut stream);
        tokio::time::timeout(timeouts.frame_read, read_request(&mut reader))
            .await
            .map_err(|_| {
                error(
                    "IPC_READ_TIMEOUT",
                    "protocol",
                    "The broker request frame was not completed before the read deadline.",
                    "Reconnect and send one bounded newline-terminated request promptly.",
                )
            })??
            .ok_or_else(|| {
                error(
                    "INVALID_PROTOCOL_FRAME",
                    "protocol",
                    "The client closed without a request frame.",
                    "Send one bounded newline-terminated request.",
                )
            })?
    };
    // Read the bounded frame before locking so a connected peer cannot hold
    // configuration maintenance open without sending a request. Enrollment
    // takes LOCK_EX; LOCK_SH now protects comparison and full dispatch.
    let _configuration_guard = if runtime.protected_config_check.load(Ordering::SeqCst) {
        let guard = match acquire_request_configuration_lock(timeouts.configuration_lock).await {
            Ok(guard) => guard,
            Err(error) if error.code == "CONFIGURATION_BUSY" => {
                write_ipc_response(
                    &mut stream,
                    &BrokerResponse::failure(error),
                    timeouts.response_write,
                )
                .await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let refreshed = match load_protected_config() {
            Ok(current) => {
                refresh_runtime(runtime, current, expected_uid, &ProtectedRuntimeLoader).await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = refreshed {
            let failure = BrokerResponse::failure(error);
            write_ipc_response(&mut stream, &failure, timeouts.response_write).await?;
            return Ok(());
        }
        Some(guard)
    } else {
        None
    };
    let response = runtime.handle(request).await;
    write_ipc_response(&mut stream, &response, timeouts.response_write).await?;
    Ok(())
}

async fn acquire_request_configuration_lock(
    timeout: Duration,
) -> fin_domain::Result<ConfigurationLock> {
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(lock) = try_lock_protected_configuration(false)? {
                return Ok(lock);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| {
        error(
            "CONFIGURATION_BUSY",
            "configuration",
            "Protected configuration maintenance did not finish before the request deadline.",
            "Retry after enrollment or another configuration update completes.",
        )
    })?
}

async fn write_ipc_response(
    stream: &mut UnixStream,
    response: &BrokerResponse,
    timeout: Duration,
) -> fin_domain::Result<()> {
    let encoded = encode_response(response)?;
    tokio::time::timeout(timeout, stream.write_all(&encoded))
        .await
        .map_err(|_| {
            error(
                "IPC_WRITE_TIMEOUT",
                "protocol",
                "The broker response write exceeded its deadline.",
                "Reconnect and inspect durable operation status before retrying a write.",
            )
        })?
        .map_err(|_| {
            error(
                "IPC_WRITE_FAILED",
                "protocol",
                "The broker response could not be written.",
                "Reconnect and inspect durable operation status before retrying a write.",
            )
        })?;
    tokio::time::timeout(timeout, stream.shutdown())
        .await
        .map_err(|_| {
            error(
                "IPC_WRITE_TIMEOUT",
                "protocol",
                "The broker response channel did not close before its deadline.",
                "Reconnect and inspect durable operation status before retrying a write.",
            )
        })?
        .map_err(|_| {
            error(
                "IPC_WRITE_FAILED",
                "protocol",
                "The broker response channel could not be closed cleanly.",
                "Reconnect and inspect durable operation status before retrying a write.",
            )
        })
}

pub async fn serve_listener(
    listener: UnixListener,
    runtime: Arc<BrokerRuntime>,
    expected_uid: u32,
) -> fin_domain::Result<()> {
    serve_listener_with_timeouts(listener, runtime, expected_uid, IpcTimeouts::PRODUCTION).await
}

async fn serve_listener_with_timeouts(
    listener: UnixListener,
    mut runtime: Arc<BrokerRuntime>,
    expected_uid: u32,
    timeouts: IpcTimeouts,
) -> fin_domain::Result<()> {
    while runtime.accepting.load(Ordering::SeqCst) {
        let (stream, _) = listener.accept().await.map_err(|_| {
            error(
                "IPC_ACCEPT_FAILED",
                "startup",
                "The protected broker socket stopped accepting connections.",
                "Restart through launchd after checking the durable journal.",
            )
        })?;
        if let Err(error) =
            serve_connection_with_timeouts(stream, &mut runtime, expected_uid, timeouts).await
        {
            // Do not echo peer-controlled data or allow malformed frames to
            // terminate the broker; safe protocol errors are per connection.
            if matches!(
                error.code.as_str(),
                "IPC_PEER_DENIED"
                    | "INVALID_PROTOCOL_FRAME"
                    | "IPC_READ_TIMEOUT"
                    | "IPC_WRITE_FAILED"
                    | "IPC_WRITE_TIMEOUT"
            ) {
                continue;
            }
            return Err(error);
        }
    }
    Ok(())
}

pub async fn serve_protected(runtime: Arc<BrokerRuntime>) -> fin_domain::Result<()> {
    validate_installation()?;
    let approver_uid = runtime.config.approver_uid;
    validate_metadata(Path::new("/Library/Fin/run"), false)?;
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::fd::FromRawFd};
        unsafe extern "C" {
            fn launch_activate_socket(
                name: *const libc::c_char,
                fds: *mut *mut libc::c_int,
                count: *mut usize,
            ) -> libc::c_int;
        }
        let name = CString::new("Broker").expect("static socket name");
        let mut descriptors: *mut libc::c_int = std::ptr::null_mut();
        let mut count = 0usize;
        // launchd allocates the descriptor array and transfers ownership of the
        // socket descriptor to this process on success.
        let status = unsafe { launch_activate_socket(name.as_ptr(), &mut descriptors, &mut count) };
        if status != 0 || descriptors.is_null() || count != 1 {
            if !descriptors.is_null() {
                unsafe {
                    libc::free(descriptors.cast());
                }
            }
            return Err(error(
                "SOCKET_ACTIVATION_FAILED",
                "startup",
                "launchd did not provide exactly one configured broker socket.",
                "Check the installed Broker socket activation service.",
            ));
        }
        let raw = unsafe { *descriptors };
        unsafe {
            libc::free(descriptors.cast());
        }
        let socket = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let listener = std::os::unix::net::UnixListener::from(socket);
        listener.set_nonblocking(true).map_err(|_| {
            error(
                "SOCKET_ACTIVATION_FAILED",
                "startup",
                "The launchd socket could not be made nonblocking.",
                "Check the installed Broker socket activation service.",
            )
        })?;
        let listener = UnixListener::from_std(listener).map_err(|_| {
            error(
                "SOCKET_ACTIVATION_FAILED",
                "startup",
                "The launchd socket could not be registered with Tokio.",
                "Check the installed Broker socket activation service.",
            )
        })?;
        serve_listener(listener, runtime, approver_uid).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (runtime, approver_uid);
        Err(error(
            "PROTECTED_BROKER_UNAVAILABLE",
            "startup",
            "Protected broker socket activation is implemented only for macOS.",
            "Use the synthetic harness or install the macOS protected service.",
        ))
    }
}

#[cfg(unix)]
fn peer_uid(stream: &UnixStream) -> fin_domain::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if result == 0 {
            return Ok(uid);
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::mem::{MaybeUninit, size_of};
        let mut credential = MaybeUninit::<libc::ucred>::uninit();
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credential.as_mut_ptr().cast(),
                &mut length,
            )
        };
        if result == 0 && length as usize == size_of::<libc::ucred>() {
            return Ok(unsafe { credential.assume_init() }.uid);
        }
    }
    Err(error(
        "IPC_PEER_UNAVAILABLE",
        "protocol",
        "The connected peer UID could not be verified.",
        "Use an operating system with supported local peer-credential checks.",
    ))
}

/// Confirm no unresolved operation currently reserves this exact physical
/// resource before enrollment changes or removes its mapping.
pub fn resource_has_pending_operations(
    store: &Store,
    resource: &ResourceId,
) -> fin_domain::Result<bool> {
    let records = store.list_records().map_err(|_| {
        error(
            "STORE_UNAVAILABLE",
            "configuration",
            "Pending-operation state could not be inspected.",
            "Preserve this resource mapping until the broker journal is available.",
        )
    })?;
    let mut pending = false;
    for value in records {
        pending |= record_reserves_resource(&value, resource)?;
    }
    Ok(pending)
}

fn record_reserves_resource(value: &Value, resource: &ResourceId) -> fin_domain::Result<bool> {
    let record: fin_domain::OperationRecord = serde_json::from_value(value.clone()).map_err(|_| {
        error("STORE_INVALID_RECORD", "configuration", "A durable operation record is malformed.", "Do not change account mappings until the journal has been repaired and reconciled.")
    })?;
    if record.status.terminal() {
        return Ok(false);
    }
    let preview = record.preview.as_ref().ok_or_else(|| {
        error(
            "STORE_INVALID_RECORD",
            "configuration",
            "A nonterminal operation has no resource binding.",
            "Preserve all account mappings until the operation record is repaired and reconciled.",
        )
    })?;
    Ok(preview
        .intent
        .debits
        .iter()
        .any(|debit| &debit.resource == resource)
        || preview
            .intent
            .maximum_costs
            .iter()
            .any(|cost| cost.location.resource() == resource)
        || preview.intent.locks.contains(resource))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::boundaries::BoundaryAuthorizationRequest;
    use fin_domain::{
        Amount, OperationKind, PreparedOperation, Quantity, ResourceAmount, Submission,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    struct ReadAdapter {
        captures: Arc<AtomicUsize>,
        pricing_snapshot: Option<Arc<Mutex<StateSnapshot>>>,
        pricing_preflight: Option<Arc<AtomicBool>>,
        identity_probe: Option<(Arc<AtomicBool>, Arc<AtomicUsize>)>,
    }

    #[async_trait]
    impl Adapter for ReadAdapter {
        fn definitions(&self) -> Vec<Definition> {
            let mut definitions = vec![
                Definition {
                    name: "fixture.read".into(),
                    description: "Synthetic read fixture".into(),
                    kind: OperationKind::Read,
                    execution_actor: ExecutionActor::AgentReadable,
                    input_schema: json!({"type":"object","additionalProperties":false}),
                    output_schema: json!({"type":"object"}),
                    error_schema: json!({"$ref":"FinError"}),
                    examples: vec![],
                    coverage: "Synthetic fixture".into(),
                    completion: "Read only".into(),
                    semantics: json!({}),
                },
                Definition {
                    name: "fixture.write".into(),
                    description: "Synthetic route-preflight fixture".into(),
                    kind: OperationKind::Write,
                    execution_actor: ExecutionActor::AgentInvocable,
                    input_schema: json!({"type":"object","additionalProperties":false}),
                    output_schema: json!({"type":"object"}),
                    error_schema: json!({"$ref":"FinError"}),
                    examples: vec![],
                    coverage: "Synthetic fixture".into(),
                    completion: "Read only".into(),
                    semantics: json!({}),
                },
            ];
            if self.identity_probe.is_some() {
                let mut identity = definitions[0].clone();
                identity.name = "binance.account-identity".into();
                definitions.push(identity);
            }
            definitions
        }
        async fn snapshot(&self) -> fin_domain::Result<StateSnapshot> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            if let Some(snapshot) = &self.pricing_snapshot {
                let mut snapshot = snapshot.lock().unwrap().clone();
                snapshot.observed_at = now();
                return Ok(snapshot);
            }
            Ok(StateSnapshot {
                state: FinancialState {
                    holdings: vec![ResourceAmount {
                        resource: ResourceId("fixture:account".into()),
                        amount: Amount {
                            asset: AssetId {
                                network: "fixture".into(),
                                reference: "USD".into(),
                                decimals: 6,
                            },
                            atoms: Quantity::new("5000000")?,
                        },
                    }],
                    ..FinancialState::default()
                },
                observed_at: now(),
                complete: true,
                coverage: vec!["fixture:account".into()],
                pending: vec![],
            })
        }
        async fn read(&self, operation: &str, _: &Value) -> fin_domain::Result<Value> {
            if operation == "binance.account-identity"
                && let Some((fail, calls)) = &self.identity_probe
            {
                calls.fetch_add(1, Ordering::SeqCst);
                if fail.load(Ordering::SeqCst) {
                    return Err(error(
                        "VENUE_PERMISSION",
                        "read",
                        "Synthetic credentials denied.",
                        "Repair the synthetic connection.",
                    ));
                }
            }
            Ok(json!({"synthetic":true}))
        }
        async fn prepare(&self, _: &str, _: &Value) -> fin_domain::Result<PreparedOperation> {
            Err(FinError::validation("Synthetic read-only adapter."))
        }
        async fn revalidate(&self, _: &PreparedOperation) -> fin_domain::Result<PreparedOperation> {
            Err(FinError::validation("Synthetic read-only adapter."))
        }
        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> fin_domain::Result<Submission> {
            Err(FinError::validation("Synthetic read-only adapter."))
        }
        async fn reconcile(&self, _: &OperationRecord) -> fin_domain::Result<Reconciliation> {
            Err(FinError::validation("Synthetic read-only adapter."))
        }
        async fn preflight_return_route(
            &self,
            operation: &str,
            input: &Value,
            context: &fin_domain::ReturnRouteContext,
        ) -> fin_domain::Result<fin_domain::ReturnRouteProof> {
            if !self
                .pricing_preflight
                .as_ref()
                .is_some_and(|allowed| allowed.load(Ordering::SeqCst))
            {
                return Err(error(
                    "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE",
                    "test",
                    "Synthetic preflight is unavailable.",
                    "No write can execute in this read-only price fixture.",
                ));
            }
            let resource = ResourceId("fixture:account".into());
            Ok(fin_domain::ReturnRouteProof {
                step_index: context.step_index,
                operation: operation.into(),
                input_hash: fin_domain::binding_hash(input)?,
                observed_at: now(),
                expires_at: (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
                intent: fin_domain::FinancialIntent {
                    debits: vec![],
                    maximum_costs: vec![],
                    expected: vec![fin_domain::EffectConstraint::Authority {
                        resource: resource.clone(),
                        authority: "synthetic-pricing-authority".into(),
                        before: "0".into(),
                        after: "0".into(),
                    }],
                    locks: vec![resource.clone()],
                },
                effects: vec![fin_domain::ProjectedEffect::AuthorityValue {
                    resource,
                    authority: "synthetic-pricing-authority".into(),
                    value: "0".into(),
                }],
                requires: vec![],
                evidence: json!({"synthetic_read_only_price_fixture":true}),
            })
        }
    }

    struct ReadOnlyAuthorizer;
    #[async_trait]
    impl Authorizer for ReadOnlyAuthorizer {
        async fn authorize(
            &self,
            _: &fin_domain::AuthorizationChallenge,
        ) -> fin_domain::Result<fin_domain::ApprovalAudit> {
            Err(FinError::new(
                "AUTHORIZATION_UNAVAILABLE",
                "test",
                "Writes are disabled in this synthetic fixture.",
                "Use read-only tests.",
            ))
        }
    }

    struct FixtureValuator;
    #[async_trait]
    impl AssetValuator for FixtureValuator {
        async fn usd_micros(&self, _: &Amount) -> fin_domain::Result<(String, String, String)> {
            Ok(("5000000".into(), now(), "synthetic fixture".into()))
        }
    }

    struct RecordingPriceValuator {
        calls: Arc<Mutex<Vec<(AssetId, usize)>>>,
        revision: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AssetValuator for RecordingPriceValuator {
        async fn usd_micros(
            &self,
            amount: &Amount,
        ) -> fin_domain::Result<(String, String, String)> {
            let revision = self.revision.load(Ordering::SeqCst);
            self.calls
                .lock()
                .unwrap()
                .push((amount.asset.clone(), revision));
            Ok((
                "1000000".into(),
                now(),
                format!("synthetic price revision {revision}"),
            ))
        }
    }

    struct PricingFixture {
        runtime: Arc<BrokerRuntime>,
        snapshot: Arc<Mutex<StateSnapshot>>,
        calls: Arc<Mutex<Vec<(AssetId, usize)>>>,
        revision: Arc<AtomicUsize>,
        allow_preflight: Arc<AtomicBool>,
    }

    fn price_holding(reference: &str, atoms: &str) -> ResourceAmount {
        ResourceAmount {
            resource: ResourceId("fixture:account".into()),
            amount: Amount {
                asset: AssetId {
                    network: "binance".into(),
                    reference: reference.into(),
                    decimals: 6,
                },
                atoms: Quantity::new(atoms).unwrap(),
            },
        }
    }

    fn price_snapshot(holdings: Vec<ResourceAmount>) -> StateSnapshot {
        StateSnapshot {
            state: FinancialState {
                holdings,
                authorities: vec![fin_domain::AuthorityState {
                    resource: ResourceId("fixture:account".into()),
                    authority: "synthetic-pricing-authority".into(),
                    value: "0".into(),
                }],
                ..FinancialState::default()
            },
            observed_at: now(),
            complete: true,
            coverage: vec!["fixture:account".into()],
            pending: vec![],
        }
    }

    fn pricing_fixture(configuration: BrokerConfigV2, observed: StateSnapshot) -> PricingFixture {
        let snapshot = Arc::new(Mutex::new(observed));
        let calls = Arc::new(Mutex::new(vec![]));
        let revision = Arc::new(AtomicUsize::new(1));
        let allow_preflight = Arc::new(AtomicBool::new(true));
        let runtime = compose_runtime(
            configuration,
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![Arc::new(ReadAdapter {
                    captures: Arc::new(AtomicUsize::new(0)),
                    pricing_snapshot: Some(snapshot.clone()),
                    pricing_preflight: Some(allow_preflight.clone()),
                    identity_probe: None,
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(RecordingPriceValuator {
                    calls: calls.clone(),
                    revision: revision.clone(),
                }),
            },
        )
        .unwrap();
        PricingFixture {
            runtime,
            snapshot,
            calls,
            revision,
            allow_preflight,
        }
    }

    #[tokio::test]
    async fn reusable_wallet_keeps_zero_protocol_assets_priced_without_expanding_to_indexer_noise()
    {
        let address = "0x1111111111111111111111111111111111111111";
        let mut config = config();
        config.networks = NetworkConfig::reviewed_evm_networks();
        config.connections.push(ConnectionConfig::Evm {
            name: "synthetic".into(),
            resource: ResourceId(format!("evm:{address}")),
            address: address.into(),
            credential_ref: None,
            signing_enabled: false,
        });
        let asset = |network: &str, reference: &str, decimals| AssetId {
            network: network.into(),
            reference: reference.into(),
            decimals,
        };
        let native = asset("ethereum:1", "native", 18);
        let weth = asset("ethereum:1", fin_ethereum::WETH, 18);
        let usdc = asset("ethereum:1", fin_ethereum::USDC, 6);
        let noise = asset("ethereum:1", "synthetic-indexer-noise", 6);
        let other_chain = asset(
            "ethereum:4663",
            "0x5fc5360d0400a0fd4f2af552add042d716f1d168",
            6,
        );
        let mut observed = price_snapshot(
            [
                native.clone(),
                weth.clone(),
                usdc.clone(),
                noise,
                other_chain,
            ]
            .into_iter()
            .map(|asset| ResourceAmount {
                resource: ResourceId(format!("{}:{address}", asset.network)),
                amount: Amount {
                    asset,
                    atoms: Quantity::zero(),
                },
            })
            .collect(),
        );
        // Financial scopes use RPC-verified AssetIds, not indexer symbols/prices.
        observed.coverage.push(format!("ethereum:1:{address}"));
        let fixture = pricing_fixture(config, observed.clone());
        let prices = fixture
            .runtime
            .trusted_asset_prices(&observed, &[])
            .await
            .unwrap();
        assert_eq!(
            prices
                .iter()
                .map(|price| price.asset.clone())
                .collect::<Vec<_>>(),
            vec![native, weth, usdc]
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 3);
        assert_eq!(fixture.snapshot.lock().unwrap().state, observed.state);
    }
    #[tokio::test]
    async fn trusted_prices_skip_a_thousand_zero_holdings_without_changing_snapshot() {
        let observed = price_snapshot(
            (0..1000)
                .map(|index| price_holding(&format!("EMPTY{index}"), "0"))
                .collect(),
        );
        let fixture = pricing_fixture(config(), observed.clone());
        let prices = fixture
            .runtime
            .trusted_asset_prices(&observed, &[])
            .await
            .unwrap();
        assert!(prices.is_empty());
        assert!(fixture.calls.lock().unwrap().is_empty());
        assert_eq!(fixture.snapshot.lock().unwrap().state, observed.state);
        assert_eq!(observed.state.holdings.len(), 1000);
        assert!(observed.complete);
    }

    #[tokio::test]
    async fn trusted_prices_keep_selected_and_configured_zero_assets_and_positive_holdings() {
        let selected = price_holding("USDC", "0").amount.asset;
        let positive = price_holding("BNB", "1000000");
        let configured = AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        };
        let mut observed = price_snapshot(vec![
            price_holding("DELISTED", "0"),
            price_holding("USDC", "0"),
            positive.clone(),
        ]);
        observed.state.holdings.push(ResourceAmount {
            resource: ResourceId("ethereum:1:synthetic".into()),
            amount: Amount {
                asset: configured.clone(),
                atoms: Quantity::zero(),
            },
        });
        observed.coverage.push("ethereum:1:synthetic".into());
        let mut configuration = config();
        configuration.connections.push(ConnectionConfig::Ethereum {
            name: "synthetic-read-only-wallet".into(),
            resource: ResourceId("ethereum:1:synthetic".into()),
            credential_ref: None,
            network: "ethereum:1".into(),
            address: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: false,
            assets: vec![configured.clone()],
        });
        let fixture = pricing_fixture(configuration, observed.clone());
        let prices = fixture
            .runtime
            .trusted_asset_prices(
                &observed,
                &[selected.clone(), configured.clone(), selected.clone()],
            )
            .await
            .unwrap();
        let actual = prices
            .into_iter()
            .map(|price| price.asset)
            .collect::<Vec<_>>();
        assert_eq!(actual, vec![positive.amount.asset, selected, configured]);
        assert_eq!(fixture.calls.lock().unwrap().len(), 3);
        assert_eq!(fixture.snapshot.lock().unwrap().state, observed.state);
    }

    #[tokio::test]
    async fn trusted_prices_deduplicate_full_network_reference_and_decimal_identity() {
        let assets = vec![
            AssetId {
                network: "binance".into(),
                reference: "USDT".into(),
                decimals: 36,
            },
            AssetId {
                network: "ethereum:1".into(),
                reference: "USDT".into(),
                decimals: 6,
            },
            AssetId {
                network: "binance".into(),
                reference: "USDT".into(),
                decimals: 6,
            },
        ];
        let observed = price_snapshot(
            assets
                .iter()
                .map(|asset| ResourceAmount {
                    resource: ResourceId("fixture:account".into()),
                    amount: Amount {
                        asset: asset.clone(),
                        atoms: Quantity::from_u64(1),
                    },
                })
                .collect(),
        );
        let fixture = pricing_fixture(config(), observed.clone());
        let prices = fixture
            .runtime
            .trusted_asset_prices(&observed, &assets)
            .await
            .unwrap();
        assert_eq!(
            prices
                .iter()
                .map(|price| price.asset.clone())
                .collect::<Vec<_>>(),
            assets
        );
        for price in prices {
            assert_eq!(
                price.unit_atoms,
                Quantity::from_decimal("1", price.asset.decimals).unwrap()
            );
        }
        assert_eq!(fixture.calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn independent_usdt_premium_blocks_99_token_allocation_over_100_usd() {
        let mut selected = price_holding("USDT", "0");
        selected.amount.asset.decimals = 36;
        selected.amount.atoms = Quantity::from_decimal("99", 36).unwrap();
        let observed = Arc::new(Mutex::new(price_snapshot(vec![selected.clone()])));
        let runtime = compose_runtime(
            config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![Arc::new(ReadAdapter {
                    captures: Arc::new(AtomicUsize::new(0)),
                    pricing_snapshot: Some(observed),
                    pricing_preflight: None,
                    identity_probe: None,
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(pricing::tests::fixed_valuator("1.02")),
            },
        )
        .unwrap();
        let step = fin_domain::PlanStep {
            operation: "fixture.write".into(),
            input: json!({}),
            maximum_fee_usd_micros: "0".into(),
            description: "synthetic cap check".into(),
        };
        let failure = runtime
            .begin_run(RunBeginRequest {
                allocations: vec![selected],
                planned_operations: vec![step.clone()],
                borrowed_proceeds_reserve_usd_micros: "0".into(),
                return_cost_reserve_usd_micros: "0".into(),
                unwind: vec![step],
            })
            .await
            .unwrap_err();
        assert_eq!(failure.code, "RUN_ALLOCATION_LIMIT");
        assert!(runtime.engine.operations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn replan_freshly_prices_original_allocation_when_current_local_holding_is_zero() {
        let original = price_holding("USDC", "1000000");
        let fixture = pricing_fixture(config(), price_snapshot(vec![original.clone()]));
        let step = fin_domain::PlanStep {
            operation: "fixture.write".into(),
            input: json!({}),
            maximum_fee_usd_micros: "0".into(),
            description: "synthetic no-change authority proof for pricing only".into(),
        };
        let run = fixture
            .runtime
            .begin_run(RunBeginRequest {
                allocations: vec![original.clone()],
                planned_operations: vec![step.clone()],
                borrowed_proceeds_reserve_usd_micros: "0".into(),
                return_cost_reserve_usd_micros: "0".into(),
                unwind: vec![step.clone()],
            })
            .await
            .unwrap();
        fixture.calls.lock().unwrap().clear();
        fixture.revision.store(2, Ordering::SeqCst);
        // The read-only fixture observes displaced custody. It intentionally
        // refuses any replacement proof after we inspect its fresh quotes.
        fixture.allow_preflight.store(false, Ordering::SeqCst);
        let displaced = ResourceAmount {
            resource: ResourceId("fixture:wallet".into()),
            amount: Amount {
                asset: AssetId {
                    network: "ethereum:1".into(),
                    reference: "synthetic-usdc".into(),
                    decimals: 6,
                },
                atoms: original.amount.atoms.clone(),
            },
        };
        {
            let mut current = fixture.snapshot.lock().unwrap();
            current.state.holdings[0].amount.atoms = Quantity::zero();
            current.state.holdings.push(displaced.clone());
            current.coverage.push(displaced.resource.0.clone());
        }
        let failure = fixture
            .runtime
            .replan_run(run.id, vec![step])
            .await
            .unwrap_err();
        assert_eq!(failure.code, "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE");
        assert_eq!(
            *fixture.calls.lock().unwrap(),
            vec![(displaced.amount.asset, 2), (original.amount.asset, 2)]
        );
        assert!(fixture.runtime.engine.operations().unwrap().is_empty());
    }

    fn config() -> BrokerConfigV2 {
        BrokerConfigV2 {
            version: 2,
            networks: vec![NetworkConfig::ethereum_mainnet()],
            deployments: vec![DeploymentConfig::ethereum_mainnet()],
            connections: vec![],
            policy: BrokerPolicy::beta(),
            approver_uid: 501,
            extensions: BTreeMap::new(),
        }
    }

    struct UnusedCredentials;
    impl fin_exchange::ccxt::CredentialProvider for UnusedCredentials {
        fn credentials(
            &self,
            _: &str,
        ) -> Result<fin_exchange::ccxt::Credentials, fin_exchange::ccxt::CcxtError> {
            panic!("composition and local management must not access credentials")
        }
    }
    impl BrokerCredentialProvider for UnusedCredentials {
        fn envelope_signer(
            &self,
            _: &fin_credentials::CredentialRef,
        ) -> fin_domain::Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)> {
            panic!("read-only fixture has no signing credentials")
        }
    }

    fn binance_only_config() -> BrokerConfigV2 {
        let mut config = config();
        config.connections.push(ConnectionConfig::Binance {
            name: "synthetic-master".into(),
            resource: ResourceId("binance:uid:271828:SPOT".into()),
            credential_ref: "550e8400-e29b-41d4-a716-446655440000".into(),
            role: "main".into(),
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
            physical_identity: "271828".into(),
            physical_identity_verified: true,
        });
        config
    }

    struct FixtureRuntimeLoader {
        database: PathBuf,
        loads: AtomicUsize,
        fail: bool,
    }
    #[async_trait]
    impl RuntimeLoader for FixtureRuntimeLoader {
        async fn load(&self, config: BrokerConfigV2) -> fin_domain::Result<Arc<BrokerRuntime>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(FinError::validation("Synthetic reload failure."));
            }
            let applications = build_protected_applications(&config, Arc::new(UnusedCredentials))?;
            compose_runtime(
                config,
                RuntimeDependencies {
                    store: Store::open(&self.database).unwrap(),
                    adapters: applications.adapters,
                    authorizer: Arc::new(ReadOnlyAuthorizer),
                    valuator: applications.valuator,
                },
            )
        }
    }

    #[tokio::test]
    async fn configuration_reload_lists_enrollment_immediately_and_preserves_pending_identity() {
        let directory = test_directory();
        let loader = FixtureRuntimeLoader {
            database: directory.join("operations.sqlite"),
            loads: AtomicUsize::new(0),
            fail: false,
        };
        let mut runtime = loader.load(config()).await.unwrap();
        let mut store = Store::open(&loader.database).unwrap();
        let record = OperationRecord {
            id: "synthetic-pending".into(),
            request_id: "synthetic-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "synthetic.write".into(),
            input: json!({}),
            status: fin_domain::Status::Submitted,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: Some(Submission {
                kind: "synthetic".into(),
                reference: "synthetic-original-submission".into(),
                data: json!({}),
            }),
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        store
            .create_request(
                &record.id,
                &record.request_id,
                &record.fingerprint,
                &record.operation,
                None,
                "submitted",
                &serde_json::to_value(&record).unwrap(),
                &now(),
            )
            .unwrap();
        drop(store);
        let original = runtime.clone();
        refresh_runtime(&mut runtime, config(), 501, &loader)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&runtime, &original));
        assert_eq!(loader.loads.load(Ordering::SeqCst), 1);

        refresh_runtime(&mut runtime, binance_only_config(), 501, &loader)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&runtime, &original));
        assert!(runtime.accepting.load(Ordering::SeqCst));
        let (mut client, server) = UnixStream::pair().unwrap();
        let serving_runtime = runtime.clone();
        let serving = tokio::spawn(async move {
            serve_connection(server, &serving_runtime, unsafe { libc::geteuid() })
                .await
                .unwrap();
        });
        client
            .write_all(b"{\"method\":\"connections\"}\n")
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut bytes)
            .await
            .unwrap();
        serving.await.unwrap();
        let response: BrokerResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(response.ok);
        assert_eq!(response.result.unwrap()[0]["name"], "synthetic-master");
        let records = runtime.engine.operations().unwrap();
        assert_eq!(records, vec![record]);
        refresh_runtime(&mut runtime, config(), 501, &loader)
            .await
            .unwrap();
        assert!(
            runtime
                .handle(BrokerRequest::Connections)
                .await
                .result
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(runtime.engine.operations().unwrap(), records);
        drop(runtime);
        drop(original);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn configuration_reload_failure_or_approver_change_never_replaces_runtime() {
        let directory = test_directory();
        let loader = FixtureRuntimeLoader {
            database: directory.join("operations.sqlite"),
            loads: AtomicUsize::new(0),
            fail: true,
        };
        let mut current = runtime();
        let original = current.clone();
        assert_eq!(
            refresh_runtime(&mut current, binance_only_config(), 501, &loader)
                .await
                .unwrap_err()
                .code,
            "INVALID_INPUT"
        );
        assert!(Arc::ptr_eq(&current, &original));
        assert!(current.accepting.load(Ordering::SeqCst));
        let mut changed = config();
        changed.approver_uid = 502;
        assert_eq!(
            refresh_runtime(&mut current, changed, 501, &loader)
                .await
                .unwrap_err()
                .code,
            "APPROVER_UID_MISMATCH"
        );
        assert_eq!(loader.loads.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&current, &original));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn pending_identity_verification_keeps_management_available_but_blocks_financial_dispatch()
     {
        let runtime = compose_runtime(
            binance_only_config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
            },
        )
        .unwrap();
        runtime
            .account_identity_check_pending
            .store(true, Ordering::SeqCst);
        for request in [
            BrokerRequest::DaemonStart,
            BrokerRequest::Connections,
            BrokerRequest::Doctor,
        ] {
            assert!(runtime.handle(request).await.ok);
        }
        for request in [
            BrokerRequest::Snapshot,
            BrokerRequest::Execute {
                request: fin_domain::OperationRequest {
                    request_id: "synthetic-blocked".into(),
                    operation: "binance.transfer".into(),
                    input: json!({}),
                    run_id: None,
                },
            },
        ] {
            let response = runtime.handle(request).await;
            assert!(!response.ok);
            assert_eq!(
                response.error.unwrap().code,
                "BINANCE_IDENTITY_OBSERVER_MISSING"
            );
        }
        assert!(runtime.engine.operations().unwrap().is_empty());
        assert!(
            runtime
                .account_identity_check_pending
                .load(Ordering::SeqCst)
        );
        assert!(runtime.handle(BrokerRequest::DaemonStop).await.ok);
    }

    #[tokio::test]
    async fn venue_identity_failure_returns_to_client_and_remains_pending_until_success() {
        let fail = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let runtime = compose_runtime(
            binance_only_config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![Arc::new(ReadAdapter {
                    captures: captures.clone(),
                    pricing_snapshot: None,
                    pricing_preflight: None,
                    identity_probe: Some((fail.clone(), calls.clone())),
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
            },
        )
        .unwrap();
        runtime
            .account_identity_check_pending
            .store(true, Ordering::SeqCst);
        for request in [
            BrokerRequest::DaemonStart,
            BrokerRequest::Doctor,
            BrokerRequest::Connections,
        ] {
            assert!(runtime.handle(request).await.ok);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let response = runtime.handle(BrokerRequest::Snapshot).await;
        assert_eq!(response.error.unwrap().code, "VENUE_PERMISSION");
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert!(
            runtime
                .account_identity_check_pending
                .load(Ordering::SeqCst)
        );
        assert!(runtime.handle(BrokerRequest::DaemonStart).await.ok);
        fail.store(false, Ordering::SeqCst);
        assert!(runtime.handle(BrokerRequest::Snapshot).await.ok);
        assert!(
            !runtime
                .account_identity_check_pending
                .load(Ordering::SeqCst)
        );
        assert!(runtime.handle(BrokerRequest::Snapshot).await.ok);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(captures.load(Ordering::SeqCst), 2);
        assert!(runtime.engine.operations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn binance_only_composition_serves_management_without_an_ethereum_wallet() {
        let config = binance_only_config();
        let applications = build_protected_applications(&config, Arc::new(UnusedCredentials))
            .expect("Binance enrollment must not require an Ethereum payout observer");
        let runtime = compose_runtime(
            config,
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: applications.adapters,
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: applications.valuator,
            },
        )
        .unwrap();
        for request in [BrokerRequest::DaemonStart, BrokerRequest::Doctor] {
            assert!(runtime.handle(request).await.ok);
        }
        let connections = runtime.handle(BrokerRequest::Connections).await;
        assert!(connections.ok);
        assert_eq!(connections.result.unwrap()[0]["name"], "synthetic-master");
        let withdrawal = runtime
            .handle(BrokerRequest::Preview {
                operation: "binance.withdraw".into(),
                input: json!({
                    "requestId":"synthetic-no-wallet", "account":"synthetic-master",
                    "destination":"unenrolled-wallet", "coin":"ETH", "amount":"0.001",
                    "network":"ETH", "walletType":"SPOT", "maxFee":"0.001",
                    "minReceiveAmount":"0.0001"
                }),
            })
            .await;
        assert!(!withdrawal.ok);
        assert_eq!(withdrawal.error.unwrap().code, "UNKNOWN_CONNECTION");
        assert!(runtime.handle(BrokerRequest::DaemonStop).await.ok);
    }

    #[test]
    fn binance_payout_destinations_cover_reusable_chains_and_preserve_legacy_scope() {
        let address = "0x1111111111111111111111111111111111111111";
        let mut config = binance_only_config();
        config.networks = NetworkConfig::reviewed_evm_networks();
        config.networks[0].confirmations = 7;
        config.networks[1].confirmations = 11;
        config.connections.push(ConnectionConfig::Evm {
            name: "synthetic-reusable".into(),
            resource: ResourceId(format!("evm:{address}")),
            address: address.into(),
            signing_enabled: false,
            credential_ref: None,
        });
        let legacy_assets = vec![
            AssetId {
                network: "ethereum:42161".into(),
                reference: "native".into(),
                decimals: 18,
            },
            AssetId {
                network: "ethereum:42161".into(),
                reference: "0x2222222222222222222222222222222222222222".into(),
                decimals: 7,
            },
        ];
        config.connections.push(ConnectionConfig::Ethereum {
            name: "synthetic-legacy".into(),
            resource: ResourceId("ethereum:42161:preserved-legacy-resource".into()),
            network: "ethereum:42161".into(),
            address: "0x3333333333333333333333333333333333333333".into(),
            signing_enabled: false,
            credential_ref: None,
            assets: legacy_assets.clone(),
        });
        config.validate().unwrap();
        let destinations = configured_binance_destinations(&config).unwrap();
        assert_eq!(destinations.len(), 4);
        let reusable = destinations
            .iter()
            .filter(|d| d.name == "synthetic-reusable")
            .collect::<Vec<_>>();
        assert_eq!(
            reusable.iter().map(|d| d.chain_id).collect::<Vec<_>>(),
            vec![1, 42161, 4663]
        );
        for destination in reusable {
            assert_eq!(
                destination.resource.0,
                format!("ethereum:{}:{address}", destination.chain_id)
            );
            assert_eq!(
                destination.assets["ETH"].network,
                format!("ethereum:{}", destination.chain_id)
            );
            assert_eq!(
                destination.confirmations,
                if destination.chain_id == 1 {
                    7
                } else if destination.chain_id == 42161 {
                    11
                } else {
                    2
                }
            );
        }
        let legacy = destinations
            .iter()
            .find(|d| d.name == "synthetic-legacy")
            .unwrap();
        assert_eq!(legacy.chain_id, 42161);
        assert_eq!(
            legacy.resource.0,
            "ethereum:42161:preserved-legacy-resource"
        );
        assert_eq!(legacy.confirmations, 11);
        assert_eq!(
            legacy.assets.values().cloned().collect::<BTreeSet<_>>(),
            legacy_assets.into_iter().collect()
        );
        assert!(build_protected_applications(&config, Arc::new(UnusedCredentials)).is_ok());
    }

    struct PayoutMetadataRpc {
        chain_id: u64,
        decimals: u8,
        deployed: bool,
        calls: Mutex<Vec<(String, Value)>>,
    }
    #[async_trait]
    impl fin_evm::EvmRpc for PayoutMetadataRpc {
        async fn request(&self, method: &str, params: Value) -> fin_domain::Result<Value> {
            self.calls.lock().unwrap().push((method.into(), params));
            match method {
                "eth_chainId" => Ok(json!(format!("0x{:x}", self.chain_id))),
                "eth_blockNumber" => Ok(json!("0x2a")),
                "eth_getCode" => Ok(json!(if self.deployed { "0x6000" } else { "0x" })),
                "eth_call" => Ok(json!(format!("0x{:064x}", self.decimals))),
                _ => panic!("unexpected synthetic payout RPC method: {method}"),
            }
        }
    }

    fn payout_metadata_verifier(
        chain_id: u64,
        rpc: Arc<PayoutMetadataRpc>,
    ) -> EthereumPayoutVerifier {
        let adapter = Arc::new(
            fin_ethereum::EthereumAdapter::new(
                fin_ethereum::EthereumConfig::wallet_only(chain_id, 2, vec![]),
                vec![fin_ethereum::WalletConnection {
                    name: "synthetic-payout".into(),
                    address: "0x1111111111111111111111111111111111111111".into(),
                    signing_enabled: false,
                }],
                Arc::new(fin_evm::EvmRuntime::new(rpc, chain_id, None)),
            )
            .unwrap(),
        );
        EthereumPayoutVerifier::new(BTreeMap::from([(chain_id, adapter)])).unwrap()
    }

    #[tokio::test]
    async fn payout_asset_resolution_verifies_generic_tokens_at_a_pinned_configured_chain() {
        use fin_binance::BinanceRecipientVerifier;
        let rpc = Arc::new(PayoutMetadataRpc {
            chain_id: 42161,
            decimals: 7,
            deployed: true,
            calls: Mutex::new(vec![]),
        });
        let verifier = payout_metadata_verifier(42161, rpc.clone());
        assert!(verifier.supports_chain(42161));
        assert!(!verifier.supports_chain(1));
        let contract = "0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa";
        let asset = verifier
            .resolve_asset(42161, "UNLISTED", Some(contract))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            asset,
            AssetId {
                network: "ethereum:42161".into(),
                reference: contract.to_ascii_lowercase(),
                decimals: 7,
            }
        );
        let calls = rpc.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            vec!["eth_chainId", "eth_blockNumber", "eth_getCode", "eth_call"]
        );
        assert_eq!(calls[2].1, json!([contract.to_ascii_lowercase(), "0x2a"]));
        assert_eq!(
            calls[3].1,
            json!([{"to": contract.to_ascii_lowercase(), "data": "0x313ce567"}, "0x2a"])
        );
    }

    #[tokio::test]
    async fn payout_asset_resolution_never_infers_native_from_unknown_chain_or_contract() {
        use fin_binance::BinanceRecipientVerifier;
        let rpc = Arc::new(PayoutMetadataRpc {
            chain_id: 4663,
            decimals: 18,
            deployed: true,
            calls: Mutex::new(vec![]),
        });
        let verifier = payout_metadata_verifier(4663, rpc.clone());
        for (chain, coin, contract) in [
            (1, "ETH", None),
            (56, "ETH", None),
            (4663, "BTC", None),
            (4663, "ETH", Some("unknown")),
            (
                4663,
                "ETH",
                Some("0x0000000000000000000000000000000000000000"),
            ),
        ] {
            assert!(
                verifier
                    .resolve_asset(chain, coin, contract)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert!(rpc.calls.lock().unwrap().is_empty());
        for contract in [None, Some("")] {
            assert_eq!(
                verifier
                    .resolve_asset(4663, "ETH", contract)
                    .await
                    .unwrap()
                    .unwrap(),
                AssetId {
                    network: "ethereum:4663".into(),
                    reference: "native".into(),
                    decimals: 18,
                }
            );
        }
        assert_eq!(rpc.calls.lock().unwrap().len(), 2);
        let unreviewed_rpc = Arc::new(PayoutMetadataRpc {
            chain_id: 56,
            decimals: 18,
            deployed: true,
            calls: Mutex::new(vec![]),
        });
        let unreviewed = payout_metadata_verifier(56, unreviewed_rpc.clone());
        assert!(
            unreviewed
                .resolve_asset(56, "ETH", None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(unreviewed_rpc.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn payout_asset_resolution_rejects_wrong_rpc_chain_missing_code_and_excess_precision() {
        use fin_binance::BinanceRecipientVerifier;
        let contract = "0x2222222222222222222222222222222222222222";
        for (rpc_chain, deployed, decimals, expected_code) in [
            (1, true, 6, "WRONG_CHAIN"),
            (42161, false, 6, "CONTRACT_UNAVAILABLE"),
        ] {
            let rpc = Arc::new(PayoutMetadataRpc {
                chain_id: rpc_chain,
                decimals,
                deployed,
                calls: Mutex::new(vec![]),
            });
            let verifier = payout_metadata_verifier(42161, rpc);
            assert_eq!(
                verifier
                    .resolve_asset(42161, "TOKEN", Some(contract))
                    .await
                    .unwrap_err()
                    .code,
                expected_code
            );
        }
        let rpc = Arc::new(PayoutMetadataRpc {
            chain_id: 42161,
            decimals: 37,
            deployed: true,
            calls: Mutex::new(vec![]),
        });
        assert!(
            payout_metadata_verifier(42161, rpc)
                .resolve_asset(42161, "TOKEN", Some(contract))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn configured_payout_destination_still_requires_its_ethereum_observer() {
        let mut config = binance_only_config();
        config.connections.push(ConnectionConfig::Ethereum {
            name: "synthetic-wallet".into(),
            resource: ResourceId("ethereum:1:synthetic-wallet".into()),
            network: "ethereum:1".into(),
            address: "0x0000000000000000000000000000000000000001".into(),
            signing_enabled: false,
            credential_ref: None,
            assets: vec![AssetId {
                network: "ethereum:1".into(),
                reference: "native".into(),
                decimals: 18,
            }],
        });
        assert!(build_protected_applications(&config, Arc::new(UnusedCredentials)).is_ok());
        config.deployments.clear();
        assert!(build_protected_applications(&config, Arc::new(UnusedCredentials)).is_ok());
        config.networks.clear();
        let Err(failure) = build_protected_applications(&config, Arc::new(UnusedCredentials))
        else {
            panic!("configured payouts must fail closed without their receipt observer")
        };
        assert_eq!(failure.code, "CONFIG_NETWORKS_REQUIRED");
        let Err(failure) = EthereumPayoutVerifier::new(BTreeMap::new()) else {
            panic!("an empty payout verifier must never be usable")
        };
        assert_eq!(failure.code, "PAYOUT_RPC_MISSING");
    }

    fn pricing_connection(plan: &str) -> ConnectionConfig {
        ConnectionConfig::CoinGecko {
            name: "synthetic-pricing".into(),
            resource: ResourceId("market-data:coingecko".into()),
            credential_ref: "550e8400-e29b-41d4-a716-446655440000".into(),
            api_plan: plan.into(),
        }
    }

    #[test]
    fn pricing_configuration_rejects_invalid_plan_identity_and_duplicate_source() {
        let mut cfg = config();
        cfg.connections.push(pricing_connection("demo"));
        assert!(cfg.validate().is_ok());
        for (plan, resource, credential) in [
            (
                "unknown",
                "market-data:coingecko",
                "550e8400-e29b-41d4-a716-446655440000",
            ),
            (
                "demo",
                "market-data:other",
                "550e8400-e29b-41d4-a716-446655440000",
            ),
            ("pro", "market-data:coingecko", "invalid-reference"),
        ] {
            cfg.connections[0] = ConnectionConfig::CoinGecko {
                name: "synthetic-pricing".into(),
                resource: ResourceId(resource.into()),
                credential_ref: credential.into(),
                api_plan: plan.into(),
            };
            assert_eq!(
                cfg.validate().unwrap_err().code,
                "CONFIG_PRICING_CONNECTION_INVALID"
            );
        }
        cfg.connections = vec![pricing_connection("demo"), pricing_connection("pro")];
        if let ConnectionConfig::CoinGecko { name, .. } = &mut cfg.connections[1] {
            *name = "synthetic-second".into();
        }
        assert!(cfg.validate().is_err());
    }

    struct PricingCredentials {
        fail: bool,
        calls: AtomicUsize,
    }
    impl fin_exchange::ccxt::CredentialProvider for PricingCredentials {
        fn credentials(
            &self,
            _: &str,
        ) -> Result<fin_exchange::ccxt::Credentials, fin_exchange::ccxt::CcxtError> {
            panic!("market-data enrollment must not obtain exchange credentials")
        }
    }
    impl BrokerCredentialProvider for PricingCredentials {
        fn usd_price_source(
            &self,
            credential: &fin_credentials::CredentialRef,
            plan: &str,
        ) -> fin_domain::Result<Arc<dyn fin_pricing::UsdPriceSource>> {
            assert_eq!(credential.as_str(), "550e8400-e29b-41d4-a716-446655440000");
            assert_eq!(plan, "pro");
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(FinError::validation(
                    "synthetic unavailable pricing credential",
                ));
            }
            Ok(Arc::new(pricing::tests::FixedUsdSource {
                price: "1.02".into(),
            }))
        }
        fn envelope_signer(
            &self,
            _: &fin_credentials::CredentialRef,
        ) -> fin_domain::Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)> {
            panic!("market-data enrollment must not obtain a wallet signer")
        }
    }

    #[tokio::test]
    async fn configured_pricing_source_is_shared_and_failure_has_no_anonymous_fallback() {
        let mut cfg = config();
        cfg.connections.push(pricing_connection("pro"));
        for fail in [false, true] {
            let credentials = Arc::new(PricingCredentials {
                fail,
                calls: AtomicUsize::new(0),
            });
            let result = build_protected_applications(&cfg, credentials.clone());
            assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
            if fail {
                assert!(result.is_err());
                continue;
            }
            let applications = result.unwrap();
            let amount = Amount {
                asset: AssetId {
                    network: "ethereum:1".into(),
                    reference: "native".into(),
                    decimals: 18,
                },
                atoms: fin_domain::Quantity::from_decimal("1", 18).unwrap(),
            };
            assert_eq!(
                applications.valuator.usd_micros(&amount).await.unwrap().0,
                "1020000"
            );
            let runtime = compose_runtime(
                cfg.clone(),
                RuntimeDependencies {
                    store: Store::in_memory().unwrap(),
                    adapters: applications.adapters,
                    authorizer: Arc::new(ReadOnlyAuthorizer),
                    valuator: applications.valuator,
                },
            )
            .unwrap();
            let response = runtime.handle(BrokerRequest::Connections).await;
            assert!(response.ok);
            let value = response.result.unwrap();
            let row = value
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["provider"] == "coingecko")
                .unwrap();
            assert_eq!(row["signing_enabled"], false);
            assert_eq!(row["details"]["financial_access"], false);
            assert_eq!(row["details"]["api_plan"], "pro");
            assert!(
                !value
                    .to_string()
                    .contains("550e8400-e29b-41d4-a716-446655440000")
            );
        }
    }

    fn runtime() -> Arc<BrokerRuntime> {
        runtime_with_capture_counter().0
    }

    fn runtime_with_capture_counter() -> (Arc<BrokerRuntime>, Arc<AtomicUsize>) {
        let captures = Arc::new(AtomicUsize::new(0));
        let runtime = compose_runtime(
            config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![Arc::new(ReadAdapter {
                    captures: captures.clone(),
                    pricing_snapshot: None,
                    pricing_preflight: None,
                    identity_probe: None,
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
            },
        )
        .unwrap();
        (runtime, captures)
    }

    #[tokio::test]
    async fn policy_reload_replaces_limits_without_reinstalling_or_creating_operations() {
        let directory = tempfile::tempdir().unwrap();
        let loader = FixtureRuntimeLoader {
            database: directory.path().join("operations.sqlite"),
            loads: AtomicUsize::new(0),
            fail: false,
        };
        let mut runtime = loader.load(config()).await.unwrap();
        assert_eq!(
            runtime
                .handle(BrokerRequest::PolicyShow)
                .await
                .result
                .unwrap()["max_movement_usd"],
            "100"
        );
        let original = runtime.clone();
        let mut changed = config();
        changed.policy.max_movement_usd_micros = "1000000000".into();
        changed.policy.max_run_allocation_usd_micros = "5000000000".into();
        refresh_runtime(&mut runtime, changed.clone(), 501, &loader)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&runtime, &original));
        assert!(runtime.accepting.load(Ordering::SeqCst));
        assert!(runtime.engine.operations().unwrap().is_empty());
        let shown = runtime
            .handle(BrokerRequest::PolicyShow)
            .await
            .result
            .unwrap();
        assert_eq!(shown["max_movement_usd"], "1000");
        assert_eq!(shown["max_run_allocation_usd"], "5000");
        assert!(shown.get("verification_max_run_allocation_usd").is_none());
        refresh_runtime(&mut runtime, changed, 501, &loader)
            .await
            .unwrap();
        assert_eq!(loader.loads.load(Ordering::SeqCst), 2);
    }

    fn install_test_stop_marker(runtime: &BrokerRuntime, path: PathBuf) {
        *runtime.daemon_marker_path.lock().unwrap() = path;
        runtime.protected_config_check.store(true, Ordering::SeqCst);
    }

    fn test_directory() -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("fin-broker-daemon-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }

    #[tokio::test]
    async fn daemon_stop_persists_until_explicit_start_after_restart() {
        let directory = test_directory();
        let marker = directory.join("daemon-stopped");
        let initial = runtime();
        install_test_stop_marker(&initial, marker.clone());

        let stopped = initial.handle(BrokerRequest::DaemonStop).await;
        assert!(stopped.ok);
        assert_eq!(stopped.result.unwrap()["status"], "stopping");
        assert!(!initial.accepting.load(Ordering::SeqCst));
        assert!(
            fin_protocol::daemon::stop_requested_at(&marker, initial.daemon_marker_owner_uid)
                .unwrap()
        );

        // A newly composed process sees the durable marker and blocks ordinary
        // requests until the human explicitly starts the service.
        let restarted = runtime();
        install_test_stop_marker(&restarted, marker.clone());
        let blocked = restarted.handle(BrokerRequest::Capabilities).await;
        assert_eq!(blocked.error.unwrap().code, "DAEMON_STOPPED");
        assert!(!restarted.accepting.load(Ordering::SeqCst));

        let started = restarted.handle(BrokerRequest::DaemonStart).await;
        assert!(started.ok);
        assert_eq!(started.result.as_ref().unwrap()["status"], "running");
        assert_eq!(started.result.unwrap()["pid"], std::process::id());
        assert!(
            !fin_protocol::daemon::stop_requested_at(&marker, restarted.daemon_marker_owner_uid)
                .unwrap()
        );
        assert!(restarted.handle(BrokerRequest::Capabilities).await.ok);
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn failed_stop_marker_write_does_not_claim_stopping_or_disable_acceptance() {
        let runtime = runtime();
        let missing_parent = test_directory().join("missing").join("daemon-stopped");
        install_test_stop_marker(&runtime, missing_parent);
        let response = runtime.handle(BrokerRequest::DaemonStop).await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "DAEMON_MARKER_INVALID");
        assert!(runtime.accepting.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn composite_snapshot_includes_all_observers_and_journal_completeness() {
        let runtime = runtime();
        let snapshot = runtime.snapshot().await.unwrap();
        assert!(snapshot.complete);
        assert!(snapshot.pending.is_empty());
        assert!(
            snapshot
                .coverage
                .iter()
                .any(|coverage| coverage == "fixture:account")
        );
        assert_eq!(snapshot.state.holdings.len(), 1);
        let definitions = runtime.engine.definitions().unwrap();
        assert!(
            definitions
                .iter()
                .any(|definition| definition.name == "fin.snapshot")
        );
    }

    #[tokio::test]
    async fn run_begin_reuses_the_engine_owned_snapshot_instead_of_recapturing() {
        let (runtime, captures) = runtime_with_capture_counter();
        let request = RunBeginRequest {
            allocations: vec![ResourceAmount {
                resource: ResourceId("fixture:account".into()),
                amount: Amount {
                    asset: AssetId {
                        network: "fixture".into(),
                        reference: "USD".into(),
                        decimals: 6,
                    },
                    atoms: Quantity::new("5000000").unwrap(),
                },
            }],
            planned_operations: vec![fin_domain::PlanStep {
                operation: "fixture.write".into(),
                input: json!({}),
                maximum_fee_usd_micros: "0".into(),
                description: "synthetic route step".into(),
            }],
            borrowed_proceeds_reserve_usd_micros: "0".into(),
            return_cost_reserve_usd_micros: "0".into(),
            unwind: vec![fin_domain::PlanStep {
                operation: "fixture.write".into(),
                input: json!({}),
                maximum_fee_usd_micros: "0".into(),
                description: "synthetic return step".into(),
            }],
        };
        let failure = runtime.begin_run(request).await.unwrap_err();
        assert_eq!(failure.code, "RETURN_ROUTE_PREFLIGHT_UNAVAILABLE");
        assert_eq!(captures.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn typed_dispatch_and_local_peer_framing_are_read_only_in_fixture() {
        let runtime = runtime();
        let response = runtime
            .handle(BrokerRequest::Read {
                operation: "fixture.read".into(),
                input: json!({}),
            })
            .await;
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["synthetic"], true);

        let (mut client, server) = UnixStream::pair().unwrap();
        let server_runtime = runtime.clone();
        let serving = tokio::spawn(async move {
            serve_connection(server, &server_runtime, unsafe { libc::geteuid() })
                .await
                .unwrap();
        });
        client
            .write_all(b"{\"method\":\"capabilities\"}\n")
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut bytes)
            .await
            .unwrap();
        serving.await.unwrap();
        let response: BrokerResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["version"], 2);
    }

    #[tokio::test]
    async fn idle_slow_and_oversized_peers_cannot_block_the_next_request() {
        let path =
            std::path::PathBuf::from(format!("/tmp/fin-broker-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&path).unwrap();
        let runtime = runtime();
        let expected_uid = unsafe { libc::geteuid() };
        let timeouts = IpcTimeouts {
            frame_read: Duration::from_millis(25),
            response_write: Duration::from_millis(250),
            configuration_lock: Duration::from_millis(25),
        };
        let serving = tokio::spawn(serve_listener_with_timeouts(
            listener,
            runtime,
            expected_uid,
            timeouts,
        ));

        let mut idle = UnixStream::connect(&path).await.unwrap();
        let mut idle_response = Vec::new();
        tokio::time::timeout(
            Duration::from_millis(200),
            tokio::io::AsyncReadExt::read_to_end(&mut idle, &mut idle_response),
        )
        .await
        .expect("idle peer should be closed at the frame deadline")
        .unwrap();
        assert!(idle_response.is_empty());

        let mut slow = UnixStream::connect(&path).await.unwrap();
        slow.write_all(b"{").await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ = slow.write_all(b"\"method\":\"capabilities\"}\n").await;
        let mut slow_response = Vec::new();
        tokio::time::timeout(
            Duration::from_millis(200),
            tokio::io::AsyncReadExt::read_to_end(&mut slow, &mut slow_response),
        )
        .await
        .expect("slow peer should be closed at the frame deadline")
        .unwrap();
        assert!(slow_response.is_empty());

        let mut oversized = UnixStream::connect(&path).await.unwrap();
        let _ = oversized
            .write_all(&vec![b'a'; fin_protocol::MAX_FRAME_BYTES + 1])
            .await;
        let _ = oversized.write_all(b"\n").await;
        let mut oversized_response = Vec::new();
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::io::AsyncReadExt::read_to_end(&mut oversized, &mut oversized_response),
        )
        .await
        .expect("oversized peer should be closed promptly")
        .unwrap();
        assert!(oversized_response.is_empty());

        let mut valid = UnixStream::connect(&path).await.unwrap();
        valid
            .write_all(b"{\"method\":\"daemon_stop\"}\n")
            .await
            .unwrap();
        let mut valid_response = Vec::new();
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::io::AsyncReadExt::read_to_end(&mut valid, &mut valid_response),
        )
        .await
        .expect("valid peer should receive response")
        .unwrap();
        let response: BrokerResponse = serde_json::from_slice(&valid_response).unwrap();
        assert!(response.ok);
        serving.await.unwrap().unwrap();
        let _ = fs::remove_file(path);
    }

    #[test]
    fn v2_config_requires_verified_physical_identity_and_safe_rpc_urls() {
        let mut config = config();
        config.networks[0].rpc_urls = vec!["https://user:secret@example.invalid".into()];
        assert_eq!(
            config.validate().unwrap_err().code,
            "CONFIG_NETWORK_INVALID"
        );
        config.networks[0] = NetworkConfig::ethereum_mainnet();
        config.connections.push(ConnectionConfig::Binance {
            name: "main".into(),
            resource: ResourceId("binance:main".into()),
            credential_ref: "550e8400-e29b-41d4-a716-446655440000".into(),
            role: "main".into(),
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
            physical_identity: "synthetic-unverified".into(),
            physical_identity_verified: false,
        });
        assert_eq!(
            config.validate().unwrap_err().code,
            "CONFIG_BINANCE_CONNECTION_INVALID"
        );
        if let ConnectionConfig::Binance {
            resource,
            physical_identity,
            physical_identity_verified,
            ..
        } = config.connections.first_mut().unwrap()
        {
            *physical_identity = "271828".into();
            *resource = ResourceId("binance:uid:271828:SPOT".into());
            *physical_identity_verified = true;
        }
        assert!(config.validate().is_ok());
        if let ConnectionConfig::Binance { resource, .. } = config.connections.first_mut().unwrap()
        {
            *resource = ResourceId("binance:uid:314159:SPOT".into());
        }
        assert_eq!(
            config.validate().unwrap_err().code,
            "CONFIG_BINANCE_CONNECTION_INVALID"
        );
    }

    #[test]
    fn unknown_durable_operation_bindings_block_enrollment_mutations() {
        let resource = ResourceId("binance:uid:271828:SPOT".into());
        assert_eq!(
            record_reserves_resource(&json!({"status":"submitted"}), &resource)
                .unwrap_err()
                .code,
            "STORE_INVALID_RECORD"
        );
        let no_preview = fin_domain::OperationRecord {
            id: "synthetic-pending".into(),
            request_id: "synthetic-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "synthetic.write".into(),
            input: json!({}),
            status: fin_domain::Status::Submitted,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: None,
            dispatch_evidence: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        assert_eq!(
            record_reserves_resource(&serde_json::to_value(no_preview).unwrap(), &resource)
                .unwrap_err()
                .code,
            "STORE_INVALID_RECORD"
        );
    }
    #[tokio::test]
    async fn retired_run_mutations_do_not_capture_state_or_change_historical_records() {
        let (runtime, captures) = runtime_with_capture_counter();
        let before = captures.load(Ordering::SeqCst);
        for request in [
            BrokerRequest::RunBegin {
                request: RunBeginRequest {
                    allocations: vec![],
                    borrowed_proceeds_reserve_usd_micros: "0".into(),
                    return_cost_reserve_usd_micros: "0".into(),
                    planned_operations: vec![],
                    unwind: vec![],
                },
            },
            BrokerRequest::RunReplan {
                id: "legacy".into(),
                remaining: vec![],
            },
            BrokerRequest::RunFinish {
                id: "legacy".into(),
            },
        ] {
            let response = runtime.handle(request).await;
            assert_eq!(response.error.unwrap().code, "RUN_RETIRED");
        }
        assert_eq!(captures.load(Ordering::SeqCst), before);
        assert!(runtime.engine.operations().unwrap().is_empty());
        let methods = protocol_methods();
        assert!(!methods.contains(&"run_status"));
        assert!(!methods.contains(&"run_begin") && !methods.contains(&"run_finish"));
    }

    #[test]
    fn allocation_display_preserves_totals_larger_than_a_single_policy_limit() {
        assert_eq!(format_micros_total("0"), "0");
        assert_eq!(format_micros_total("1"), "0.000001");
        assert_eq!(
            format_micros_total("20000000000000000000"),
            "20000000000000"
        );
    }

    #[tokio::test]
    async fn boundary_methods_are_advertised_and_requests_reach_the_engine() {
        let methods = protocol_methods();
        for method in [
            "preview_boundary_authorization",
            "authorize_boundary",
            "boundary_status",
            "revoke_boundary",
            "preview_boundary",
            "execute_boundary",
            "replace_boundary_candidate",
        ] {
            assert!(
                methods.contains(&method),
                "missing protocol method {method}"
            );
        }

        let runtime = runtime();
        let request = BoundaryAuthorizationRequest {
            request_id: "synthetic-boundary-intent".into(),
            operation: "fixture.write".into(),
            input: json!({}),
            bounds: json!({}),
            valid_for_seconds: 600,
            replaces: None,
        };
        let preview = runtime
            .handle(BrokerRequest::PreviewBoundaryAuthorization {
                request: request.clone(),
            })
            .await;
        assert!(!preview.ok);
        assert_ne!(preview.error.unwrap().code, "INVALID_PROTOCOL_FRAME");

        for request in [
            BrokerRequest::BoundaryStatus {
                id: "synthetic-missing-authorization".into(),
            },
            BrokerRequest::RevokeBoundary {
                id: "synthetic-missing-authorization".into(),
            },
            BrokerRequest::PreviewBoundary {
                id: "synthetic-missing-authorization".into(),
                operation: "fixture.write".into(),
                input: json!({}),
            },
            BrokerRequest::ReplaceBoundaryCandidate {
                authorization_id: "synthetic-missing-authorization".into(),
                previous_attempt_id: "synthetic-old-attempt".into(),
                request: fin_domain::OperationRequest {
                    request_id: "synthetic-replacement-attempt".into(),
                    operation: "fixture.write".into(),
                    input: json!({}),
                    run_id: None,
                },
            },
        ] {
            let response = runtime.handle(request).await;
            assert_eq!(
                response.error.unwrap().code,
                "BOUNDARY_AUTHORIZATION_NOT_FOUND"
            );
        }
    }
}
