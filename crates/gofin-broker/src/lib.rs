//! Protected composition and local protocol endpoint for GoFin.
//!
//! Financial callers can access only typed protocol methods. Production startup
//! is rooted at the installed paths; the separately named composition entry
//! point exists for synthetic adapter/credential harnesses.

mod amm_deployments;
mod artifacts;
mod bridge;
pub mod coinbase;
pub mod coinbase_deposit;
mod coinbase_funding;
pub mod configure;
pub mod deposit;
pub mod enrollment;
mod evm_public_data;
mod evm_router;
#[doc(hidden)]
pub use evm_router::EvmNetworkRouter;
pub mod hyperliquid_deposit;
mod hyperliquid_transport;
pub mod hyperliquid_withdrawal;
pub mod kraken;
mod kraken_aliases;
mod kraken_deposit;
mod kraken_pricing;
#[cfg(test)]
mod kraken_solana_funding_tests;
mod policy;
mod pricing;
mod pricing_cache;
mod public_submission;
pub mod solana_cex;
#[cfg(test)]
mod solana_funding_tests;
#[cfg(test)]
mod test_support;
mod wallet_router;
pub use bridge::BridgeAdapter;
pub use deposit::BinanceEthereumDepositAdapter;
pub use hyperliquid_deposit::HyperliquidEthereumDepositAdapter;
pub use hyperliquid_withdrawal::HyperliquidArbitrumWithdrawalProof;
pub use pricing::SharedUsdValuator;

use async_trait::async_trait;
use chrono::Utc;
use gofin_authorization::{MacOsAuthorizer, validate_installation, validate_metadata};
use gofin_domain::{
    Adapter, Amount, AssetId, AssetPrice, Authorizer, Definition, ExecutionActor, FinancialState,
    GoFinError, OperationKind, OperationRecord, PreparedOperation, Reconciliation, ResourceId,
    SnapshotScope, StateSnapshot, Submission, SubmissionJournal, now,
};
use gofin_engine::{Engine, EngineOptions};
#[cfg(test)]
use gofin_engine::{RunAllocation, RunPlan, RunResult};
#[cfg(test)]
use gofin_protocol::RunBeginRequest;
use gofin_protocol::{BrokerRequest, BrokerResponse, encode_response, read_request_envelope};
use gofin_store::Store;
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
    sync::{Mutex as AsyncMutex, RwLock, Semaphore},
    task::JoinSet,
};

pub const CONFIG_PATH: &str = "/Library/GoFin/private/config.json";
pub const OPERATIONS_PATH: &str = "/Library/GoFin/private/operations.sqlite";
pub const SOCKET_PATH: &str = "/Library/GoFin/run/broker.sock";
pub const MAINTENANCE_LOCK_PATH: &str = "/Library/GoFin/private/maintenance.lock";
pub const MAX_CONFIG_BYTES: usize = 1_048_576;
const IPC_FRAME_READ_TIMEOUT: Duration = Duration::from_secs(10);
const IPC_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const IPC_CONFIGURATION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const BINANCE_ACCOUNT_IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
const CONFIGURATION_PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);
const NON_RPC_OBSERVER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BROKER_READ_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_READ_OBSERVERS: usize = 4;
pub const MAX_BROKER_CONNECTIONS: usize = 16;

fn error(code: &str, stage: &str, message: &str, action: &str) -> GoFinError {
    GoFinError::new(code, stage, message, action)
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
    #[serde(default)]
    pub weth: Option<String>,
    #[serde(default)]
    pub snapshot_tokens: Vec<String>,
    #[serde(default)]
    pub uniswap: Option<gofin_ethereum::UniswapConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ramses: Option<gofin_ethereum::RamsesConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub v3_deployments: BTreeMap<gofin_ethereum::V3Dex, gofin_ethereum::V3DeploymentConfig>,
    #[serde(default)]
    pub aave: Option<gofin_ethereum::AaveConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aave_markets: Vec<gofin_ethereum::AaveConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectionConfig {
    Coinbase {
        name: String,
        resource: ResourceId,
        credential_ref: String,
        physical_identity: String,
        physical_identity_verified: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portfolio_id: Option<String>,
    },
    #[serde(rename = "coinbaseexchange")]
    CoinbaseExchange {
        name: String,
        resource: ResourceId,
        credential_ref: String,
        physical_identity: String,
        physical_identity_verified: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portfolio_id: Option<String>,
    },
    #[serde(rename = "coinbaseinternational")]
    CoinbaseInternational {
        name: String,
        resource: ResourceId,
        credential_ref: String,
        physical_identity: String,
        physical_identity_verified: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portfolio_id: Option<String>,
    },
    Kraken {
        /// Human-selected default alias; authenticated owned-wallet membership is checked on use.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_wallet_account_id: Option<String>,
        name: String,
        resource: ResourceId,
        credential_ref: String,
        physical_identity: String,
        physical_identity_verified: bool,
    },
    #[serde(rename = "krakenfutures")]
    KrakenFutures {
        name: String,
        resource: ResourceId,
        credential_ref: String,
        physical_identity: String,
        physical_identity_verified: bool,
    },
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
    Hyperliquid {
        name: String,
        resource: ResourceId,
        owner: String,
        credential_ref: Option<String>,
        signing_enabled: bool,
        #[serde(default)]
        execution_scope: gofin_hyperliquid::HyperliquidExecutionScope,
        /// Runtime-only provenance for links derived from reusable EVM wallets.
        /// This must never be persisted as an enrolled connection.
        #[serde(skip)]
        auto_derived: bool,
    },
    Solana {
        name: String,
        resource: ResourceId,
        address: String,
        credential_ref: Option<String>,
        signing_enabled: bool,
        network: String,
    },
    Jupiter {
        name: String,
        resource: ResourceId,
        credential_ref: String,
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
        Self::from_reviewed(gofin_networks::by_chain_id(1).expect("Ethereum is reviewed"))
    }
    pub fn reviewed_evm_networks() -> Vec<Self> {
        gofin_networks::all()
            .iter()
            .map(Self::from_reviewed)
            .collect()
    }

    fn from_reviewed(network: &gofin_networks::EvmNetwork) -> Self {
        Self {
            id: network.network_id.into(),
            chain_id: network.chain_id,
            rpc_urls: network.rpc_urls.iter().map(|url| (*url).into()).collect(),
            confirmations: network.minimum_confirmations,
            snapshot_tokens: vec![],
        }
    }
}

impl DeploymentConfig {
    pub fn reviewed_amm_networks() -> Vec<Self> {
        NetworkConfig::reviewed_evm_networks()
            .into_iter()
            .filter_map(|network| {
                let uniswap = amm_deployments::reviewed_uniswap(network.chain_id);
                let ramses = amm_deployments::reviewed_ramses(network.chain_id);
                let v3_deployments = amm_deployments::reviewed_v3_deployments(network.chain_id);
                let aave = if network.chain_id == 1 {
                    gofin_ethereum::EthereumConfig::default().aave
                } else if network.chain_id == 8453 {
                    gofin_ethereum::EthereumConfig::base_aave(vec![]).aave
                } else {
                    gofin_ethereum::aave::reviewed_markets(network.chain_id)
                        .first()
                        .map(|m| gofin_ethereum::AaveConfig {
                            pool: m.pool.clone(),
                            oracle: m.oracle.clone(),
                        })
                };
                (uniswap.is_some()
                    || ramses.is_some()
                    || !v3_deployments.is_empty()
                    || aave.is_some())
                .then(|| Self {
                    network: network.id,
                    weth: (network.chain_id == 8453)
                        .then(|| "0x4200000000000000000000000000000000000006".into()),
                    snapshot_tokens: vec![],
                    uniswap,
                    ramses,
                    v3_deployments,
                    aave,
                    aave_markets: gofin_ethereum::aave::reviewed_markets(network.chain_id)
                        .into_iter()
                        .map(|m| gofin_ethereum::AaveConfig {
                            pool: m.pool,
                            oracle: m.oracle,
                        })
                        .collect(),
                })
            })
            .collect()
    }

    /// Backwards-compatible name for callers that only require Uniswap profiles.
    pub fn reviewed_uniswap_networks() -> Vec<Self> {
        Self::reviewed_amm_networks()
    }
    pub fn ethereum_mainnet() -> Self {
        let defaults = gofin_ethereum::EthereumConfig::default();
        Self {
            network: "ethereum:1".into(),
            weth: defaults.weth,
            snapshot_tokens: defaults.snapshot_tokens,
            uniswap: amm_deployments::reviewed_uniswap(1),
            ramses: amm_deployments::reviewed_ramses(1),
            v3_deployments: amm_deployments::reviewed_v3_deployments(1),
            aave: defaults.aave,
            aave_markets: defaults.aave_markets,
        }
    }
}

impl BrokerPolicy {
    pub fn beta() -> Self {
        Self {
            require_run_for_writes: false,
            max_movement_usd_micros: gofin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS
                .to_string(),
            max_run_allocation_usd_micros: gofin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS
                .to_string(),
            preview_ttl_seconds: 90,
            allow_unpriced_exchange_assets: false,
        }
    }
}

impl BrokerConfigV2 {
    pub fn validate(&self) -> gofin_domain::Result<()> {
        kraken_aliases::validate(self)?;
        if self.version != 2 || self.approver_uid < 501 {
            return Err(error(
                "CONFIG_VERSION_OR_APPROVER_INVALID",
                "configuration",
                "Configuration version 2 and a human approver UID are required.",
                "Use the protected human enrollment flow to repair broker configuration.",
            ));
        }
        let has_solana_wallet = self
            .connections
            .iter()
            .any(|connection| matches!(connection, ConnectionConfig::Solana { .. }));
        if self.networks.is_empty() && !has_solana_wallet {
            return Err(error(
                "CONFIG_NETWORKS_REQUIRED",
                "configuration",
                "At least one reviewed EVM network is required.",
                "Configure a supported Ethereum network and reviewed deployments.",
            ));
        }
        let jupiter_connections = self
            .connections
            .iter()
            .filter(|connection| matches!(connection, ConnectionConfig::Jupiter { .. }))
            .count();
        if jupiter_connections > 1 || (jupiter_connections > 0 && !has_solana_wallet) {
            return Err(error(
                "CONFIG_JUPITER_SCOPE_INVALID",
                "configuration",
                "At most one optional Jupiter service credential may be attached to an enrolled Solana wallet profile.",
                "Remove the duplicate service entry or enroll a Solana wallet first.",
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
                || !gofin_networks::supports_chain(network.chain_id)
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
                ConnectionConfig::Coinbase {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    portfolio_id,
                    ..
                }
                | ConnectionConfig::CoinbaseExchange {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    portfolio_id,
                    ..
                }
                | ConnectionConfig::CoinbaseInternational {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    portfolio_id,
                    ..
                } => {
                    let provider = match connection {
                        ConnectionConfig::Coinbase { .. } => "coinbase",
                        ConnectionConfig::CoinbaseExchange { .. } => "coinbaseexchange",
                        _ => "coinbaseinternational",
                    };
                    let valid_identity = |id: &str| {
                        !id.is_empty()
                            && id.len() <= 128
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    };
                    if !valid_identity(physical_identity)
                        || portfolio_id
                            .as_deref()
                            .is_some_and(|id| !valid_identity(id) || id != physical_identity)
                        || resource.0 != format!("{provider}:portfolio:{physical_identity}")
                        || gofin_credentials::CredentialRef::new(credential_ref).is_err()
                    {
                        return Err(error(
                            "CONFIG_COINBASE_CONNECTION_INVALID",
                            "configuration",
                            "Coinbase product, portfolio identity or credential reference is invalid.",
                            "Repair the verified product and portfolio binding through protected enrollment.",
                        ));
                    }
                    (name, resource)
                }
                ConnectionConfig::Kraken {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    ..
                }
                | ConnectionConfig::KrakenFutures {
                    name,
                    resource,
                    credential_ref,
                    physical_identity,
                    ..
                } => {
                    if let ConnectionConfig::Kraken {
                        default_wallet_account_id: Some(id),
                        ..
                    } = connection
                        && (id.is_empty()
                            || id.len() > 128
                            || !id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)))
                    {
                        return Err(error(
                            "CONFIG_KRAKEN_CONNECTION_INVALID",
                            "configuration",
                            "The default wallet account ID is invalid.",
                            "Update the relationship in your human Terminal.",
                        ));
                    }
                    let provider = if matches!(connection, ConnectionConfig::Kraken { .. }) {
                        "kraken"
                    } else {
                        "krakenfutures"
                    };
                    if physical_identity.is_empty()
                        || physical_identity.len() > 128
                        || !physical_identity
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                        || resource.0 != format!("{provider}:account:{physical_identity}")
                        || gofin_credentials::CredentialRef::new(credential_ref).is_err()
                    {
                        return Err(error(
                            "CONFIG_KRAKEN_CONNECTION_INVALID",
                            "configuration",
                            "Kraken product, account identity or credential reference is invalid.",
                            "Repair this connection in your human Terminal.",
                        ));
                    }
                    (name, resource)
                }
                ConnectionConfig::CoinGecko {
                    name,
                    resource,
                    credential_ref,
                    api_plan,
                } => {
                    if resource.0 != "market-data:coingecko"
                        || !matches!(api_plan.as_str(), "demo" | "pro")
                        || gofin_credentials::CredentialRef::new(credential_ref).is_err()
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
                ConnectionConfig::Hyperliquid {
                    name,
                    resource,
                    owner,
                    credential_ref,
                    signing_enabled,
                    execution_scope,
                    ..
                } => {
                    let execution_address = execution_scope.address().unwrap_or(owner);
                    if !gofin_hyperliquid::valid_address(owner)
                        || owner[2..].bytes().all(|byte| byte == b'0')
                        || owner.bytes().any(|byte| byte.is_ascii_uppercase())
                        || !gofin_hyperliquid::valid_address(execution_address)
                        || execution_address[2..].bytes().all(|byte| byte == b'0')
                        || execution_address
                            .bytes()
                            .any(|byte| byte.is_ascii_uppercase())
                        || resource.0
                            != format!("hyperliquid:mainnet:{}:account:primary", execution_address)
                        || (execution_scope.address().is_some()
                            && execution_address.eq_ignore_ascii_case(owner))
                        || (*signing_enabled != credential_ref.is_some())
                        || credential_ref.as_ref().is_some_and(|reference| {
                            gofin_credentials::CredentialRef::new(reference).is_err()
                        })
                    {
                        return Err(error(
                            "CONFIG_HYPERLIQUID_CONNECTION_INVALID",
                            "configuration",
                            "Hyperliquid owner or protected signer mapping is invalid.",
                            "Repair the connection through protected human enrollment.",
                        ));
                    }
                    (name, resource)
                }
                ConnectionConfig::Solana {
                    name,
                    resource,
                    address,
                    credential_ref,
                    signing_enabled,
                    network,
                } => {
                    let decoded = (address.len() <= 44)
                        .then(|| bs58::decode(address).into_vec().ok())
                        .flatten();
                    if network != "solana:mainnet-beta"
                        || decoded.as_ref().is_none_or(|bytes| bytes.len() != 32)
                        || resource.0 != format!("{network}:{address}")
                        || (*signing_enabled != credential_ref.is_some())
                        || credential_ref.as_ref().is_some_and(|reference| {
                            gofin_credentials::CredentialRef::new(reference).is_err()
                        })
                    {
                        return Err(error(
                            "CONFIG_SOLANA_CONNECTION_INVALID",
                            "configuration",
                            "Solana address, cluster or protected signer mapping is invalid.",
                            "Repair the wallet through protected enrollment.",
                        ));
                    }
                    (name, resource)
                }
                ConnectionConfig::Jupiter {
                    name,
                    resource,
                    credential_ref,
                } => {
                    if resource.0 != "jupiter:service"
                        || gofin_credentials::CredentialRef::new(credential_ref).is_err()
                    {
                        return Err(error(
                            "CONFIG_JUPITER_CREDENTIAL_INVALID",
                            "configuration",
                            "Jupiter service credential mapping is invalid.",
                            "Repair the credential through protected connection enrollment.",
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
                            && (parent.is_some() != email.is_some()
                                || email.as_deref().is_some_and(str::is_empty)))
                        || !canonical_uid(physical_identity)
                        || !physical_identity_verified
                        || resource.0 != format!("binance:uid:{physical_identity}:SPOT")
                        || gofin_credentials::CredentialRef::new(credential_ref).is_err()
                    {
                        return Err(error(
                            "CONFIG_BINANCE_CONNECTION_INVALID",
                            "configuration",
                            "Binance SPOT connection metadata or verified physical identity is invalid.",
                            "Verify the account identity; enroll a main connection only when parent-authorized hierarchy access is needed.",
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
                            gofin_credentials::CredentialRef::new(reference).is_err()
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
                            gofin_credentials::CredentialRef::new(value).is_err()
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
            if gofin_domain::policy::policy_micros(value).is_err() {
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
    let mut definitions = gofin_binance::operation_definitions();
    definitions.extend(gofin_exchange::DefaultExchangeProfile::definitions());
    definitions.extend(gofin_kraken::contracts::definitions("kraken"));
    definitions.extend(gofin_kraken::contracts::definitions("krakenfutures"));
    definitions.extend(gofin_coinbase::contracts::definitions());
    definitions.push(coinbase_deposit::definition());
    definitions.push(gofin_kraken::funding::deposit_definition());
    definitions.extend(gofin_ethereum::operation_definitions());
    definitions.extend(gofin_solana::operation_definitions());
    definitions.extend(gofin_jupiter::operation_definitions());
    definitions.push(BinanceEthereumDepositAdapter::operation_definition());
    definitions.push(snapshot_definition());
    definitions.extend(BridgeAdapter::operation_definitions());
    definitions.extend(gofin_hyperliquid::catalog::definitions());
    let mut definitions = wallet_router::merge_wallet_definitions(definitions);
    for definition in &mut definitions {
        if definition.semantics["standalone_only"] == true {
            definition.semantics["plan_authorization"] = json!({
                "mode":"unsupported",
                "reason":"This operation requires standalone exact approval of venue-calculated fees."
            });
            continue;
        }
        // Bridge transfers use their own persisted finite-boundary grants;
        // nominated plan-only wrappers advertise their adapter-owned
        // plan_permission contract instead of this generic exact-input mode.
        if definition.name.starts_with("bridge.")
            || (definition.semantics["plan_only"] == true
                && definition.semantics["plan_permission"]["mode"] == "agent_nominated")
        {
            continue;
        }
        let eligible = matches!(
            (&definition.kind, &definition.execution_actor),
            (
                OperationKind::Write,
                gofin_domain::ExecutionActor::AgentInvocable
            ) | (
                OperationKind::Read,
                gofin_domain::ExecutionActor::AgentReadable
            )
        );
        if eligible {
            if !definition.semantics.is_object() {
                definition.semantics = json!({"adapter_semantics":definition.semantics.clone()});
            }
            if let Some(semantics) = definition.semantics.as_object_mut() {
                semantics.insert(
                    "plan_authorization".into(),
                    json!({
                        "mode":"exact_input",
                        "binds":["operation", "input_schema", "canonical_input", "protected_connection_identity"],
                        "financial_limits":"per_step_maximum_debits_and_costs",
                        "deferred_write_checks":["current_balances", "current_prices", "fee_quotes", "market_metadata", "permissions", "simulation", "verified_typed_effects"],
                        "approval_requires_future_balances":false
                    }),
                );
            }
        }
    }
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    definitions
}

fn snapshot_definition() -> Definition {
    Definition {
        name: "gofin.snapshot".into(),
        description: "Capture a scoped or full broker-combined financial snapshot and installation-wide unresolved journal identities.".into(),
        kind: OperationKind::Read,
        execution_actor: ExecutionActor::AgentReadable,
        input_schema: serde_json::to_value(schemars::schema_for!(SnapshotScope)).expect("snapshot scope schema"),
        output_schema: serde_json::to_value(schemars::schema_for!(StateSnapshot)).expect("snapshot output schema"),
        error_schema: json!({"$ref":"GoFinError"}),
        examples: vec![json!({"input":{}})],
        coverage: "Selected tracked resources and related exposure from reviewed adapters, plus installation-wide nonterminal financial operations.".into(),
        completion: "complete covers only the selected tracked scope, and is false for failed/timed-out observers, unsupported scope, missing coverage or unresolved financial journal records.".into(),
        semantics: json!({"clientSuppliedState":false,"scope":"optional-intersecting-resources-networks-assets","emptyScope":"all-configured-resources","relatedExposureIncluded":true,"pendingJournalIncluded":"installation-wide","observerTimeoutSeconds":10,"observerConcurrency":4}),
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

pub trait BrokerCredentialProvider: gofin_exchange::ccxt::CredentialProvider + Send + Sync {
    fn coinbase_auth(
        &self,
        _credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<gofin_exchange::coinbase::CoinbaseAuth> {
        Err(error(
            "CREDENTIAL_TYPE_MISMATCH",
            "credentials",
            "This credential provider cannot access a protected Coinbase credential.",
            "Enroll the applicable Coinbase product through protected connection enrollment.",
        ))
    }
    fn kraken_nonces(
        &self,
    ) -> gofin_domain::Result<Arc<dyn gofin_exchange::kraken::NonceAllocator>> {
        Err(GoFinError::validation(
            "This credential provider has no durable Kraken nonce allocator.",
        ))
    }
    fn usd_price_source(
        &self,
        _credential: &gofin_credentials::CredentialRef,
        _plan: &str,
    ) -> gofin_domain::Result<Arc<dyn gofin_pricing::UsdPriceSource>> {
        Err(error(
            "PRICING_CREDENTIAL_UNAVAILABLE",
            "configuration",
            "This credential provider has no protected pricing connection.",
            "Enroll the CoinGecko key in your own Terminal.",
        ))
    }
    fn envelope_signer(
        &self,
        credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)>;

    fn solana_signer(
        &self,
        _credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<Arc<gofin_solana::SolanaSigner>> {
        Err(error(
            "CREDENTIAL_TYPE_MISMATCH",
            "credentials",
            "This credential provider cannot access a protected Solana signer.",
            "Use the protected installed credential provider.",
        ))
    }

    fn jupiter_api_key(
        &self,
        _credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<zeroize::Zeroizing<String>> {
        Err(error(
            "CREDENTIAL_TYPE_MISMATCH",
            "credentials",
            "This credential provider cannot access a protected Jupiter service key.",
            "Enroll the key through protected connection enrollment.",
        ))
    }

    fn hyperliquid_signer(
        &self,
        _credential: &str,
        _owner: &str,
    ) -> gofin_domain::Result<gofin_exchange::ccxt::HyperliquidSigner> {
        Err(error(
            "CREDENTIAL_TYPE_MISMATCH",
            "credentials",
            "This credential provider cannot access a protected Hyperliquid signer.",
            "Use the protected installed credential provider.",
        ))
    }
}

/// Root-owned credentials are supplied only to protected signing components.
/// CCXT holds transient in-process copies; no private bytes are logged or
/// returned through the public protocol.
pub struct ProtectedCredentialProvider {
    store: Arc<gofin_credentials::TrustedCredentialStore>,
}

impl ProtectedCredentialProvider {
    pub fn open() -> gofin_domain::Result<Self> {
        Ok(Self {
            store: Arc::new(gofin_credentials::TrustedCredentialStore::open()?),
        })
    }

    pub fn from_store(store: Arc<gofin_credentials::TrustedCredentialStore>) -> Self {
        Self { store }
    }
}

impl gofin_exchange::ccxt::CredentialProvider for ProtectedCredentialProvider {
    fn credentials(
        &self,
        reference: &str,
    ) -> Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError> {
        use gofin_exchange::ccxt::{CcxtError, CcxtErrorKind, Credentials};
        let denied = || CcxtError::new(CcxtErrorKind::PermissionDenied);
        let reference = gofin_credentials::CredentialRef::new(reference).map_err(|_| denied())?;
        self.store
            .with_material(&reference, |material| {
                let (api_key, secret) = match material {
                    gofin_credentials::SecretMaterial::BinanceHmac { api_key, secret }
                    | gofin_credentials::SecretMaterial::KrakenApi { api_key, secret }
                    | gofin_credentials::SecretMaterial::KrakenFuturesApi { api_key, secret } => {
                        (api_key, secret)
                    }
                    gofin_credentials::SecretMaterial::BinanceRsa {
                        api_key,
                        private_key,
                    }
                    | gofin_credentials::SecretMaterial::BinanceEd25519 {
                        api_key,
                        private_key,
                    } => (api_key, private_key),
                    _ => {
                        return Err(GoFinError::validation(
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
    fn coinbase_auth(
        &self,
        credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<gofin_exchange::coinbase::CoinbaseAuth> {
        use gofin_credentials::SecretMaterial as M;
        use gofin_exchange::coinbase::CoinbaseAuth as A;
        self.store
            .with_material(credential, |material| match material {
                M::CoinbaseCdpEd25519 {
                    api_key,
                    private_key_base64,
                } => Ok(A::CdpEd25519 {
                    api_key: api_key.clone(),
                    private_key_base64: private_key_base64.clone(),
                }),
                M::CoinbaseCdpEcdsa {
                    api_key,
                    private_key_pem,
                } => Ok(A::CdpEcdsa {
                    api_key: api_key.clone(),
                    private_key_pem: private_key_pem.clone(),
                }),
                M::CoinbaseExchangeHmac {
                    api_key,
                    secret,
                    passphrase,
                } => Ok(A::ExchangeHmac {
                    api_key: api_key.clone(),
                    secret: secret.clone(),
                    passphrase: passphrase.clone(),
                }),
                M::CoinbaseInternationalHmac {
                    api_key,
                    secret,
                    passphrase,
                } => Ok(A::InternationalHmac {
                    api_key: api_key.clone(),
                    secret: secret.clone(),
                    passphrase: passphrase.clone(),
                }),
                _ => Err(GoFinError::validation(
                    "Credential is not the selected Coinbase product credential.",
                )),
            })
    }
    fn kraken_nonces(
        &self,
    ) -> gofin_domain::Result<Arc<dyn gofin_exchange::kraken::NonceAllocator>> {
        Ok(Arc::new(kraken::DurableKrakenNonces::open(
            std::path::Path::new("/Library/GoFin/private/kraken-nonces.sqlite"),
        )?))
    }
    fn usd_price_source(
        &self,
        credential: &gofin_credentials::CredentialRef,
        plan: &str,
    ) -> gofin_domain::Result<Arc<dyn gofin_pricing::UsdPriceSource>> {
        self.store
            .with_material(credential, |material| match material {
                gofin_credentials::SecretMaterial::CoinGeckoApiKey {
                    api_key,
                    plan: stored_plan,
                } if stored_plan == plan => {
                    let plan = match plan {
                        "demo" => gofin_pricing::ApiPlan::Demo,
                        "pro" => gofin_pricing::ApiPlan::Pro,
                        _ => return Err(GoFinError::validation("The CoinGecko plan is invalid.")),
                    };
                    Ok(Arc::new(gofin_pricing::ExchangeSpotPrices::new(
                        pricing_cache::attach(gofin_pricing::CoinGeckoPrices::with_api_key(
                            plan, api_key,
                        )?)?,
                    )?)
                        as Arc<dyn gofin_pricing::UsdPriceSource>)
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
        credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
        self.store
            .with_material(credential, |material| match material {
                gofin_credentials::SecretMaterial::EvmPrivateKey { private_key } => {
                    let signer = Arc::new(gofin_evm::LocalEnvelopeSigner::from_private_key(
                        private_key,
                    )?);
                    let address = format!("{:#x}", signer.address());
                    Ok((address, signer as Arc<dyn gofin_evm::EnvelopeSigner>))
                }
                gofin_credentials::SecretMaterial::BinanceHmac { .. }
                | gofin_credentials::SecretMaterial::BinanceRsa { .. }
                | gofin_credentials::SecretMaterial::BinanceEd25519 { .. }
                | gofin_credentials::SecretMaterial::CoinGeckoApiKey { .. }
                | gofin_credentials::SecretMaterial::KrakenApi { .. }
                | gofin_credentials::SecretMaterial::KrakenFuturesApi { .. }
                | gofin_credentials::SecretMaterial::CoinbaseCdpEd25519 { .. }
                | gofin_credentials::SecretMaterial::CoinbaseCdpEcdsa { .. }
                | gofin_credentials::SecretMaterial::CoinbaseExchangeHmac { .. }
                | gofin_credentials::SecretMaterial::CoinbaseInternationalHmac { .. } => {
                    Err(error(
                        "CREDENTIAL_TYPE_MISMATCH",
                        "credentials",
                        "Credential is not an Ethereum wallet key.",
                        "Select the enrolled Ethereum signer reference.",
                    ))
                }
                gofin_credentials::SecretMaterial::SolanaPrivateKey { .. }
                | gofin_credentials::SecretMaterial::JupiterApiKey { .. } => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "credentials",
                    "Credential is not an EVM wallet key.",
                    "Select the enrolled EVM signer reference.",
                )),
            })
    }

    fn solana_signer(
        &self,
        credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<Arc<gofin_solana::SolanaSigner>> {
        self.store
            .with_material(credential, |material| match material {
                gofin_credentials::SecretMaterial::SolanaPrivateKey { private_key } => {
                    Ok(Arc::new(gofin_solana::SolanaSigner::parse(private_key)?))
                }
                _ => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "credentials",
                    "Credential is not a Solana keypair.",
                    "Select the enrolled Solana signer reference.",
                )),
            })
    }

    fn jupiter_api_key(
        &self,
        credential: &gofin_credentials::CredentialRef,
    ) -> gofin_domain::Result<zeroize::Zeroizing<String>> {
        self.store
            .with_material(credential, |material| match material {
                gofin_credentials::SecretMaterial::JupiterApiKey { api_key } => {
                    if !gofin_jupiter::valid_api_key(api_key) {
                        return Err(error(
                            "CREDENTIAL_VALUE_INVALID",
                            "credentials",
                            "The protected Jupiter API key has an invalid format.",
                            "Repair the credential through protected human-terminal enrollment.",
                        ));
                    }
                    Ok(zeroize::Zeroizing::new(api_key.clone()))
                }
                _ => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "credentials",
                    "Credential is not a Jupiter service key.",
                    "Select the enrolled Jupiter credential reference.",
                )),
            })
    }

    fn hyperliquid_signer(
        &self,
        credential: &str,
        owner: &str,
    ) -> gofin_domain::Result<gofin_exchange::ccxt::HyperliquidSigner> {
        let reference = gofin_credentials::CredentialRef::new(credential)?;
        self.store
            .with_material(&reference, |material| match material {
                gofin_credentials::SecretMaterial::EvmPrivateKey { private_key } => {
                    hyperliquid_transport::signer_from_key(private_key, owner)
                }
                _ => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "credentials",
                    "Credential is not the enrolled Hyperliquid EVM owner key.",
                    "Select the protected key explicitly enrolled for this Hyperliquid owner.",
                )),
            })
    }
}

/// Ethereum runtime has one signer seam; this map routes only an already
/// frozen sender to its matching protected credential signer.
pub struct MappedEnvelopeSigner {
    signers: BTreeMap<String, Arc<dyn gofin_evm::EnvelopeSigner>>,
}

impl MappedEnvelopeSigner {
    pub fn new(
        signers: BTreeMap<String, Arc<dyn gofin_evm::EnvelopeSigner>>,
    ) -> gofin_domain::Result<Self> {
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
impl gofin_evm::EnvelopeSigner for MappedEnvelopeSigner {
    async fn sign_prepared(
        &self,
        transaction: &gofin_evm::FrozenTransaction,
    ) -> gofin_domain::Result<gofin_evm::alloy::primitives::Bytes> {
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
    async fn sign_authorized_permit(
        &self,
        permit: &gofin_evm::AuthorizedPermit,
    ) -> gofin_domain::Result<gofin_evm::alloy::primitives::Bytes> {
        let owner = gofin_evm::lower_address(permit.owner()?);
        let signer = self.signers.get(&owner).ok_or_else(|| {
            error(
                "SIGNER_MISMATCH",
                "signing",
                "No protected signer is mapped to the permit owner.",
                "Use the enrolled wallet that owns this exact permit scope.",
            )
        })?;
        signer.sign_authorized_permit(permit).await
    }
}

#[async_trait]
pub trait AssetValuator: Send + Sync {
    async fn usd_micros(&self, amount: &Amount) -> gofin_domain::Result<(String, String, String)>;

    async fn asset_price(&self, asset: &AssetId) -> gofin_domain::Result<AssetPrice> {
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
            atoms: gofin_domain::Quantity::new(unit_atoms.to_string())?,
        };
        let (usd_micros_per_unit, observed_at, source) = self.usd_micros(&amount).await?;
        Ok(AssetPrice {
            asset: asset.clone(),
            usd_micros_per_unit: gofin_domain::Quantity::new(usd_micros_per_unit)?,
            unit_atoms: gofin_domain::Quantity::new(unit_atoms.to_string())?,
            observed_at,
            source,
        })
    }
}

pub struct EthereumPayoutVerifier {
    adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
}

pub struct ProtectedApplications {
    pub adapters: Vec<Arc<dyn Adapter>>,
    pub valuator: Arc<dyn AssetValuator>,
    pub bridge: Option<Arc<BridgeAdapter>>,
}

pub fn build_protected_applications(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
) -> gofin_domain::Result<ProtectedApplications> {
    config.validate()?;
    let source: Arc<dyn gofin_pricing::UsdPriceSource> =
        if let Some(ConnectionConfig::CoinGecko {
            credential_ref,
            api_plan,
            ..
        }) = config
            .connections
            .iter()
            .find(|connection| matches!(connection, ConnectionConfig::CoinGecko { .. }))
        {
            credentials.usd_price_source(
                &gofin_credentials::CredentialRef::new(credential_ref)?,
                api_plan,
            )?
        } else {
            Arc::new(gofin_pricing::ExchangeSpotPrices::new(
                pricing_cache::attach(gofin_pricing::CoinGeckoPrices::new()?)?,
            )?)
        };
    build_applications_with_price_source(config, credentials, source)
}

/// Compose the same real adapters with explicitly supplied credentials and pricing.
/// Callers own those dependencies; this constructor never selects installed state.
pub fn build_applications_with_price_source(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
    source: Arc<dyn gofin_pricing::UsdPriceSource>,
) -> gofin_domain::Result<ProtectedApplications> {
    config.validate()?;
    let mut eth_rpcs = BTreeMap::<u64, Arc<dyn gofin_evm::EvmRpc>>::new();
    for network in &config.networks {
        let rpc = Arc::new(gofin_evm::HttpRpc::new(
            network.rpc_urls.clone(),
            network.chain_id,
        )?);
        eth_rpcs.insert(network.chain_id, rpc.clone());
    }
    build_applications_with_transports(config, credentials, source, eth_rpcs)
}

/// Compose real adapters with caller-owned typed EVM transports. The configuration
/// remains subject to production validation; this is an internal dependency seam,
/// not a serialized configuration or broker-protocol transport override.
pub fn build_applications_with_transports(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
    source: Arc<dyn gofin_pricing::UsdPriceSource>,
    eth_rpcs: BTreeMap<u64, Arc<dyn gofin_evm::EvmRpc>>,
) -> gofin_domain::Result<ProtectedApplications> {
    build_applications_with_network_transports(config, credentials, source, eth_rpcs, None, None)
}

/// Internal composition seam for synthetic adapter rehearsals. Production
/// configuration cannot select or serialize these transports.
pub fn build_applications_with_network_transports(
    config: &BrokerConfigV2,
    credentials: Arc<dyn BrokerCredentialProvider>,
    source: Arc<dyn gofin_pricing::UsdPriceSource>,
    eth_rpcs: BTreeMap<u64, Arc<dyn gofin_evm::EvmRpc>>,
    supplied_solana_rpc: Option<Arc<dyn gofin_solana::SolanaRpc>>,
    supplied_jupiter_api: Option<Arc<dyn gofin_jupiter::JupiterApi>>,
) -> gofin_domain::Result<ProtectedApplications> {
    config.validate()?;
    let expected: std::collections::BTreeSet<_> = config
        .networks
        .iter()
        .map(|network| network.chain_id)
        .collect();
    if eth_rpcs
        .keys()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        != expected
    {
        return Err(GoFinError::validation(
            "Supplied EVM transports must exactly match the configured network scope.",
        ));
    }
    let has_solana_wallets = config
        .connections
        .iter()
        .any(|connection| matches!(connection, ConnectionConfig::Solana { .. }));
    let solana_rpc: Option<Arc<dyn gofin_solana::SolanaRpc>> = if has_solana_wallets {
        Some(match supplied_solana_rpc {
            Some(rpc) => rpc,
            None => gofin_solana::HttpRpc::mainnet_beta()?,
        })
    } else {
        None
    };
    let jupiter_api_key = config
        .connections
        .iter()
        .find_map(|connection| match connection {
            ConnectionConfig::Jupiter { credential_ref, .. } => Some(credential_ref),
            _ => None,
        })
        .map(|reference| {
            gofin_credentials::CredentialRef::new(reference)
                .and_then(|reference| credentials.jupiter_api_key(&reference))
        })
        .transpose()?;
    let jupiter_api: Option<Arc<dyn gofin_jupiter::JupiterApi>> = if has_solana_wallets {
        Some(match supplied_jupiter_api {
            Some(api) => api,
            None => gofin_jupiter::HttpJupiterApi::new(jupiter_api_key.clone())?,
        })
    } else {
        None
    };
    let solana_prices: Option<Arc<dyn gofin_pricing::UsdPriceSource>> =
        match (&solana_rpc, &jupiter_api) {
            (Some(rpc), Some(api)) => Some(Arc::new(gofin_jupiter::JupiterUsdPrices::new(
                api.clone(),
                rpc.clone(),
            ))),
            _ => None,
        };
    let pricing_source: Arc<dyn gofin_pricing::UsdPriceSource> = match &solana_prices {
        Some(solana_prices) => Arc::new(pricing::NetworkUsdPriceSource::new(
            source.clone(),
            solana_prices.clone(),
        )),
        None => source.clone(),
    };
    let binance_prices = Arc::new(SharedUsdValuator::with_source(pricing_source.clone()));
    let wallet_prices = binance_prices.clone();
    let token_discovery = Arc::new(evm_public_data::PublicTokenDiscovery::new()?);
    let mut ethereum_adapters = BTreeMap::<String, Arc<gofin_ethereum::EthereumAdapter>>::new();
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
            wallets.push(gofin_ethereum::WalletConnection {
                name: name.clone(),
                address: address.clone(),
                signing_enabled: *signing_enabled,
            });
            if let Some(reference) = credential_ref {
                let credential = gofin_credentials::CredentialRef::new(reference)?;
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
        let signer: Option<Arc<dyn gofin_evm::EnvelopeSigner>> = if signer_map.is_empty() {
            None
        } else {
            Some(Arc::new(MappedEnvelopeSigner::new(signer_map)?))
        };
        let runtime = Arc::new(gofin_evm::EvmRuntime::new(rpc, network.chain_id, signer));
        let ethereum_config = if let Some(deployment) = config
            .deployments
            .iter()
            .find(|deployment| deployment.network == network.id)
        {
            gofin_ethereum::EthereumConfig {
                aave_markets: deployment.aave_markets.clone(),
                chain_id: network.chain_id,
                confirmations: network.confirmations,
                require_finalized: network.chain_id != 1
                    && !gofin_ethereum::EthereumConfig::is_reviewed_sequencer_rollup(
                        network.chain_id,
                    ),
                weth: deployment.weth.clone(),
                snapshot_tokens: deployment.snapshot_tokens.clone(),
                uniswap: deployment.uniswap.clone(),
                ramses: deployment.ramses.clone(),
                v3_deployments: deployment.v3_deployments.clone(),
                aave: deployment.aave.clone(),
            }
        } else if network.chain_id == 8453 {
            // Existing wallet profiles adopt reviewed compiled deployments
            // without changing signer or legacy chain identity. Every write
            // still performs fresh protocol/fee/route checks.
            let mut base =
                gofin_ethereum::EthereumConfig::base_aave(network.snapshot_tokens.clone());
            base.uniswap = amm_deployments::reviewed_uniswap(8453);
            base
        } else {
            gofin_ethereum::EthereumConfig::wallet_only(
                network.chain_id,
                network.confirmations,
                network.snapshot_tokens.clone(),
            )
        };
        let adapter = Arc::new(
            gofin_ethereum::EthereumAdapter::new(ethereum_config, wallets, runtime)?
                .with_wallet_valuation(wallet_prices.clone())
                .with_token_discovery(token_discovery.clone()),
        );
        ethereum_adapters.insert(network.id.clone(), adapter);
    }
    let mut adapters: Vec<Arc<dyn Adapter>> = Vec::new();
    let evm_wallet_router: Option<Arc<dyn Adapter>> = if !ethereum_adapters.is_empty() {
        Some(Arc::new(evm_router::EvmNetworkRouter::new(
            ethereum_adapters
                .values()
                .map(|adapter| (adapter.config.chain_id, adapter.clone()))
                .collect(),
        )))
    } else {
        None
    };
    let mut solana_wallets = BTreeMap::<String, Arc<gofin_solana::SolanaWalletAdapter>>::new();
    for connection in &config.connections {
        let ConnectionConfig::Solana {
            name,
            address,
            credential_ref,
            ..
        } = connection
        else {
            continue;
        };
        let signer = credential_ref
            .as_deref()
            .map(gofin_credentials::CredentialRef::new)
            .transpose()?
            .map(|reference| credentials.solana_signer(&reference))
            .transpose()?;
        if let Some(signer) = &signer
            && signer.address() != address
        {
            return Err(error(
                "SIGNER_ADDRESS_MISMATCH",
                "configuration",
                "Protected Solana key does not derive the configured physical address.",
                "Correct the wallet enrollment mapping through protected human-terminal setup.",
            ));
        }
        let adapter = Arc::new(gofin_solana::SolanaWalletAdapter::new(
            name.clone(),
            address,
            signer,
            solana_rpc.as_ref().expect("RPC initialized").clone(),
            solana_prices
                .as_ref()
                .expect("price source initialized with Solana wallet")
                .clone(),
        )?);
        solana_wallets.insert(name.clone(), adapter);
    }
    if !solana_wallets.is_empty() {
        adapters.push(Arc::new(wallet_router::WalletFamilyRouter::new(
            evm_wallet_router,
            solana_wallets.clone(),
        )));
    } else if let Some(evm) = evm_wallet_router {
        adapters.push(evm);
    }
    if !solana_wallets.is_empty() {
        adapters.push(Arc::new(gofin_jupiter::JupiterAdapter::with_price_source(
            solana_wallets.clone(),
            jupiter_api
                .as_ref()
                .expect("API initialized with Solana wallet")
                .clone(),
            solana_prices
                .as_ref()
                .expect("price source initialized with Solana wallet")
                .clone(),
        )?));
        adapters.push(Arc::new(gofin_jupiter::JlpAdapter::with_price_source(
            solana_wallets.clone(),
            jupiter_api
                .as_ref()
                .expect("API initialized with Solana wallet")
                .clone(),
            solana_prices
                .as_ref()
                .expect("price source initialized with Solana wallet")
                .clone(),
        )?));
        adapters.push(Arc::new(gofin_jupiter::PerpsAdapter::with_price_source(
            solana_wallets.clone(),
            jupiter_api
                .as_ref()
                .expect("API initialized with Solana wallet")
                .clone(),
            solana_prices
                .as_ref()
                .expect("price source initialized with Solana wallet")
                .clone(),
        )?));
        adapters.push(Arc::new(gofin_jupiter::TriggerAdapter::with_price_source(
            solana_wallets.clone(),
            jupiter_api
                .as_ref()
                .expect("API initialized with Solana wallet")
                .clone(),
            solana_prices
                .as_ref()
                .expect("price source initialized with Solana wallet")
                .clone(),
        )?));
    }
    let bridge = if ethereum_adapters.is_empty() {
        None
    } else {
        let bridge_observers = eth_rpcs
            .iter()
            .map(|(chain_id, rpc)| {
                (
                    *chain_id,
                    Arc::new(gofin_evm::EvmRuntime::new(rpc.clone(), *chain_id, None)),
                )
            })
            .collect();
        let bridge = Arc::new(
            BridgeAdapter::with_observers(
                ethereum_adapters
                    .values()
                    .map(|adapter| (adapter.config.chain_id, adapter.clone()))
                    .collect(),
                bridge_observers,
                binance_prices.clone(),
            )?
            .with_solana_wallets(solana_wallets.clone()),
        );
        adapters.push(bridge.clone());
        Some(bridge)
    };
    let exchange_client = Arc::new(gofin_exchange::ccxt::CcxtClient::new().map_err(|_| {
        error(
            "EXCHANGE_TRANSPORT_UNAVAILABLE",
            "startup",
            "Exchange transport could not be initialized.",
            "Check the protected broker installation.",
        )
    })?);
    adapters.push(Arc::new(gofin_exchange::ExchangeAdapter::new(
        gofin_exchange::DefaultExchangeProfile::new(exchange_client.clone()),
    )));
    let hyperliquid_accounts = config
        .connections
        .iter()
        .filter_map(|connection| match connection {
            ConnectionConfig::Hyperliquid {
                name,
                owner,
                credential_ref,
                signing_enabled,
                execution_scope,
                auto_derived,
                ..
            } => {
                let account_name = if *auto_derived {
                    config
                        .connections
                        .iter()
                        .find_map(|candidate| match candidate {
                            ConnectionConfig::Evm { name, address, .. }
                                if address.eq_ignore_ascii_case(owner) =>
                            {
                                Some(name.clone())
                            }
                            _ => None,
                        })?
                } else {
                    name.clone()
                };
                Some(gofin_hyperliquid::HyperliquidAccount {
                    name: account_name,
                    owner: owner.clone(),
                    credential_ref: credential_ref.clone(),
                    signing_enabled: *signing_enabled,
                    execution_scope: execution_scope.clone(),
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !hyperliquid_accounts.is_empty() {
        let exchange_credentials: Arc<dyn BrokerCredentialProvider> = credentials.clone();
        let transport = Arc::new(hyperliquid_transport::HyperliquidCcxtTransport::new(
            exchange_client.clone(),
            exchange_credentials,
        ));
        let mut profile = gofin_hyperliquid::HyperliquidProfile::new(
            hyperliquid_accounts,
            transport.clone(),
            Arc::new(hyperliquid_transport::HyperliquidUsdPrices::new(
                binance_prices.clone(),
            )),
            Arc::new(gofin_hyperliquid::DeferredExecutionState::default()),
        )?;
        if let Some(arbitrum) = ethereum_adapters
            .values()
            .find(|adapter| adapter.config.chain_id == 42161)
        {
            profile = profile.with_withdrawal_proof(Arc::new(
                HyperliquidArbitrumWithdrawalProof::new(arbitrum.clone()),
            ));
        }
        let profile = Arc::new(profile);
        adapters.push(Arc::new(gofin_exchange::ExchangeAdapter::new(
            profile.without_operation("hyperliquid.account.deposit"),
        )));
        if let Some(arbitrum) = ethereum_adapters
            .values()
            .find(|adapter| adapter.config.chain_id == 42161)
        {
            adapters.push(Arc::new(HyperliquidEthereumDepositAdapter::new(
                profile,
                arbitrum.clone(),
                transport,
            )));
        }
    }
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
                    Some(gofin_binance::BinanceAccount {
                        name: name.clone(),
                        resource: resource.clone(),
                        credential: gofin_binance::CredentialHandle::new(credential_ref).ok()?,
                        role: if role == "main" {
                            gofin_binance::AccountRole::Main
                        } else {
                            gofin_binance::AccountRole::Sub
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
        let exchange_credentials: Arc<dyn gofin_exchange::ccxt::CredentialProvider> =
            credentials.clone();
        let transport = Arc::new(gofin_binance::BinanceCcxtTransport::new(
            exchange_client.clone(),
            exchange_credentials,
        ));
        // Binance reads and enrollment do not require a wallet observer.
        // Payout preparation checks the selected configured chain separately.
        let evm_payout_verifier: Option<Arc<dyn gofin_binance::BinanceRecipientVerifier>> =
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
        let payout_verifier: Option<Arc<dyn gofin_binance::BinanceRecipientVerifier>> =
            if solana_wallets.is_empty() {
                evm_payout_verifier
            } else {
                Some(Arc::new(solana_cex::SolanaCexPayoutVerifier::new(
                    evm_payout_verifier,
                    solana_wallets.clone(),
                )?))
            };
        let adapter = Arc::new(gofin_binance::BinanceAdapter::new(
            gofin_binance::BinanceConfig {
                accounts,
                destinations,
            },
            transport,
            Some(binance_prices.clone()),
            payout_verifier,
        )?);
        adapters.push(adapter.clone());
        if !ethereum_adapters.is_empty() || !solana_wallets.is_empty() {
            adapters.push(Arc::new(
                BinanceEthereumDepositAdapter::with_networks(
                    adapter,
                    ethereum_adapters.values().cloned(),
                )
                .with_solana_wallets(solana_wallets.clone()),
            ));
        }
    }
    let mut coinbase_profile =
        coinbase::profile(config, credentials.clone(), pricing_source.clone())?;
    if !ethereum_adapters.is_empty() {
        coinbase_profile = coinbase_profile.with_recipient_verifier(Arc::new(
            coinbase_funding::CoinbaseEvmRecipientVerifier::new(
                config,
                ethereum_adapters
                    .values()
                    .map(|adapter| (adapter.config.chain_id, adapter.clone()))
                    .collect(),
            )?,
        ));
    }
    let coinbase_profile = Arc::new(coinbase_profile);
    if ethereum_adapters.values().any(|adapter| {
        matches!(
            adapter.config.chain_id,
            1 | 10 | 137 | 999 | 5042 | 8453 | 42161 | 43114
        )
    }) && coinbase_profile
        .accounts()
        .iter()
        .any(|account| account.product == gofin_coinbase::CoinbaseProduct::Advanced)
    {
        adapters.push(Arc::new(
            coinbase_deposit::CoinbaseBaseDepositAdapter::with_networks(
                coinbase_profile.clone(),
                ethereum_adapters
                    .values()
                    .filter(|adapter| {
                        matches!(
                            adapter.config.chain_id,
                            1 | 10 | 137 | 999 | 5042 | 8453 | 42161 | 43114
                        )
                    })
                    .cloned()
                    .collect(),
            )?,
        ));
    }
    if !coinbase_profile.accounts().is_empty() {
        adapters.push(Arc::new(gofin_exchange::ExchangeAdapter::new(
            (*coinbase_profile).clone(),
        )));
    }
    adapters.extend(kraken::adapters(
        config,
        credentials.clone(),
        Arc::new(kraken_pricing::KrakenCollateralPrices(pricing_source)),
        ethereum_adapters
            .values()
            .map(|adapter| (adapter.config.chain_id, adapter.clone()))
            .collect(),
        solana_wallets,
    )?);
    let valuator: Arc<dyn AssetValuator> = binance_prices;
    Ok(ProtectedApplications {
        adapters,
        valuator,
        bridge,
    })
}

/// A reusable wallet has one payout destination per configured chain. Legacy
/// connections retain their original custody and configured asset identities.
fn configured_binance_destinations(
    config: &BrokerConfigV2,
) -> gofin_domain::Result<Vec<gofin_binance::EthereumDestination>> {
    let mut destinations = Vec::new();
    for connection in &config.connections {
        if let ConnectionConfig::Solana {
            name,
            resource,
            address,
            ..
        } = connection
        {
            // Retain the legacy destination record shape. The exact Solana
            // resource plus venue network selects its observer; chain_id=0
            // alone never identifies a network or grants receipt coverage.
            destinations.push(gofin_binance::EthereumDestination {
                name: name.clone(),
                resource: resource.clone(),
                address: address.clone(),
                chain_id: 0,
                assets: BTreeMap::new(),
                confirmations: 1,
            });
            continue;
        }
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
            let payout_confirmations =
                if gofin_ethereum::EthereumConfig::external_credit_uses_sequencer(network.chain_id)
                {
                    1
                } else {
                    network.confirmations
                };
            let confirmations = u32::try_from(payout_confirmations).map_err(|_| {
                error(
                    "CONFIG_CONFIRMATIONS_INVALID",
                    "configuration",
                    "Payout confirmations exceed the supported count.",
                    "Configure a confirmation count representable by the payout verifier.",
                )
            })?;
            destinations.push(gofin_binance::EthereumDestination {
                name: name.clone(),
                resource,
                address: address.clone(),
                chain_id: network.chain_id,
                assets: assets
                    .into_iter()
                    .map(|asset| {
                        let key = if asset.reference == "native"
                            && gofin_networks::supports_chain(network.chain_id)
                        {
                            gofin_networks::by_chain_id(network.chain_id)
                                .expect("reviewed chain checked above")
                                .native_symbol()
                                .to_owned()
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

gofin_evm::alloy::sol! {
    interface PayoutErc20 {
        function decimals() external view returns (uint8 value);
    }
}

fn pow10(power: u32) -> num_bigint::BigUint {
    num_bigint::BigUint::from(10u8).pow(power)
}

impl EthereumPayoutVerifier {
    pub fn new(
        adapters: BTreeMap<u64, Arc<gofin_ethereum::EthereumAdapter>>,
    ) -> gofin_domain::Result<Self> {
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
impl gofin_binance::BinanceRecipientVerifier for EthereumPayoutVerifier {
    fn supports_chain(&self, chain_id: u64) -> bool {
        self.adapters.contains_key(&chain_id)
    }

    async fn resolve_asset(
        &self,
        chain_id: u64,
        coin: &str,
        contract: Option<&str>,
    ) -> gofin_domain::Result<Option<AssetId>> {
        let Some(adapter) = self.adapters.get(&chain_id) else {
            return Ok(None);
        };
        if !gofin_networks::supports_chain(chain_id)
            || adapter.config.chain_id != chain_id
            || adapter.runtime.chain_id != chain_id
        {
            return Ok(None);
        }
        let contract = contract.unwrap_or("");
        let (reference, decimals) = if contract.is_empty() {
            if coin
                != gofin_networks::by_chain_id(chain_id)
                    .expect("reviewed chain checked above")
                    .native_symbol()
            {
                return Ok(None);
            }
            adapter.runtime.assert_chain().await?;
            ("native".to_owned(), 18)
        } else {
            if !contract.starts_with("0x") || !valid_eth_address(contract) {
                return Ok(None);
            }
            let token = gofin_evm::address(contract)?;
            if token == gofin_evm::alloy::primitives::Address::ZERO {
                return Ok(None);
            }
            adapter.runtime.assert_chain().await?;
            // Deployment and decimals are observed at one pinned block, rather
            // than accepting exchange symbols as an onchain asset identity.
            let block = gofin_evm::rpc_u64(
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
            if u32::from(decimals) > gofin_binance::MAX_DECIMAL_SCALE {
                return Ok(None);
            }
            (gofin_evm::lower_address(token), decimals)
        };
        Ok(Some(AssetId {
            network: format!("ethereum:{chain_id}"),
            reference,
            decimals,
        }))
    }

    async fn verify(&self, request: gofin_binance::PayoutRequest) -> gofin_binance::PayoutResult {
        let Some(adapter) = self.adapters.get(&request.chain_id) else {
            return gofin_binance::PayoutResult {
                status: gofin_domain::Status::Unknown,
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
                if gofin_ethereum::EthereumConfig::external_credit_uses_sequencer(request.chain_id)
                {
                    // Reconcile historical payouts by their original identity under
                    // the reviewed incoming-credit policy, without resubmission.
                    1
                } else {
                    request
                        .confirmations
                        .max(adapter.config.confirmations as u32)
                },
            )
            .await
        {
            Ok(proof) => gofin_binance::PayoutResult {
                status: proof.status,
                amount_atoms: proof.amount_atoms,
                evidence: proof.evidence,
            },
            Err(error) => {
                let mut evidence = json!({
                    "verification":"failed-closed",
                    "code":error.code,
                    "stage":error.stage,
                });
                if let Some(details) = error.details.as_deref().and_then(Value::as_object) {
                    if let Some(code) = details.get("json_rpc_code").and_then(Value::as_i64) {
                        evidence["jsonRpcCode"] = json!(code);
                    }
                    if let Some(status) = details.get("http_status").and_then(Value::as_u64) {
                        evidence["httpStatus"] = json!(status);
                    }
                }
                gofin_binance::PayoutResult {
                    status: gofin_domain::Status::Unknown,
                    amount_atoms: None,
                    evidence,
                }
            }
        }
    }
}

pub struct RuntimeDependencies {
    pub store: Store,
    pub adapters: Vec<Arc<dyn Adapter>>,
    pub authorizer: Arc<dyn Authorizer>,
    pub valuator: Arc<dyn AssetValuator>,
    pub bridge: Option<Arc<BridgeAdapter>>,
}

fn concurrent_read_request(request: &BrokerRequest) -> bool {
    matches!(
        request,
        BrokerRequest::Capabilities
            | BrokerRequest::Connections
            | BrokerRequest::PolicyShow
            | BrokerRequest::Doctor
            | BrokerRequest::Operations
            | BrokerRequest::Snapshot
    ) || matches!(request, BrokerRequest::Read { operation, .. } if !matches!(operation.as_str(), "bridge.quotes" | "bridge.status" | "bridge.recovery"))
}

// These paths either collect read-only evidence or atomically update authority.
// Financial writes still serialize in the engine; revocation races are ordered
// against durable submission by the store's authority/CAS checks.
fn independent_request(request: &BrokerRequest) -> bool {
    matches!(
        request,
        BrokerRequest::Preview { .. }
            | BrokerRequest::PreviewPlan { .. }
            | BrokerRequest::PreviewPlanStep { .. }
            | BrokerRequest::PreviewBoundary { .. }
            | BrokerRequest::PreviewBoundaryAuthorization { .. }
            | BrokerRequest::PlanStatus { .. }
            | BrokerRequest::Status { .. }
            | BrokerRequest::RevokePlan { .. }
            | BrokerRequest::RevokeBoundary { .. }
    )
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
    mutation_gate: AsyncMutex<()>,
    verified_account_names: Mutex<BTreeSet<String>>,
    account_identity_gates: BTreeMap<String, AsyncMutex<()>>,
    artifacts: Arc<Mutex<artifacts::ArtifactCache>>,
    artifact_profiles: Mutex<Vec<Arc<gofin_kraken::KrakenProfile>>>,
}

struct ValuatorPriceProvider {
    valuator: Arc<dyn AssetValuator>,
}

#[async_trait]
impl gofin_engine::TrustedAssetPriceProvider for ValuatorPriceProvider {
    async fn asset_price(&self, asset: &AssetId) -> gofin_domain::Result<AssetPrice> {
        self.valuator.asset_price(asset).await
    }
}

pub fn compose_runtime(
    config: BrokerConfigV2,
    dependencies: RuntimeDependencies,
) -> gofin_domain::Result<Arc<BrokerRuntime>> {
    compose_runtime_with_clock(
        config,
        dependencies,
        Arc::new(gofin_domain::clock::SystemClock),
    )
}

/// Internal deterministic composition. The protected entry point always uses system UTC.
#[doc(hidden)]
pub fn compose_runtime_with_clock(
    config: BrokerConfigV2,
    dependencies: RuntimeDependencies,
    clock: Arc<dyn gofin_domain::clock::Clock>,
) -> gofin_domain::Result<Arc<BrokerRuntime>> {
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
            .any(|definition| definition.name == "gofin.snapshot")
    }) {
        return Err(error(
            "DUPLICATE_SNAPSHOT_OBSERVER",
            "startup",
            "The reserved composite snapshot operation is already registered.",
            "Remove the conflicting operation from the injected adapter set.",
        ));
    }
    let shared_store = dependencies.store.share();
    let execution_state = Arc::new(gofin_store::execution_state::ExecutionStateRepository(
        shared_store.clone(),
    ));
    for adapter in &dependencies.adapters {
        adapter.attach_execution_state(execution_state.clone())?;
    }
    if let Some(bridge) = &dependencies.bridge {
        bridge.attach_state(gofin_store::BridgeStateRepository::new(
            shared_store.clone(),
        ))?;
    }
    let observer = Arc::new(CompositeObserver::new(dependencies.adapters.clone()));
    let mut engine_adapters = dependencies.adapters.clone();
    engine_adapters.push(observer.clone());
    let price_provider = Arc::new(ValuatorPriceProvider {
        valuator: dependencies.valuator.clone(),
    });
    let engine = Arc::new(
        Engine::new_with_shared_store(
            shared_store,
            engine_adapters,
            dependencies.authorizer,
            options,
        )?
        .with_cost_origin_price_provider(price_provider)
        .with_clock(clock),
    );
    if let Some(bridge) = &dependencies.bridge {
        bridge.attach_engine(Arc::downgrade(&engine))?;
    }
    observer.attach(Arc::downgrade(&engine));
    let account_identity_gates = config
        .connections
        .iter()
        .filter_map(|connection| {
            if let ConnectionConfig::Binance { name, .. } = connection {
                Some((name.clone(), AsyncMutex::new(())))
            } else {
                None
            }
        })
        .collect();
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
        daemon_marker_path: Mutex::new(PathBuf::from(gofin_protocol::daemon::STOP_MARKER_PATH)),
        daemon_marker_owner_uid: unsafe { libc::geteuid() },
        mutation_gate: AsyncMutex::new(()),
        verified_account_names: Mutex::new(BTreeSet::new()),
        account_identity_gates,
        artifacts: Arc::new(Mutex::new(artifacts::ArtifactCache::default())),
        artifact_profiles: Mutex::new(vec![]),
    }))
}

/// Compose the installed runtime only after fixed-path root installation,
/// protected store/config validation, and trusted credential setup succeed.
pub async fn protected_runtime() -> gofin_domain::Result<Arc<BrokerRuntime>> {
    validate_installation()?;
    let config = load_protected_config()?;
    protected_runtime_from_config(config).await
}

async fn protected_runtime_from_config(
    config: BrokerConfigV2,
) -> gofin_domain::Result<Arc<BrokerRuntime>> {
    validate_installation()?;
    let store = open_protected_store()?;
    let credentials = Arc::new(ProtectedCredentialProvider::open()?);
    let artifact_profiles = kraken::profiles(&config, credentials.clone(), None, None)?;
    let applications = build_protected_applications(&config, credentials)?;
    let authorizer = Arc::new(MacOsAuthorizer::protected(config.approver_uid)?);
    let runtime = compose_runtime(
        config,
        RuntimeDependencies {
            store,
            adapters: applications.adapters,
            authorizer,
            valuator: applications.valuator,
            bridge: applications.bridge,
        },
    )?;
    runtime.attach_kraken_artifacts(artifact_profiles)?;
    runtime.protected_config_check.store(true, Ordering::SeqCst);
    runtime.require_account_identity_verification();
    Ok(runtime)
}

impl BrokerRuntime {
    /// Synthetic and protected composition attach only typed product profiles.
    pub fn attach_kraken_artifacts(
        &self,
        profiles: Vec<gofin_kraken::KrakenProfile>,
    ) -> gofin_domain::Result<()> {
        let mut attached = self
            .artifact_profiles
            .lock()
            .map_err(|_| internal_error())?;
        if !attached.is_empty() || self.protected_config_check.load(Ordering::SeqCst) {
            return Err(GoFinError::validation(
                "Artifact providers cannot be replaced after protected startup.",
            ));
        }
        *attached = profiles.into_iter().map(Arc::new).collect();
        Ok(())
    }

    /// Gate account operations on verified venue identity. Local discovery and
    /// recovery inventory remain available while a venue is unavailable.
    pub fn require_account_identity_verification(&self) {
        if let Ok(mut verified) = self.verified_account_names.lock() {
            verified.clear();
        }
        self.account_identity_check_pending
            .store(true, Ordering::SeqCst);
    }

    async fn verify_account_identities(&self, request: &BrokerRequest) -> gofin_domain::Result<()> {
        if !self.account_identity_check_pending.load(Ordering::SeqCst) {
            return Ok(());
        }
        use BrokerRequest as R;
        let (operation, input) = match request {
            R::Read { operation, input }
            | R::Preview { operation, input }
            | R::PreviewBoundary {
                operation, input, ..
            }
            | R::PreviewPlanStep {
                operation, input, ..
            } => (operation, input),
            R::Execute { request }
            | R::ExecuteBoundary { request, .. }
            | R::ExecutePlanStep { request, .. }
            | R::ReplacePlanCandidate { request, .. }
            | R::ReplaceBoundaryCandidate { request, .. } => (&request.operation, &request.input),
            R::AuthorizeBoundary { request } | R::PreviewBoundaryAuthorization { request } => {
                (&request.operation, &request.input)
            }
            // Snapshot observers verify their own identities and report partial
            // evidence. Static plan review binds protected identities without IO.
            _ => return Ok(()),
        };
        if !operation.starts_with("binance.") {
            return Ok(());
        }
        fn contains_identity(value: &Value, name: &str, resource: &str) -> bool {
            match value {
                Value::String(value) => value == name || value == resource,
                Value::Array(values) => values.iter().any(|v| contains_identity(v, name, resource)),
                Value::Object(values) => values
                    .values()
                    .any(|v| contains_identity(v, name, resource)),
                _ => false,
            }
        }
        let accounts = self
            .config
            .connections
            .iter()
            .filter_map(|connection| {
                if let ConnectionConfig::Binance { name, resource, .. } = connection {
                    Some((name, resource))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let selected = accounts
            .iter()
            .filter(|(name, resource)| contains_identity(input, name, &resource.0))
            .copied()
            .collect::<Vec<_>>();
        // Malformed/unknown Binance input remains fail-closed until normal
        // schema/account resolution rejects it; never infer another account.
        let selected = if selected.is_empty() {
            &accounts
        } else {
            &selected
        };
        for (name, _) in selected {
            let _identity_guard = self
                .account_identity_gates
                .get(*name)
                .ok_or_else(internal_error)?
                .lock()
                .await;
            if self
                .verified_account_names
                .lock()
                .map_err(|_| internal_error())?
                .contains(*name)
            {
                continue;
            }
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
            tokio::time::timeout(
                BINANCE_ACCOUNT_IDENTITY_TIMEOUT,
                adapter.read("binance.account-identity", &json!({"account":name})),
            )
            .await
            .map_err(|_| {
                error(
                    "ACCOUNT_IDENTITY_TIMEOUT",
                    "read",
                    "The selected Binance account identity check exceeded its deadline.",
                    "Retry after account connectivity recovers; no financial action was submitted.",
                )
            })??;
            self.verified_account_names
                .lock()
                .map_err(|_| internal_error())?
                .insert((*name).clone());
        }
        // Cache only this immutable runtime's identities. Reload creates a new
        // cache, and every requested account is checked before its first use.
        let verified = self
            .verified_account_names
            .lock()
            .map_err(|_| internal_error())?;
        if accounts.iter().all(|(name, _)| verified.contains(*name)) {
            self.account_identity_check_pending
                .store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    pub fn configuration(&self) -> &BrokerConfigV2 {
        &self.config
    }

    /// Resolve RPC observation scope from registered adapters and the journal.
    /// Wire opt-ins alone never change execution authority or response budgets.
    pub fn request_uses_rpc_reads(&self, request: &BrokerRequest) -> bool {
        use BrokerRequest as R;
        let scope = match request {
            R::Read { operation, input } | R::ReadArtifact { operation, input } => {
                self.engine.rpc_read_scope(operation, input)
            }
            R::Snapshot => self.engine.rpc_read_scope("gofin.snapshot", &json!({})),
            R::Status { id } => self.engine.status_rpc_read_scope(id),
            R::PlanStatus { plan_id } => self.engine.financial_plan_rpc_read_scope(plan_id),
            R::BoundaryStatus { id } => self.engine.boundary_rpc_read_scope(id),
            _ => return false,
        };
        // Invalid identifiers/inputs still reach the usual request validation.
        scope.unwrap_or(false)
    }

    /// Legacy clients may half-close normally and cannot signal cancellation.
    /// Preserve their bounded observation path; only negotiated clients can
    /// wait indefinitely while releasing work on disconnect.
    pub fn handle_client_request(
        &self,
        request: BrokerRequest,
        negotiated_rpc_read: bool,
    ) -> impl std::future::Future<Output = BrokerResponse> {
        let legacy_rpc_read = !negotiated_rpc_read && self.request_uses_rpc_reads(&request);
        let work = Box::pin(self.handle(request));
        async move {
            if legacy_rpc_read {
                tokio::time::timeout(BROKER_READ_TIMEOUT, work).await.unwrap_or_else(|_| {
                    BrokerResponse::failure(error(
                        "BROKER_READ_TIMEOUT", "read", "The read exceeded its bounded deadline.",
                        "Retry with the updated GoFin CLI to wait for RPC evidence with cancellation support.",
                    ))
                })
            } else {
                work.await
            }
        }
    }

    pub async fn handle(&self, request: BrokerRequest) -> BrokerResponse {
        // Read/control concurrency grants no execution authority. Submission
        // admission remains serialized; slow I/O cannot hide status or revocation.
        let _mutation_guard = if concurrent_read_request(&request) || independent_request(&request)
        {
            None
        } else {
            Some(self.mutation_gate.lock().await)
        };
        let result = if concurrent_read_request(&request) && !self.request_uses_rpc_reads(&request)
        {
            tokio::time::timeout(BROKER_READ_TIMEOUT, self.handle_result(request))
                .await
                .unwrap_or_else(|_| Err(error(
                    "BROKER_READ_TIMEOUT", "read", "The read exceeded its bounded deadline.",
                    "Retry a narrower read; incomplete or timed-out evidence cannot authorize a write.",
                )))
        } else {
            // Never cancel a financial dispatch or reconciliation on a read deadline.
            self.handle_result(request).await
        };
        match result {
            Ok(mut value) => {
                public_submission::redact_public_payload(&mut value);
                BrokerResponse::success(value)
            }
            Err(error) => BrokerResponse::failure(error),
        }
    }

    async fn handle_result(&self, request: BrokerRequest) -> gofin_domain::Result<Value> {
        use BrokerRequest as R;
        let lifecycle_request = matches!(&request, R::DaemonStart | R::DaemonStop);
        if self.protected_config_check.load(Ordering::SeqCst) {
            let stopped = gofin_protocol::daemon::stop_requested_at(
                &self.daemon_marker_path.lock().unwrap(),
                self.daemon_marker_owner_uid,
            )?;
            if stopped && !lifecycle_request {
                self.accepting.store(false, Ordering::SeqCst);
                return Err(error(
                    "DAEMON_STOPPED",
                    "daemon",
                    "The protected broker is stopped and accepts lifecycle requests only.",
                    "Explicitly start the GoFin broker service before sending other requests.",
                ));
            }
        }
        if !self.accepting.load(Ordering::SeqCst) && !lifecycle_request {
            return Err(error(
                "DAEMON_STOPPED",
                "daemon",
                "The broker is stopping.",
                "Start GoFin before dispatching another request; inspect durable operations before retrying writes.",
            ));
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
                | R::ConfigurationPreview { .. }
                | R::DaemonStart
                | R::DaemonStop
                | R::RevokeBoundary { .. }
                | R::BoundaryStatus { .. }
                | R::PlanStatus { .. }
                | R::RevokePlan { .. }
                | R::ArtifactChunk { .. }
                | R::ReleaseArtifact { .. }
        ) || matches!(&request,R::Status { id } if id.starts_with("configuration:"));
        let public_market_read = matches!(&request, R::Read { operation, .. }
            if matches!(operation.as_str(), "exchange.order-book" | "exchange.markets" | "exchange.ticker"));
        if !local_management && !public_market_read {
            self.verify_account_identities(&request).await?;
        }
        match request {
            R::Capabilities => Ok(
                json!({"version":2,"methods":protocol_methods(),"maxFrameBytes":gofin_protocol::MAX_FRAME_BYTES,"maxResponseFrameBytes":gofin_protocol::MAX_RESPONSE_FRAME_BYTES,"exchanges":gofin_discovery::exchange_capabilities()}),
            ),
            R::PolicyShow => {
                let mut view = gofin_discovery::PolicyView::new(
                    &self.config.policy.max_movement_usd_micros,
                    &self.config.policy.max_run_allocation_usd_micros,
                )?;
                view.set_unpriced_policy(self.config.policy.allow_unpriced_exchange_assets);
                match self.engine.allocation_unpriced_operations() {
                    Ok(count) => {
                        view.allocation_unpriced_operations = Some(count);
                        if count > 0 {
                            view.unpriced_assets_warning =
                                Some(gofin_domain::unpriced::WARNING.into());
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
                let derived_hyperliquid_resources = self
                    .config
                    .connections
                    .iter()
                    .filter_map(|connection| match connection {
                        ConnectionConfig::Hyperliquid {
                            resource,
                            auto_derived: true,
                            ..
                        } => Some(resource),
                        _ => None,
                    })
                    .collect::<BTreeSet<_>>();
                connections.retain(|connection| {
                    connection.provider != "hyperliquid"
                        || !derived_hyperliquid_resources.contains(&connection.resource)
                });
                for connection in &self.config.connections {
                    if let ConnectionConfig::CoinGecko {
                        name,
                        resource,
                        api_plan,
                        ..
                    } = connection
                    {
                        connections.push(gofin_domain::ConnectionMetadata { name:name.clone(), provider:"coingecko".into(), resource:resource.clone(), role:None, parent:None, network:None, address:None, credential_ref:None, signing_enabled:false, details:json!({"api_plan":api_plan,"purpose":"USD pricing reads","financial_access":false}) });
                    }
                    if let ConnectionConfig::Jupiter { name, resource, .. } = connection {
                        connections.push(gofin_domain::ConnectionMetadata { name:name.clone(), provider:"jupiter".into(), resource:resource.clone(), role:Some("service".into()), parent:None, network:Some("solana:mainnet-beta".into()), address:None, credential_ref:None, signing_enabled:false, details:json!({"purpose":"Jupiter service access","financial_access":false}) });
                    }
                }
                connections = deduplicate_connections(connections);
                serde_json::to_value(connections).map_err(|_| internal_error())
            }
            R::Doctor => Ok(
                json!({"configurationVersion":self.config.version,"approverConfigured":true,"adapters":self.adapters.iter().flat_map(|a|a.definitions()).map(|d| d.name).collect::<Vec<_>>(),"protectedInstallationRequired":true,"financialReadiness":"requires-live-probes","rpc_health":observed_rpc_health(&self.adapters)}),
            ),
            R::Read { operation, input } => {
                serde_json::to_value(self.engine.read(&operation, &input).await?)
                    .map_err(|_| internal_error())
            }
            R::ReadArtifact { operation, input } => {
                let profile = self
                    .artifact_profiles
                    .lock()
                    .map_err(|_| internal_error())?
                    .iter()
                    .find(|profile| {
                        gofin_exchange::ExchangeProfile::definitions(profile.as_ref())
                            .iter()
                            .any(|d| d.name == operation)
                    })
                    .cloned()
                    .ok_or_else(|| {
                        GoFinError::new(
                            "ARTIFACT_PROVIDER_UNAVAILABLE",
                            "read",
                            "This runtime has no export provider for the selected operation.",
                            "Use a supported enrolled Kraken product and explicit --output path.",
                        )
                    })?;
                let reservation = artifacts::Reservation::begin(self.artifacts.clone())?;
                let (content_type, bytes) = profile.artifact(&operation, &input).await?;
                let provider = gofin_kraken::contracts::contract(
                    gofin_kraken::contracts::endpoint(&operation)?,
                )
                .client
                .as_str();
                reservation.publish(provider, &content_type, bytes)
            }
            R::ArtifactChunk {
                artifact_id,
                offset,
            } => self
                .artifacts
                .lock()
                .map_err(|_| internal_error())?
                .chunk(&artifact_id, offset),
            R::ReleaseArtifact { artifact_id } => Ok(self
                .artifacts
                .lock()
                .map_err(|_| internal_error())?
                .release(&artifact_id)),
            R::Preview { operation, input } => {
                serde_json::to_value(self.engine.preview(&operation, &input).await?)
                    .map_err(|_| internal_error())
            }
            R::ConfigurationPreview { operation, input }
                if configure::is_kraken_operation(&operation) =>
            {
                let profile = self
                    .artifact_profiles
                    .lock()
                    .map_err(|_| internal_error())?
                    .iter()
                    .find(|profile| {
                        gofin_exchange::ExchangeProfile::definitions(profile.as_ref())
                            .iter()
                            .any(|definition| definition.name == operation)
                    })
                    .cloned()
                    .ok_or_else(|| {
                        GoFinError::new(
                            "CONFIGURATION_PROVIDER_UNAVAILABLE",
                            "configuration",
                            "The selected Kraken configuration profile is unavailable.",
                            "Enroll the matching Kraken product before previewing its settings.",
                        )
                    })?;
                let prepared = tokio::time::timeout(
                    CONFIGURATION_PREVIEW_TIMEOUT, configure::preview(&profile, &operation, &input)
                ).await.map_err(|_| GoFinError::new(
                    "CONFIGURATION_PREVIEW_TIMEOUT", "configuration",
                    "The configuration permission check exceeded its deadline; no change was submitted.",
                    "Retry the same dry-run after venue connectivity recovers.",
                ))??;
                serde_json::to_value(prepared).map_err(|_| internal_error())
            }
            R::ConfigurationPreview { operation, input } => serde_json::to_value(
                self.engine
                    .preview_configuration(&operation, &input)
                    .await?,
            )
            .map_err(|_| internal_error()),
            R::PreviewBoundaryAuthorization { request } => {
                self.engine.preview_boundary_authorization(request).await
            }
            R::PreviewPlan { request } => {
                serde_json::to_value(self.engine.preview_financial_plan(request).await?)
                    .map_err(|_| internal_error())
            }
            R::AuthorizePlan { request } => {
                serde_json::to_value(self.engine.authorize_financial_plan(request).await?)
                    .map_err(|_| internal_error())
            }
            R::PlanStatus { plan_id } => {
                serde_json::to_value(self.engine.financial_plan_status(&plan_id).await?)
                    .map_err(|_| internal_error())
            }
            R::RevokePlan { plan_id } => {
                serde_json::to_value(self.engine.revoke_financial_plan(&plan_id).await?)
                    .map_err(|_| internal_error())
            }
            R::PreviewPlanStep {
                plan_id,
                step_id,
                operation,
                input,
            } => serde_json::to_value(
                self.engine
                    .preview_financial_plan_step(&plan_id, &step_id, &operation, &input)
                    .await?,
            )
            .map_err(|_| internal_error()),
            R::ExecutePlanStep {
                plan_id,
                step_id,
                request,
            } => serde_json::to_value(
                self.engine
                    .execute_financial_plan_step(&plan_id, &step_id, request)
                    .await?,
            )
            .map_err(|_| internal_error()),
            R::ReplacePlanCandidate {
                plan_id,
                step_id,
                previous_attempt_id,
                request,
            } => serde_json::to_value(
                self.engine
                    .execute_financial_plan_step_replacing(
                        &plan_id,
                        &step_id,
                        &previous_attempt_id,
                        request,
                    )
                    .await?,
            )
            .map_err(|_| internal_error()),
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
            R::Status { id } if id.starts_with("configuration:") => {
                if !self.protected_config_check.load(Ordering::SeqCst) {
                    return Err(GoFinError::validation(
                        "Protected configuration attempts are not available in this synthetic runtime.",
                    ));
                }
                configure::status(&self.config, &id).await
            }
            R::Status { id } => serde_json::to_value(self.engine.status(&id, true).await?)
                .map_err(|_| internal_error()),
            R::Operations => {
                serde_json::to_value(self.engine.operations()?).map_err(|_| internal_error())
            }
            R::Snapshot => {
                serde_json::to_value(self.snapshot().await?).map_err(|_| internal_error())
            }
            R::RunBegin { .. } | R::RunReplan { .. } | R::RunFinish { .. } => Err(GoFinError::new(
                "RUN_RETIRED",
                "validation",
                "Verification run mutations are retired.",
                "Execute ordinary operations directly; inspect operations with gofin operations and reconcile them with gofin status.",
            )),
            R::DaemonStart => {
                if self.protected_config_check.load(Ordering::SeqCst) {
                    gofin_protocol::daemon::resume_at(
                        &self.daemon_marker_path.lock().unwrap(),
                        self.daemon_marker_owner_uid,
                    )?;
                }
                self.accepting.store(true, Ordering::SeqCst);
                Ok(json!({"status":"running","pid":std::process::id()}))
            }
            R::DaemonStop => {
                if self.protected_config_check.load(Ordering::SeqCst) {
                    gofin_protocol::daemon::request_stop_at(
                        &self.daemon_marker_path.lock().unwrap(),
                        self.daemon_marker_owner_uid,
                    )?;
                }
                self.accepting.store(false, Ordering::SeqCst);
                Ok(json!({"status":"stopping"}))
            }
        }
    }

    pub async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
        self.observer.snapshot().await
    }

    #[cfg(test)]
    async fn begin_run(&self, request: RunBeginRequest) -> gofin_domain::Result<RunResult> {
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
        let observed = self.engine.observe_run_baseline("gofin.snapshot").await?;
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
        if gofin_domain::policy::limit(&self.config.policy.max_run_allocation_usd_micros)?
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
        remaining: Vec<gofin_domain::PlanStep>,
    ) -> gofin_domain::Result<RunResult> {
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
                "gofin.snapshot",
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
    ) -> gofin_domain::Result<Vec<AssetPrice>> {
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

// Diagnostics describe the actual long-lived adapter transports. Reading them
// must not open another client, perform a readiness probe or reveal a URL.
fn observed_rpc_health(adapters: &[Arc<dyn Adapter>]) -> Vec<Value> {
    let mut health = Vec::new();
    for adapter in adapters {
        let snapshots = match adapter.diagnostics() {
            Value::Array(snapshots) => snapshots,
            snapshot => vec![snapshot],
        };
        for snapshot in snapshots {
            if let Some(snapshot) = public_rpc_diagnostics(&snapshot)
                && !health.contains(&snapshot)
            {
                health.push(snapshot);
            }
        }
    }
    health
}

fn public_rpc_diagnostics(value: &Value) -> Option<Value> {
    fn endpoint(value: &Value) -> Value {
        let Some(id) = value.as_str().filter(|id| {
            id.strip_prefix("rpc-").is_some_and(|number| {
                !number.is_empty()
                    && number.len() <= 5
                    && number.bytes().all(|byte| byte.is_ascii_digit())
            })
        }) else {
            return Value::Null;
        };
        json!(id)
    }
    fn reviewed(value: &Value, allowed: &[&str]) -> Value {
        value
            .as_str()
            .filter(|text| allowed.contains(text))
            .map_or(Value::Null, |text| json!(text))
    }
    const METHODS: &[&str] = &[
        "eth_blockNumber",
        "eth_call",
        "eth_chainId",
        "eth_estimateGas",
        "eth_getBalance",
        "eth_getBlockByNumber",
        "eth_getCode",
        "eth_getLogs",
        "eth_getProof",
        "eth_getStorageAt",
        "eth_getTransactionByHash",
        "eth_getTransactionCount",
        "eth_getTransactionReceipt",
        "eth_maxPriorityFeePerGas",
    ];
    const REASONS: &[&str] = &[
        "ok",
        "cancelled",
        "missing",
        "lagging",
        "unsupported",
        "timeout",
        "rate_limited",
        "historical_unavailable",
        "access_denied",
        "unavailable",
        "incompatible",
    ];
    let chain_id = value["chain_id"].as_u64().filter(|id| *id > 0)?;
    let endpoints = value["endpoints"].as_array()?.iter().filter_map(|entry| {
        let id = endpoint(&entry["endpoint_id"]);
        if id.is_null() { return None; }
        let unsupported = entry["unsupported_methods"].as_array().map(|methods| {
            methods.iter().map(|method| {
                if let Some(text) = method.as_str()
                    && let Some((method, context)) = text.split_once(':')
                    && METHODS.contains(&method)
                    && ["canonical", "finalized", "safe"].contains(&context)
                {
                    return json!(text);
                }
                reviewed(method, METHODS)
            })
                .filter(|method| !method.is_null()).collect::<Vec<_>>()
        }).unwrap_or_default();
        Some(json!({
            "endpoint_id":id,
            "identity_verified":entry["identity_verified"].as_bool(),
            "status":reviewed(&entry["status"], &["unobserved", "healthy", "cooling_down", "unavailable", "lagging", "unsupported", "incompatible"]),
            "retry_after_ms":entry["retry_after_ms"].as_u64(),
            "last_error_code":reviewed(&entry["last_error_code"], &[
                "RPC_READ_TIMEOUT", "RPC_ENDPOINT_TIMEOUT", "RPC_RATE_LIMITED",
                "RPC_HISTORICAL_STATE_UNAVAILABLE", "RPC_UNAVAILABLE", "RPC_REQUEST_FAILED",
                "RPC_DATA_INVALID", "RPC_METHOD_UNREVIEWED", "CHAIN_ID_MISMATCH",
                "RPC_CHAIN_MISMATCH", "RPC_METHOD_UNSUPPORTED", "RPC_ACCESS_DENIED", "WRONG_CHAIN",
                "RPC_STALE_READ", "RPC_EVIDENCE_UNAVAILABLE", "RPC_UNSUPPORTED_CAPABILITY", "RPC_INCOMPATIBLE_BLOCK",
            ]),
            "unsupported_methods":unsupported,
        }))
    }).collect::<Vec<_>>();
    let read_observation = |observation: &Value| {
        let Some(_) = observation.as_object() else {
            return Value::Null;
        };
        let attempts = observation["attempts"]
            .as_array()
            .map(|attempts| {
                attempts.iter().filter_map(|attempt| {
                let id = endpoint(&attempt["endpoint_id"]);
                if id.is_null() { return None; }
                Some(json!({"endpoint_id":id,"reason":reviewed(&attempt["reason"], REASONS)}))
            }).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        json!({
            "method":reviewed(&observation["method"], METHODS),
            "selected_endpoint":endpoint(&observation["selected_endpoint"]),
            "attempts":attempts,
            "outcome":reviewed(&observation["outcome"], &["ok", "error", "cancelled", "missing"]),
        })
    };
    let mut public = json!({"chain_id":chain_id,"read_timeout_ms":value["read_timeout_ms"].as_u64(),"endpoints":endpoints,"last_read":read_observation(&value["last_read"])});
    if let Some(observation) = value.get("last_failover") {
        public["last_failover"] = read_observation(observation);
    }
    Some(public)
}

/// Collapse repeated view rows caused by composite adapters also exposing the
/// metadata of their constituent providers. Jupiter's wallet-backed operations
/// share the enrolled Solana wallet identity, even though their roles and
/// details differ. Keep that wallet as the single public connection while
/// retaining distinct identities and standalone service connections.
fn deduplicate_connections(
    connections: Vec<gofin_domain::ConnectionMetadata>,
) -> Vec<gofin_domain::ConnectionMetadata> {
    let solana_wallets = connections
        .iter()
        .filter(|connection| connection.provider == "solana")
        .cloned()
        .collect::<Vec<_>>();
    let mut unique = Vec::with_capacity(connections.len());
    for connection in connections {
        if matches!(connection.provider.as_str(), "jupiter" | "jupiter-trigger")
            && solana_wallets.iter().any(|wallet| {
                wallet.name == connection.name
                    && wallet.resource == connection.resource
                    && wallet.network == connection.network
                    && wallet.address == connection.address
            })
        {
            continue;
        }
        if !unique.contains(&connection) {
            unique.push(connection);
        }
    }
    unique
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

fn snapshot_connection_matches(
    scope: &SnapshotScope,
    connection: &gofin_domain::ConnectionMetadata,
) -> bool {
    let related_binance = connection.provider == "binance"
        && scope
            .resources
            .iter()
            .any(|resource| resource.0 == format!("{}:USD_M", connection.resource.0));
    let grouped_evm = matches!(connection.provider.as_str(), "evm" | "ethereum")
        && connection
            .network
            .as_deref()
            .is_some_and(|network| scope.matches_network(network))
        && connection.address.as_deref().is_some_and(|address| {
            scope
                .resources
                .iter()
                .any(|resource| resource.0.eq_ignore_ascii_case(&format!("evm:{address}")))
        });
    (scope.matches_connection(connection) || related_binance || grouped_evm)
        && (connection.provider != "binance" || scope.matches_network("binance"))
}

fn observer_label(adapter: &dyn Adapter, scope: &SnapshotScope) -> String {
    let mut identities = adapter
        .snapshot_identities()
        .into_iter()
        .filter(|connection| scope.is_full() || snapshot_connection_matches(scope, connection))
        .map(|connection| {
            format!(
                "{}@{}",
                connection.resource.0,
                connection.network.unwrap_or(connection.provider)
            )
        })
        .collect::<Vec<_>>();
    identities.sort();
    identities.dedup();
    if identities.is_empty() {
        identities = adapter
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
    }
    identities.join(",")
}

impl CompositeObserver {
    async fn observe(&self, scope: &SnapshotScope) -> gofin_domain::Result<StateSnapshot> {
        let mut combined = StateSnapshot {
            state: FinancialState::default(),
            observed_at: now(),
            complete: true,
            coverage: vec![],
            pending: vec![],
        };
        let selected = self
            .adapters
            .iter()
            .filter(|adapter| {
                scope.is_full()
                    || adapter.snapshot_is_global()
                    || adapter
                        .snapshot_identities()
                        .iter()
                        .any(|connection| snapshot_connection_matches(scope, connection))
            })
            .cloned()
            .collect::<Vec<_>>();
        if !scope.is_full() {
            combined.coverage.push(format!(
                "requested-scope:{}",
                serde_json::to_string(scope).map_err(|_| internal_error())?
            ));
            if selected.is_empty() {
                combined.complete = false;
                combined.pending.push("scope:no-matching-observer".into());
            }
        }
        let selected_networks = selected
            .iter()
            .flat_map(|adapter| adapter.snapshot_identities())
            .filter(|connection| snapshot_connection_matches(scope, connection))
            .filter_map(|connection| {
                connection
                    .network
                    .or_else(|| (connection.provider == "binance").then_some("binance".into()))
            })
            .collect::<BTreeSet<_>>();
        for network in scope
            .networks
            .iter()
            .chain(scope.assets.iter().map(|asset| &asset.network))
        {
            if !selected_networks.contains(network) {
                combined.complete = false;
                combined
                    .pending
                    .push(format!("scope:network-not-covered:{network}"));
            }
        }
        let slots = Arc::new(Semaphore::new(MAX_READ_OBSERVERS));
        let mut observers = JoinSet::new();
        let scope_input = serde_json::to_value(scope).map_err(|_| internal_error())?;
        for (index, adapter) in selected.into_iter().enumerate() {
            let rpc_read = adapter.uses_rpc_reads("gofin.snapshot", &scope_input);
            let scope = scope.clone();
            let slots = slots.clone();
            observers.spawn(async move {
                let label = observer_label(adapter.as_ref(), &scope);
                // RPC observers can wait until cancellation. Other integrations
                // keep the original deadline, including time queued for a slot.
                // Dropping this JoinSet cancels running and queued observers.
                let observation = async {
                    let _slot = slots
                        .acquire_owned()
                        .await
                        .expect("observer slots stay open");
                    adapter.snapshot_scoped(&scope).await
                };
                let result = if rpc_read {
                    observation.await
                } else {
                    tokio::time::timeout(NON_RPC_OBSERVER_READ_TIMEOUT, observation)
                        .await.unwrap_or_else(|_| Err(error("OBSERVER_READ_TIMEOUT", "snapshot",
                            "The selected snapshot observer exceeded its read deadline.",
                            "Retry the reported observer/network with a narrower scope; incomplete evidence cannot authorize a write.")))
                };
                (index, label, result)
            });
        }
        let mut results = Vec::new();
        while let Some(result) = observers.join_next().await {
            match result {
                Ok(result) => results.push(result),
                Err(_) => {
                    combined.complete = false;
                    combined.pending.push("observer:task-failed".into());
                }
            }
        }
        // Preserve stable state ordering independently of RPC completion order.
        results.sort_by_key(|(index, _, _)| *index);
        for (_, label, result) in results {
            match result {
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
                    combined
                        .pending
                        .push(format!("observer:{label}:{}", error.code));
                }
            }
        }
        for resource in &scope.resources {
            if !combined.coverage.contains(&resource.0) {
                combined.complete = false;
                combined
                    .pending
                    .push(format!("scope:resource-not-covered:{}", resource.0));
            }
        }
        for asset in &scope.assets {
            if !combined
                .state
                .holdings
                .iter()
                .any(|holding| holding.amount.asset == *asset)
            {
                combined.complete = false;
                combined.pending.push(format!(
                    "scope:asset-not-covered:{}:{}:{}",
                    asset.network, asset.reference, asset.decimals
                ));
            }
        }
        if let Some(engine) = self
            .engine
            .lock()
            .map_err(|_| internal_error())?
            .as_ref()
            .and_then(Weak::upgrade)
        {
            // Read records are journaled while the adapter runs, but they do
            // not represent unresolved financial movements. In particular,
            // `gofin.snapshot` can itself be a plan read, so count writes and
            // unknown records conservatively while excluding registered reads.
            let mut read_operations = self
                .adapters
                .iter()
                .flat_map(|adapter| adapter.definitions())
                .filter(|definition| definition.kind == OperationKind::Read)
                .map(|definition| definition.name)
                .collect::<BTreeSet<_>>();
            read_operations.insert("gofin.snapshot".into());
            read_operations.insert("fin.snapshot".into()); // Historical read journal rows.
            for record in engine.operations()? {
                if !record.status.terminal() && !read_operations.contains(&record.operation) {
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
}

#[async_trait]
impl Adapter for CompositeObserver {
    fn uses_rpc_reads(&self, operation: &str, input: &Value) -> bool {
        if !matches!(operation, "gofin.snapshot" | "fin.snapshot") {
            return false;
        }
        let Ok(scope) = serde_json::from_value::<SnapshotScope>(input.clone()) else {
            return false;
        };
        self.adapters.iter().any(|adapter| {
            (scope.is_full()
                || adapter.snapshot_is_global()
                || adapter
                    .snapshot_identities()
                    .iter()
                    .any(|connection| snapshot_connection_matches(&scope, connection)))
                && adapter.uses_rpc_reads(operation, input)
        })
    }
    fn definitions(&self) -> Vec<Definition> {
        vec![snapshot_definition()]
    }

    fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        self.adapters
            .iter()
            .flat_map(|adapter| adapter.connections())
            .collect()
    }

    fn authorization_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        self.adapters
            .iter()
            .flat_map(|adapter| adapter.authorization_identities())
            .collect()
    }

    // This adapter is an aggregate read view. Its component connections are
    // already registered as individual observers, so advertising them here
    // would make baseline capture select the aggregate and duplicate reads.
    // Keep `gofin.snapshot` behavior in snapshot_scoped/read unchanged.
    fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
        Vec::new()
    }

    async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
        self.observe(&SnapshotScope::default()).await
    }

    async fn snapshot_scoped(&self, scope: &SnapshotScope) -> gofin_domain::Result<StateSnapshot> {
        self.observe(scope).await
    }

    async fn read(&self, operation: &str, input: &Value) -> gofin_domain::Result<Value> {
        if !matches!(operation, "gofin.snapshot" | "fin.snapshot") {
            return Err(GoFinError::validation(
                "Unknown composite observer operation.",
            ));
        }
        let scope: SnapshotScope = serde_json::from_value(input.clone()).map_err(|_| {
            GoFinError::validation(
                "Invalid snapshot scope; inspect gofin schema gofin.snapshot --json.",
            )
        })?;
        serde_json::to_value(self.snapshot_scoped(&scope).await?).map_err(|_| internal_error())
    }
    async fn prepare(
        &self,
        _operation: &str,
        _input: &Value,
    ) -> gofin_domain::Result<PreparedOperation> {
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
    ) -> gofin_domain::Result<PreparedOperation> {
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
    ) -> gofin_domain::Result<Submission> {
        Err(error(
            "READ_ONLY_OPERATION",
            "validation",
            "Composite observer has no write operations.",
            "Use supported application write previews.",
        ))
    }
    async fn reconcile(&self, _record: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
        Err(error(
            "READ_ONLY_OPERATION",
            "reconciliation",
            "Composite observer has no write operations.",
            "Reconcile through the originating application adapter.",
        ))
    }
}

#[cfg(test)]
fn parse_atoms(value: &str) -> gofin_domain::Result<num_bigint::BigUint> {
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
        "read_artifact",
        "artifact_chunk",
        "release_artifact",
        "preview",
        "configuration_preview",
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

fn internal_error() -> GoFinError {
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
pub fn open_protected_store() -> gofin_domain::Result<Store> {
    validate_installation()?;
    validate_private_file_parent(Path::new(OPERATIONS_PATH))?;
    Store::open(OPERATIONS_PATH).map_err(store_startup_error)
}

fn store_startup_error(store_error: gofin_store::StoreError) -> GoFinError {
    let (error_class, sqlite_extended_code) = store_error.startup_diagnostic();
    store_startup_error_from_diagnostic(error_class, sqlite_extended_code)
}

fn store_startup_error_from_diagnostic(
    error_class: &'static str,
    sqlite_extended_code: Option<i32>,
) -> GoFinError {
    let (message, next_action) = if error_class == "sqlite_cannot_open" {
        (
            "SQLite could not open the protected journal or its support files.",
            "Preserve the journal. Have a human inspect protected storage permissions and available disk space; do not reset or delete it.",
        )
    } else {
        (
            "The protected operation store is unavailable.",
            "Preserve the journal. Have a human inspect the protected store diagnostics; do not reset or delete it.",
        )
    };
    let mut details = serde_json::Map::new();
    details.insert("error_class".into(), json!(error_class));
    if let Some(code) = sqlite_extended_code {
        details.insert("sqlite_extended_code".into(), json!(code));
    }
    // launchd captures the concise human error output, where JSON details are
    // absent. Include only the same static class/numeric code in that output.
    let diagnostic = match sqlite_extended_code {
        Some(code) => format!("{error_class}, SQLite code {code}"),
        None => error_class.to_owned(),
    };
    error(
        "STORE_UNAVAILABLE",
        "startup",
        &format!("{message} ({diagnostic})"),
        next_action,
    )
    .with_details(Value::Object(details))
}

#[cfg(test)]
mod store_startup_diagnostic_tests {
    use super::*;

    #[test]
    fn store_error_keeps_public_code_and_exposes_only_safe_classification() {
        let error = store_startup_error(gofin_store::StoreError::Io(
            "secret /Library/GoFin/private/operations.sqlite detail".into(),
        ));
        assert_eq!(error.code, "STORE_UNAVAILABLE");
        assert_eq!(error.stage, "startup");
        assert_eq!(
            error.details.unwrap().as_ref(),
            &json!({"error_class":"filesystem"})
        );
        assert!(!error.message.contains("operations.sqlite"));
        assert!(!error.next_action.contains("operations.sqlite"));
    }

    #[test]
    fn sqlite_open_failure_gets_preserve_first_guidance_and_numeric_code() {
        let error = store_startup_error_from_diagnostic("sqlite_cannot_open", Some(14));
        assert_eq!(error.code, "STORE_UNAVAILABLE");
        assert!(error.message.contains("support files"));
        assert!(error.next_action.contains("Preserve the journal"));
        assert!(error.next_action.contains("do not reset or delete"));
        assert_eq!(
            error.details.unwrap().as_ref(),
            &json!({
                "error_class":"sqlite_cannot_open",
                "sqlite_extended_code":14
            })
        );
    }
}

pub fn load_protected_config() -> gofin_domain::Result<BrokerConfigV2> {
    let config = load_stored_protected_config()?;
    enrollment::effective_protected_config(config)
}

/// Read the protected configuration without runtime-only reviewed aliases.
/// Enrollment writes this persisted form so derived connections cannot become
/// stale credentials after their parent wallet is changed or removed.
pub(crate) fn load_stored_protected_config() -> gofin_domain::Result<BrokerConfigV2> {
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
    Ok(config)
}

/// Enrollment helper. It is not exposed over the agent protocol and never
/// accepts financial actions; root's human terminal owns calls to this API.
pub fn save_protected_config(config: &BrokerConfigV2) -> gofin_domain::Result<()> {
    validate_installation()?;
    let mut persisted = config.clone();
    strip_derived_hyperliquid_links(&mut persisted);
    persisted.validate()?;
    let path = Path::new(CONFIG_PATH);
    let parent = path.parent().expect("fixed config parent");
    validate_metadata(parent, false)?;
    let bytes = serde_json::to_vec(&persisted).map_err(|_| {
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

pub(crate) fn strip_derived_hyperliquid_links(config: &mut BrokerConfigV2) {
    config.connections.retain(|connection| {
        !matches!(
            connection,
            ConnectionConfig::Hyperliquid {
                auto_derived: true,
                ..
            }
        )
    });
}

fn atomic_replace(parent: &Path, destination: &Path, bytes: &[u8]) -> gofin_domain::Result<()> {
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

pub fn lock_protected_configuration(exclusive: bool) -> gofin_domain::Result<ConfigurationLock> {
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
) -> gofin_domain::Result<Option<ConfigurationLock>> {
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

fn protected_lock_file() -> gofin_domain::Result<File> {
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

fn validate_private_file_parent(path: &Path) -> gofin_domain::Result<()> {
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
) -> gofin_domain::Result<()> {
    serve_connection_with_timeouts(
        stream,
        &RwLock::new(runtime.clone()),
        expected_uid,
        IpcTimeouts::PRODUCTION,
    )
    .await
}

#[async_trait]
trait RuntimeLoader: Send + Sync {
    async fn load(&self, config: BrokerConfigV2) -> gofin_domain::Result<Arc<BrokerRuntime>>;
}

struct ProtectedRuntimeLoader;
#[async_trait]
impl RuntimeLoader for ProtectedRuntimeLoader {
    async fn load(&self, config: BrokerConfigV2) -> gofin_domain::Result<Arc<BrokerRuntime>> {
        protected_runtime_from_config(config).await
    }
}

async fn refresh_runtime(
    runtime: &mut Arc<BrokerRuntime>,
    current: BrokerConfigV2,
    expected_uid: u32,
    loader: &dyn RuntimeLoader,
) -> gofin_domain::Result<()> {
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
    shared_runtime: &RwLock<Arc<BrokerRuntime>>,
    expected_uid: u32,
    timeouts: IpcTimeouts,
) -> gofin_domain::Result<()> {
    let peer_uid = peer_uid(&stream)?;
    if peer_uid != expected_uid {
        return Err(error(
            "IPC_PEER_DENIED",
            "protocol",
            "Local broker peer UID is not the configured human approver.",
            "Connect from the enrolled human account.",
        ));
    }
    let envelope = {
        let mut reader = BufReader::new(&mut stream);
        tokio::time::timeout(timeouts.frame_read, read_request_envelope(&mut reader))
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
    let protected = shared_runtime
        .read()
        .await
        .protected_config_check
        .load(Ordering::SeqCst);
    let _configuration_guard = if protected {
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
        let refreshed = async {
            let current = load_protected_config()?;
            let unchanged = serde_json::to_value(shared_runtime.read().await.configuration())
                .map_err(|_| internal_error())?
                == serde_json::to_value(&current).map_err(|_| internal_error())?;
            if !unchanged {
                let mut runtime = shared_runtime.write().await;
                refresh_runtime(&mut runtime, current, expected_uid, &ProtectedRuntimeLoader)
                    .await?;
            }
            Ok::<_, GoFinError>(())
        }
        .await;
        if let Err(error) = refreshed {
            write_ipc_response(
                &mut stream,
                &BrokerResponse::failure(error),
                timeouts.response_write,
            )
            .await?;
            return Ok(());
        }
        Some(guard)
    } else {
        None
    };
    // Keep this runtime revision pinned throughout dispatch. A reload waits for
    // active reads/writes and swaps only after protected composition succeeds.
    let runtime = shared_runtime.read().await;
    let cancellable_read =
        envelope.cancellable_read() && runtime.request_uses_rpc_reads(&envelope.request);
    let Some(response) = gofin_protocol::progress::observe_request(
        &mut stream,
        envelope.progress,
        cancellable_read,
        runtime.handle_client_request(envelope.request, cancellable_read),
    )
    .await
    else {
        return Ok(());
    };
    write_ipc_response(&mut stream, &response, timeouts.response_write).await?;
    Ok(())
}

async fn acquire_request_configuration_lock(
    timeout: Duration,
) -> gofin_domain::Result<ConfigurationLock> {
    acquire_configuration_lock(false, timeout).await
}

async fn acquire_configuration_lock(
    exclusive: bool,
    timeout: Duration,
) -> gofin_domain::Result<ConfigurationLock> {
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(lock) = try_lock_protected_configuration(exclusive)? {
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
) -> gofin_domain::Result<()> {
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
) -> gofin_domain::Result<()> {
    serve_listener_with_timeouts(listener, runtime, expected_uid, IpcTimeouts::PRODUCTION).await
}

// Dropping a caller's listener future must not abort an already started
// financial dispatch. Normal stop drains; cancellation detaches tasks so their
// immutable journaled identity can finish safely while the runtime remains alive.
struct RequestTasks(JoinSet<gofin_domain::Result<()>>);
impl Drop for RequestTasks {
    fn drop(&mut self) {
        self.0.detach_all();
    }
}

fn peer_local_error(error: &GoFinError) -> bool {
    matches!(
        error.code.as_str(),
        "IPC_PEER_DENIED"
            | "INVALID_PROTOCOL_FRAME"
            | "IPC_READ_TIMEOUT"
            | "IPC_WRITE_FAILED"
            | "IPC_WRITE_TIMEOUT"
    )
}

async fn serve_listener_with_timeouts(
    listener: UnixListener,
    runtime: Arc<BrokerRuntime>,
    expected_uid: u32,
    timeouts: IpcTimeouts,
) -> gofin_domain::Result<()> {
    // Synthetic listeners own their isolated journal. Production performs this
    // step after launchd activation, under the protected configuration lock.
    runtime.engine.recover_interrupted()?;
    serve_started_listener(listener, runtime, expected_uid, timeouts).await
}

async fn serve_started_listener(
    listener: UnixListener,
    runtime: Arc<BrokerRuntime>,
    expected_uid: u32,
    timeouts: IpcTimeouts,
) -> gofin_domain::Result<()> {
    let runtime = Arc::new(RwLock::new(runtime));
    let mut tasks = RequestTasks(JoinSet::new());
    let mut failure = None;
    while runtime.read().await.accepting.load(Ordering::SeqCst) {
        tokio::select! {
            finished = tasks.0.join_next(), if !tasks.0.is_empty() => {
                match finished {
                    Some(Ok(Err(error))) if !peer_local_error(&error) => { failure = Some(error); break; }
                    Some(Err(_)) => { failure = Some(internal_error()); break; }
                    _ => {}
                }
            }
            accepted = listener.accept(), if tasks.0.len() < MAX_BROKER_CONNECTIONS => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(_) => {
                        failure = Some(error("IPC_ACCEPT_FAILED", "startup",
                            "The protected broker socket stopped accepting connections.",
                            "Restart through launchd after checking the durable journal."));
                        break;
                    }
                };
                let runtime = runtime.clone();
                tasks.0.spawn(async move {
                    serve_connection_with_timeouts(stream, &runtime, expected_uid, timeouts).await
                });
            }
        }
    }
    // No new peers are accepted during drain, but submitted writes are allowed
    // to finish recording their result. Socket cancellation never resubmits.
    while let Some(finished) = tasks.0.join_next().await {
        match finished {
            Ok(Err(error)) if !peer_local_error(&error) && failure.is_none() => {
                failure = Some(error)
            }
            Err(_) if failure.is_none() => failure = Some(internal_error()),
            _ => {}
        }
    }
    failure.map_or(Ok(()), Err)
}

pub async fn serve_protected(runtime: Arc<BrokerRuntime>) -> gofin_domain::Result<()> {
    validate_installation()?;
    let approver_uid = runtime.config.approver_uid;
    validate_metadata(Path::new("/Library/GoFin/run"), false)?;
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
        // Socket ownership must be established before recovery can declare any
        // previous work interrupted. Exclude separate configuration processes;
        // ordinary composition and reload must never cancel active attempts.
        let startup_guard =
            acquire_configuration_lock(true, IPC_CONFIGURATION_LOCK_TIMEOUT).await?;
        runtime.engine.recover_interrupted()?;
        drop(startup_guard);
        serve_started_listener(listener, runtime, approver_uid, IpcTimeouts::PRODUCTION).await
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
fn peer_uid(stream: &UnixStream) -> gofin_domain::Result<u32> {
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
) -> gofin_domain::Result<bool> {
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

/// Detect unresolved Hyperliquid work owned by a master signer. Unlike a
/// single account resource, one signing identity may have owner, delegated,
/// spot, perp, unified, DEX and nonce-recovery records. The prepared binding
/// carries the exact master owner for every such action.
pub(crate) fn hyperliquid_owner_has_pending_operations(
    store: &Store,
    owner: &str,
) -> gofin_domain::Result<bool> {
    let records = store.list_records().map_err(|_| {
        error(
            "STORE_UNAVAILABLE",
            "configuration",
            "Pending-operation state could not be inspected.",
            "Preserve this EVM signer mapping until the broker journal is available.",
        )
    })?;
    for value in records {
        if record_reserves_hyperliquid_owner(&value, owner)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn record_reserves_hyperliquid_owner(value: &Value, owner: &str) -> gofin_domain::Result<bool> {
    let record: gofin_domain::OperationRecord =
        serde_json::from_value(value.clone()).map_err(|_| {
            error(
                "STORE_INVALID_RECORD",
                "configuration",
                "A durable operation record is malformed.",
                "Do not change signer mappings until the journal has been repaired and reconciled.",
            )
        })?;
    if record.status.terminal() || !record.operation.starts_with("hyperliquid.") {
        return Ok(false);
    }
    let binding = &record.preview.as_ref().ok_or_else(|| {
        error(
            "STORE_INVALID_RECORD",
            "configuration",
            "A nonterminal Hyperliquid operation has no durable account binding.",
            "Preserve all reusable signer mappings until the record is repaired and reconciled.",
        )
    })?.binding;
    let bound_owner = binding
        .get("account")
        .and_then(|account| account.get("owner"))
        .and_then(Value::as_str)
        .or_else(|| {
            // The broker-composed EVM-to-Hyperliquid deposit wraps the
            // adapter binding and carries its authenticated owner at top level.
            (record.operation == "hyperliquid.account.deposit")
                .then(|| binding.get("owner").and_then(Value::as_str))
                .flatten()
        })
        .ok_or_else(|| {
            error(
                "STORE_INVALID_RECORD",
                "configuration",
                "A nonterminal Hyperliquid operation has no master-owner binding.",
                "Preserve all reusable signer mappings until the record is repaired and reconciled.",
            )
        })?;
    Ok(bound_owner.eq_ignore_ascii_case(owner))
}

fn record_reserves_resource(value: &Value, resource: &ResourceId) -> gofin_domain::Result<bool> {
    let record: gofin_domain::OperationRecord = serde_json::from_value(value.clone()).map_err(|_| {
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
    mod snapshot_read_tests;
    use gofin_domain::boundaries::BoundaryAuthorizationRequest;
    use gofin_domain::{
        Amount, OperationKind, OperationRequest, PreparedOperation, Quantity, ResourceAmount,
        Submission,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn operation_catalog_advertises_generic_exact_plan_support_by_actor_and_kind() {
        let definitions = operation_definitions();
        for definition in &definitions {
            if definition.name == "coinbase.withdraw" {
                assert_eq!(
                    definition.semantics["plan_authorization"]["mode"],
                    "exact_input"
                );
                assert_eq!(definition.semantics["feePolicy"]["reserveIsFeeCap"], false);
                assert_eq!(
                    definition.semantics["feeConsentScopes"],
                    json!(["standalone_exact", "named_plan_exact"])
                );
            }
            if definition.semantics["standalone_only"] == true {
                assert_eq!(
                    definition.semantics["plan_authorization"]["mode"],
                    "unsupported"
                );
                assert_eq!(definition.semantics["feePolicy"]["reserveIsFeeCap"], false);
                continue;
            }
            let eligible = !definition.name.starts_with("bridge.")
                && !(definition.semantics["plan_only"] == true
                    && definition.semantics["plan_permission"]["mode"] == "agent_nominated")
                && matches!(
                    (&definition.kind, &definition.execution_actor),
                    (
                        OperationKind::Write,
                        gofin_domain::ExecutionActor::AgentInvocable
                    ) | (
                        OperationKind::Read,
                        gofin_domain::ExecutionActor::AgentReadable
                    )
                );
            assert_eq!(
                definition
                    .semantics
                    .get("plan_authorization")
                    .and_then(Value::as_object)
                    .is_some(),
                eligible,
                "{}",
                definition.name
            );
            if eligible {
                assert_eq!(
                    definition.semantics["plan_authorization"]["mode"],
                    "exact_input"
                );
                assert_eq!(
                    definition.semantics["plan_authorization"]["approval_requires_future_balances"],
                    false
                );
                jsonschema::options()
                    .offline()
                    .build(&definition.input_schema)
                    .unwrap_or_else(|error| {
                        panic!(
                            "eligible plan operation {} has an invalid or unavailable input schema: {error}",
                            definition.name
                        )
                    });
            }
        }
        let deposit = definitions
            .iter()
            .find(|definition| definition.name == "binance.deposit")
            .unwrap();
        assert_eq!(
            deposit.semantics["broker_generated_attempt_input_fields"],
            json!(["/requestId"])
        );
        assert!(
            deposit.input_schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "requestId"))
        );
        assert_eq!(
            deposit.input_schema["properties"]["requestId"]["type"],
            "string"
        );
        let nominated_swap = definitions
            .iter()
            .find(|definition| definition.name == "swap.execute")
            .unwrap();
        assert!(nominated_swap.semantics.get("plan_authorization").is_none());
        assert_eq!(
            nominated_swap.semantics["plan_permission"]["mode"],
            "agent_nominated"
        );
    }

    struct ReadAdapter {
        captures: Arc<AtomicUsize>,
        pricing_snapshot: Option<Arc<Mutex<StateSnapshot>>>,
        pricing_preflight: Option<Arc<AtomicBool>>,
        identity_probe: Option<(Arc<AtomicBool>, Arc<AtomicUsize>)>,
        connection_metadata: Vec<gofin_domain::ConnectionMetadata>,
    }

    struct PositionBaselineCaptureAdapter {
        resource: ResourceId,
        prepared: Arc<AtomicBool>,
        snapshots: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Adapter for PositionBaselineCaptureAdapter {
        fn definitions(&self) -> Vec<Definition> {
            vec![Definition {
                name: "synthetic.position.close".into(),
                description: "Synthetic position baseline regression".into(),
                kind: OperationKind::Write,
                execution_actor: ExecutionActor::AgentInvocable,
                input_schema: json!({"type":"object","additionalProperties":false}),
                output_schema: json!({"type":"object"}),
                error_schema: json!({"$ref":"GoFinError"}),
                examples: vec![],
                coverage: "Synthetic fixture".into(),
                completion: "Synthetic fixture".into(),
                semantics: json!({}),
            }]
        }

        fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            vec![gofin_domain::ConnectionMetadata {
                name: "synthetic-position-wallet".into(),
                provider: "synthetic-position".into(),
                resource: self.resource.clone(),
                role: None,
                parent: None,
                network: Some("solana:mainnet-beta".into()),
                address: Some("SYNTHETIC".into()),
                credential_ref: None,
                signing_enabled: false,
                details: json!({"fixture":true}),
            }]
        }

        async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
            assert!(
                self.prepared.load(Ordering::SeqCst),
                "position capture runs after the current operation row is prepared"
            );
            self.snapshots.fetch_add(1, Ordering::SeqCst);
            let asset = AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "native".into(),
                decimals: 9,
            };
            let amount = Amount {
                asset: asset.clone(),
                atoms: Quantity::new("2039280")?,
            };
            let anchor = gofin_domain::PositionAnchor {
                resource: self.resource.clone(),
                position: "account-funding:SYNTHETIC".into(),
                position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                principal: amount.clone(),
                scaled_receipt: None,
                evidence_version: 1,
                evidence: json!({"fixture":"finalized synthetic account"}),
                observed_at: now(),
            };
            Ok(StateSnapshot {
                state: FinancialState {
                    positions: vec![gofin_domain::PositionState {
                        resource: self.resource.clone(),
                        position: anchor.position.clone(),
                        kind: anchor.position_kind.clone(),
                        amount,
                        scaled_receipt: None,
                        anchor: Some(anchor),
                    }],
                    ..FinancialState::default()
                },
                observed_at: now(),
                complete: true,
                coverage: vec![self.resource.0.clone()],
                pending: vec![],
            })
        }

        async fn snapshot_scoped(
            &self,
            scope: &SnapshotScope,
        ) -> gofin_domain::Result<StateSnapshot> {
            assert_eq!(scope.resources, vec![self.resource.clone()]);
            self.snapshot().await
        }

        async fn read(&self, _: &str, _: &Value) -> gofin_domain::Result<Value> {
            Ok(json!({}))
        }

        async fn prepare(
            &self,
            operation: &str,
            input: &Value,
        ) -> gofin_domain::Result<PreparedOperation> {
            self.prepared.store(true, Ordering::SeqCst);
            let asset = AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "native".into(),
                decimals: 9,
            };
            let at = Utc::now();
            Ok(PreparedOperation {
                operation: operation.into(),
                input: input.clone(),
                binding: json!({"position":"external-account"}),
                summary: "Close a synthetic observed position".into(),
                checks: vec![],
                warnings: vec![],
                observed_at: at.to_rfc3339(),
                expires_at: (at + chrono::Duration::minutes(2)).to_rfc3339(),
                valuation: Some(gofin_domain::Valuation {
                    movement_usd_micros: Quantity::zero(),
                    maximum_fee_usd_micros: Quantity::zero(),
                    additional_allocation_usd_micros: Quantity::zero(),
                    observed_at: now(),
                    source: "synthetic baseline regression".into(),
                    prices: vec![],
                }),
                intent: gofin_domain::FinancialIntent {
                    debits: vec![],
                    maximum_costs: vec![],
                    expected: vec![gofin_domain::EffectConstraint::PositionOfKind {
                        resource: self.resource.clone(),
                        position: "account-funding:SYNTHETIC".into(),
                        asset,
                        position_kind: gofin_domain::PositionKind::RefundableAccountFunding,
                        direction: gofin_domain::Direction::Decrease,
                        maximum_change: Quantity::new("2039280")?,
                    }],
                    locks: vec![self.resource.clone()],
                },
                operation_version: 1,
                action_deadline: None,
            })
        }

        async fn revalidate(
            &self,
            prepared: &PreparedOperation,
        ) -> gofin_domain::Result<PreparedOperation> {
            Ok(prepared.clone())
        }

        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> gofin_domain::Result<Submission> {
            Err(error(
                "SYNTHETIC_STOP_AFTER_BASELINE",
                "submit",
                "Synthetic regression stops after proving pre-dispatch baseline capture.",
                "No transaction was sent by this test.",
            ))
        }

        async fn reconcile(&self, _: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
            Err(GoFinError::validation("Unused in baseline regression."))
        }
    }

    struct PositionBaselineAuthorizer;
    #[async_trait]
    impl Authorizer for PositionBaselineAuthorizer {
        async fn authorize(
            &self,
            challenge: &gofin_domain::AuthorizationChallenge,
        ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
            Ok(gofin_domain::ApprovalAudit {
                mechanism: "synthetic-position-baseline-test".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[async_trait]
    impl Adapter for ReadAdapter {
        fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            self.connection_metadata.clone()
        }
        fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            if !self.connection_metadata.is_empty() {
                return self.connection_metadata.clone();
            }
            vec![gofin_domain::ConnectionMetadata {
                name: "synthetic-observer".into(),
                provider: "fixture".into(),
                resource: ResourceId("fixture:account".into()),
                role: None,
                parent: None,
                network: Some("fixture".into()),
                address: None,
                credential_ref: None,
                signing_enabled: false,
                details: json!({}),
            }]
        }
        fn definitions(&self) -> Vec<Definition> {
            let mut definitions = vec![
                Definition {
                    name: "fixture.read".into(),
                    description: "Synthetic read fixture".into(),
                    kind: OperationKind::Read,
                    execution_actor: ExecutionActor::AgentReadable,
                    input_schema: json!({"type":"object","additionalProperties":false}),
                    output_schema: json!({"type":"object"}),
                    error_schema: json!({"$ref":"GoFinError"}),
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
                    error_schema: json!({"$ref":"GoFinError"}),
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
        async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
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
        async fn read(&self, operation: &str, _: &Value) -> gofin_domain::Result<Value> {
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
        async fn prepare(&self, _: &str, _: &Value) -> gofin_domain::Result<PreparedOperation> {
            Err(GoFinError::validation("Synthetic read-only adapter."))
        }
        async fn revalidate(
            &self,
            _: &PreparedOperation,
        ) -> gofin_domain::Result<PreparedOperation> {
            Err(GoFinError::validation("Synthetic read-only adapter."))
        }
        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> gofin_domain::Result<Submission> {
            Err(GoFinError::validation("Synthetic read-only adapter."))
        }
        async fn reconcile(&self, _: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
            Err(GoFinError::validation("Synthetic read-only adapter."))
        }
        async fn preflight_return_route(
            &self,
            operation: &str,
            input: &Value,
            context: &gofin_domain::ReturnRouteContext,
        ) -> gofin_domain::Result<gofin_domain::ReturnRouteProof> {
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
            Ok(gofin_domain::ReturnRouteProof {
                step_index: context.step_index,
                operation: operation.into(),
                input_hash: gofin_domain::binding_hash(input)?,
                observed_at: now(),
                expires_at: (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
                intent: gofin_domain::FinancialIntent {
                    debits: vec![],
                    maximum_costs: vec![],
                    expected: vec![gofin_domain::EffectConstraint::Authority {
                        resource: resource.clone(),
                        authority: "synthetic-pricing-authority".into(),
                        before: "0".into(),
                        after: "0".into(),
                    }],
                    locks: vec![resource.clone()],
                },
                effects: vec![gofin_domain::ProjectedEffect::AuthorityValue {
                    resource,
                    authority: "synthetic-pricing-authority".into(),
                    value: "0".into(),
                }],
                requires: vec![],
                evidence: json!({"synthetic_read_only_price_fixture":true}),
            })
        }
    }

    struct DiagnosticsAdapter {
        health: Arc<Mutex<Value>>,
        reads: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Adapter for DiagnosticsAdapter {
        fn definitions(&self) -> Vec<Definition> {
            vec![]
        }
        fn diagnostics(&self) -> Value {
            self.health.lock().unwrap().clone()
        }
        async fn read(&self, _: &str, _: &Value) -> gofin_domain::Result<Value> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Err(GoFinError::validation(
                "Diagnostics must not probe the network.",
            ))
        }
        async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Err(GoFinError::validation(
                "Diagnostics must not capture financial state.",
            ))
        }
        async fn prepare(&self, _: &str, _: &Value) -> gofin_domain::Result<PreparedOperation> {
            Err(GoFinError::validation("Diagnostic fixture is read only."))
        }
        async fn revalidate(
            &self,
            _: &PreparedOperation,
        ) -> gofin_domain::Result<PreparedOperation> {
            Err(GoFinError::validation("Diagnostic fixture is read only."))
        }
        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> gofin_domain::Result<Submission> {
            Err(GoFinError::validation("Diagnostic fixture is read only."))
        }
        async fn reconcile(&self, _: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
            Err(GoFinError::validation("Diagnostic fixture is read only."))
        }
    }

    fn rpc_diagnostic_fixture() -> Value {
        json!({"chain_id":42161,"read_timeout_ms":null,"endpoints":[
            {"endpoint_id":"rpc-1","identity_verified":true,"status":"cooling_down","retry_after_ms":12000,"last_error_code":"RPC_RATE_LIMITED","unsupported_methods":[]},
            {"endpoint_id":"rpc-2","identity_verified":true,"status":"healthy","retry_after_ms":0,"last_error_code":null,"unsupported_methods":[]}
        ],"last_read":{"method":"eth_call","selected_endpoint":"rpc-2","attempts":[{"endpoint_id":"rpc-1","reason":"rate_limited"},{"endpoint_id":"rpc-2","reason":"ok"}],"outcome":"ok"}})
    }

    #[tokio::test]
    async fn doctor_reports_current_transport_health_without_network_reads_and_deduplicates_snapshots()
     {
        let health = Arc::new(Mutex::new(rpc_diagnostic_fixture()));
        let reads = Arc::new(AtomicUsize::new(0));
        let adapter: Arc<dyn Adapter> = Arc::new(DiagnosticsAdapter {
            health: health.clone(),
            reads: reads.clone(),
        });
        let mut different_pool = rpc_diagnostic_fixture();
        different_pool["last_read"]["outcome"] = json!("error");
        let runtime = compose_runtime(
            binance_only_config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![
                    adapter.clone(),
                    adapter,
                    Arc::new(DiagnosticsAdapter {
                        health: Arc::new(Mutex::new(json!([
                            different_pool.clone(),
                            rpc_diagnostic_fixture(),
                            null
                        ]))),
                        reads: reads.clone(),
                    }),
                ],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
            },
        )
        .unwrap();
        let response = runtime.handle(BrokerRequest::Doctor).await;
        assert!(response.ok);
        let result = response.result.unwrap();
        assert_eq!(
            result["rpc_health"][0].get("read_timeout_ms"),
            Some(&Value::Null)
        );
        assert_eq!(
            result["rpc_health"],
            json!([rpc_diagnostic_fixture(), different_pool])
        );
        health.lock().unwrap()["last_read"]["outcome"] = json!("cancelled");
        let next = runtime.handle(BrokerRequest::Doctor).await.result.unwrap();
        assert_eq!(next["rpc_health"][0]["last_read"]["outcome"], "cancelled");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(runtime.engine.operations().unwrap().is_empty());
    }

    #[test]
    fn rpc_diagnostics_only_publish_reviewed_fields_and_never_provider_credentials() {
        let mut value = rpc_diagnostic_fixture();
        let secret = "API_KEY_SECRET_SENTINEL";
        value["url"] = json!(format!(
            "https://user:password@example.test/{secret}?api_key={secret}"
        ));
        value["headers"] = json!({"Authorization":secret});
        value["endpoints"][0]["url"] = value["url"].clone();
        value["endpoints"][0]["last_error_code"] = json!(secret);
        value["endpoints"][0]["unsupported_methods"] = json!([
            secret,
            "eth_getProof",
            "eth_getBlockByNumber:finalized",
            "eth_call:API_KEY_SECRET_SENTINEL"
        ]);
        value["last_read"]["provider_message"] = json!(secret);
        value["last_read"]["attempts"][0]["reason"] = json!(secret);
        value["last_read"]["attempts"][0]["url"] = value["url"].clone();
        value["last_read"]["attempts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"endpoint_id":secret,"reason":"timeout"}));
        value["last_failover"] = value["last_read"].clone();
        value["last_failover"]["headers"] = json!({"Authorization":secret});
        let public = public_rpc_diagnostics(&value).unwrap();
        let encoded = serde_json::to_string(&public).unwrap();
        for private in [
            secret,
            "https://",
            "password",
            "Authorization",
            "provider_message",
        ] {
            assert!(!encoded.contains(private));
        }
        assert!(public["endpoints"][0]["last_error_code"].is_null());
        assert!(public["last_read"]["attempts"][0]["reason"].is_null());
        assert_eq!(public["last_failover"], public["last_read"]);
        assert_eq!(
            public["endpoints"][0]["unsupported_methods"],
            json!(["eth_getProof", "eth_getBlockByNumber:finalized"])
        );
        assert_eq!(public["last_read"]["attempts"].as_array().unwrap().len(), 2);
        assert!(public_rpc_diagnostics(&Value::Null).is_none());
    }

    #[test]
    fn public_rpc_diagnostics_keep_previous_failover_distinct_from_latest_healthy_read() {
        let mut value = rpc_diagnostic_fixture();
        assert!(
            public_rpc_diagnostics(&value)
                .unwrap()
                .get("last_failover")
                .is_none()
        );
        value["last_failover"] = value["last_read"].clone();
        value["last_failover"]["method"] = json!("eth_getTransactionReceipt");
        value["last_read"] = json!({"method":"eth_getBlockByNumber","selected_endpoint":"rpc-1","attempts":[{"endpoint_id":"rpc-1","reason":"ok"}],"outcome":"ok"});
        let public = public_rpc_diagnostics(&value).unwrap();
        assert_eq!(public["last_read"], value["last_read"]);
        assert_eq!(public["last_failover"], value["last_failover"]);
        value["last_failover"] = Value::Null;
        assert!(public_rpc_diagnostics(&value).unwrap()["last_failover"].is_null());
    }

    struct ReadOnlyAuthorizer;
    #[async_trait]
    impl Authorizer for ReadOnlyAuthorizer {
        async fn authorize(
            &self,
            _: &gofin_domain::AuthorizationChallenge,
        ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
            Err(GoFinError::new(
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
        async fn usd_micros(&self, _: &Amount) -> gofin_domain::Result<(String, String, String)> {
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
        ) -> gofin_domain::Result<(String, String, String)> {
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
                authorities: vec![gofin_domain::AuthorityState {
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
                    connection_metadata: vec![],
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(RecordingPriceValuator {
                    calls: calls.clone(),
                    revision: revision.clone(),
                }),
                bridge: None,
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
        let weth = asset("ethereum:1", gofin_ethereum::WETH, 18);
        let usdc = asset("ethereum:1", gofin_ethereum::USDC, 6);
        config.deployments[0].snapshot_tokens =
            vec![weth.reference.clone(), usdc.reference.clone()];
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
                    connection_metadata: vec![],
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(pricing::tests::fixed_valuator("1.02")),
                bridge: None,
            },
        )
        .unwrap();
        let step = gofin_domain::PlanStep {
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
        let step = gofin_domain::PlanStep {
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

    #[test]
    fn solana_profile_validates_without_invented_evm_networks() {
        let address = "11111111111111111111111111111111";
        let mut profile = config();
        profile.networks.clear();
        profile.deployments.clear();
        profile.connections.push(ConnectionConfig::Solana {
            name: "watch-solana".into(),
            resource: ResourceId(format!("solana:mainnet-beta:{address}")),
            address: address.into(),
            credential_ref: None,
            signing_enabled: false,
            network: "solana:mainnet-beta".into(),
        });
        assert!(profile.validate().is_ok());
        profile.connections.push(ConnectionConfig::Jupiter {
            name: "jupiter".into(),
            resource: ResourceId("jupiter:service".into()),
            credential_ref: "550e8400-e29b-41d4-a716-446655440000".into(),
        });
        assert!(profile.validate().is_ok());
        profile.connections.push(ConnectionConfig::Jupiter {
            name: "jupiter-duplicate".into(),
            resource: ResourceId("jupiter:service:2".into()),
            credential_ref: "550e8400-e29b-41d4-a716-446655440001".into(),
        });
        assert!(profile.validate().is_err());
    }

    #[test]
    fn hyperliquid_scope_migration_keeps_legacy_owner_and_separates_delegated_resources() {
        let owner = "0x1111111111111111111111111111111111111111";
        let target = "0x2222222222222222222222222222222222222222";
        let legacy: ConnectionConfig = serde_json::from_value(json!({
            "provider":"hyperliquid","name":"legacy","resource":format!("hyperliquid:mainnet:{owner}:account:primary"),
            "owner":owner,"credential_ref":null,"signing_enabled":false
        }))
        .unwrap();
        assert!(matches!(
            legacy,
            ConnectionConfig::Hyperliquid {
                execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
                ..
            }
        ));

        let mut config = config();
        config.connections.push(ConnectionConfig::Hyperliquid {
            name: "delegated".into(),
            resource: ResourceId(format!("hyperliquid:mainnet:{target}:account:primary")),
            owner: owner.into(),
            credential_ref: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            signing_enabled: true,
            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Subaccount {
                address: target.into(),
            },
            auto_derived: false,
        });
        assert!(config.validate().is_ok());
    }

    struct UnusedCredentials;
    impl gofin_exchange::kraken::NonceAllocator for UnusedCredentials {
        fn allocate(&self, _: &str, _: u64) -> Result<u64, gofin_exchange::ccxt::CcxtError> {
            panic!("composition must not dispatch authenticated Kraken requests")
        }
    }
    impl gofin_exchange::ccxt::CredentialProvider for UnusedCredentials {
        fn credentials(
            &self,
            _: &str,
        ) -> Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError> {
            panic!("composition and local management must not access credentials")
        }
    }
    impl BrokerCredentialProvider for UnusedCredentials {
        fn kraken_nonces(
            &self,
        ) -> gofin_domain::Result<Arc<dyn gofin_exchange::kraken::NonceAllocator>> {
            Ok(Arc::new(Self))
        }
        fn envelope_signer(
            &self,
            _: &gofin_credentials::CredentialRef,
        ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
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

    #[tokio::test]
    async fn public_connections_group_evm_wallet_while_broker_keeps_all_authorization_identities() {
        let address = "0x1111111111111111111111111111111111111111";
        let mut config = config();
        config.networks = NetworkConfig::reviewed_evm_networks();
        config.deployments = vec![DeploymentConfig::ethereum_mainnet()];
        config.connections = vec![
            ConnectionConfig::Binance {
                name: "main".into(),
                resource: ResourceId("binance:uid:1001:SPOT".into()),
                credential_ref: "550e8400-e29b-41d4-a716-446655440001".into(),
                role: "main".into(),
                parent: None,
                email: None,
                wallet_type: "SPOT".into(),
                physical_identity: "1001".into(),
                physical_identity_verified: true,
            },
            ConnectionConfig::Binance {
                name: "sub".into(),
                resource: ResourceId("binance:uid:1002:SPOT".into()),
                credential_ref: "550e8400-e29b-41d4-a716-446655440002".into(),
                role: "sub".into(),
                parent: Some("main".into()),
                email: Some("sub@example.invalid".into()),
                wallet_type: "SPOT".into(),
                physical_identity: "1002".into(),
                physical_identity_verified: true,
            },
            ConnectionConfig::Evm {
                name: "wallet".into(),
                resource: ResourceId(format!("evm:{address}")),
                address: address.into(),
                credential_ref: None,
                signing_enabled: false,
            },
            ConnectionConfig::Hyperliquid {
                name: "manual-hyperliquid".into(),
                resource: ResourceId(
                    "hyperliquid:mainnet:0x2222222222222222222222222222222222222222:account:primary"
                        .into(),
                ),
                owner: "0x2222222222222222222222222222222222222222".into(),
                credential_ref: None,
                signing_enabled: false,
                execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::Owner,
                auto_derived: false,
            },
        ];
        config = enrollment::effective_protected_config(config).unwrap();
        config.validate().unwrap();
        let applications = build_protected_applications(&config, Arc::new(UnusedCredentials))
            .expect("synthetic production composition should build without credential access");
        let pre_runtime_identities = applications
            .adapters
            .iter()
            .flat_map(|adapter| adapter.authorization_identities())
            .filter(|identity| identity.provider == "hyperliquid")
            .collect::<Vec<_>>();
        let evm_link = pre_runtime_identities
            .iter()
            .find(|identity| {
                identity
                    .resource
                    .0
                    .ends_with(&format!("{address}:account:primary"))
            })
            .expect("derived Hyperliquid authorization identity keeps the EVM wallet alias");
        assert_eq!(evm_link.name, "wallet");
        assert_eq!(
            evm_link.resource.0,
            format!("hyperliquid:mainnet:{address}:account:primary")
        );
        assert!(pre_runtime_identities.iter().any(|identity| {
            identity.name == "manual-hyperliquid"
                && identity
                    .resource
                    .0
                    .contains("0x2222222222222222222222222222222222222222")
        }));
        let runtime = compose_runtime(
            config,
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: applications.adapters,
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: applications.valuator,
                bridge: applications.bridge,
            },
        )
        .unwrap();

        let response = runtime.handle(BrokerRequest::Connections).await;
        assert!(response.ok, "{:?}", response.error);
        let rows = response.result.unwrap().as_array().unwrap().clone();
        assert_eq!(
            rows.len(),
            4,
            "only two Binance accounts, one EVM wallet, and the explicit Hyperliquid row are public"
        );
        let human = gofin_presentation::render_human("connections", &json!(rows), false);
        assert!(human.contains("main") && human.contains("sub") && human.contains("wallet"));
        assert!(human.contains("manual-hyperliquid"));
        assert!(!human.contains("wallet-hyperliquid"));
        assert!(!human.contains("binance-ethereum-destination"));
        assert_eq!(
            rows.iter()
                .filter(|row| row["provider"] == "binance")
                .count(),
            2
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row["provider"] == "hyperliquid")
                .count(),
            1
        );
        assert!(
            rows.iter()
                .all(|row| row["provider"] != "binance-ethereum-destination")
        );
        let wallet = rows
            .iter()
            .find(|row| row["provider"] == "evm")
            .expect("one grouped EVM wallet is public");
        let networks = wallet["details"]["networks"].as_array().unwrap();
        let chain_ids = networks
            .iter()
            .map(|network| network["chain_id"].as_u64().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            chain_ids,
            [1, 42161, 4663, 8453, 10, 137, 43114, 5042, 999]
                .into_iter()
                .collect()
        );
        assert!(wallet["network"].is_null());
        assert!(networks.iter().all(|network| {
            network
                .as_object()
                .unwrap()
                .keys()
                .all(|key| key == "network" || key == "chain_id")
        }));

        let identities = runtime.observer.authorization_identities();
        assert!(identities.len() > rows.len());
        let derived_hyperliquid_identity = identities
            .iter()
            .find(|identity| {
                identity.provider == "hyperliquid"
                    && identity.resource.0
                        == format!("hyperliquid:mainnet:{address}:account:primary")
            })
            .expect("hidden inventory row remains a protected plan identity");
        assert_eq!(derived_hyperliquid_identity.name, "wallet");
        let destination_rows = identities
            .iter()
            .filter(|identity| identity.provider == "binance-ethereum-destination")
            .collect::<Vec<_>>();
        assert!(destination_rows.len() >= 3);
        for destination in &destination_rows {
            assert!(
                destination
                    .address
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(address))
            );
            assert!(destination.details["assets"].is_object());
            assert!(destination.details["chain_id"].as_u64().is_some());
            assert!(destination.details["confirmations"].as_u64().is_some());
        }
        let wallet_identities = identities
            .iter()
            .filter(|identity| identity.provider == "evm")
            .collect::<Vec<_>>();
        assert!(wallet_identities.len() >= 3);
        assert_eq!(
            wallet_identities
                .iter()
                .map(|identity| identity.details["chain_id"].as_u64().unwrap())
                .collect::<BTreeSet<_>>(),
            [1, 42161, 4663, 8453, 10, 137, 43114, 5042, 999]
                .into_iter()
                .collect()
        );
        assert!(wallet_identities.iter().all(|identity| {
            !identity.signing_enabled
                && identity.details["confirmations"].as_u64().is_some()
                && identity
                    .address
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(address))
        }));
    }

    struct FixtureRuntimeLoader {
        database: PathBuf,
        loads: AtomicUsize,
        fail: bool,
    }
    #[async_trait]
    impl RuntimeLoader for FixtureRuntimeLoader {
        async fn load(&self, config: BrokerConfigV2) -> gofin_domain::Result<Arc<BrokerRuntime>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(GoFinError::validation("Synthetic reload failure."));
            }
            let applications = build_protected_applications(&config, Arc::new(UnusedCredentials))?;
            compose_runtime(
                config,
                RuntimeDependencies {
                    store: Store::open(&self.database).unwrap(),
                    adapters: applications.adapters,
                    authorizer: Arc::new(ReadOnlyAuthorizer),
                    valuator: applications.valuator,
                    bridge: applications.bridge,
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
            status: gofin_domain::Status::Submitted,
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
            position_baseline: None,
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
    async fn reviewed_wallet_coverage_reload_preserves_pending_submission_without_credentials() {
        let directory = test_directory();
        let loader = FixtureRuntimeLoader {
            database: directory.join("operations.sqlite"),
            loads: AtomicUsize::new(0),
            fail: false,
        };
        let mut stored = config();
        stored.connections.push(ConnectionConfig::Evm {
            name: "read-only-wallet".into(),
            resource: ResourceId("evm:0x1111111111111111111111111111111111111111".into()),
            credential_ref: None,
            address: "0x1111111111111111111111111111111111111111".into(),
            signing_enabled: false,
        });
        // Persisted rollup rows still have the old single RPC defaults.
        for chain_id in [8453, 42161, 4663] {
            let mut network = NetworkConfig::reviewed_evm_networks()
                .into_iter()
                .find(|network| network.chain_id == chain_id)
                .unwrap();
            network.rpc_urls.truncate(1);
            stored.networks.push(network);
        }
        let old_reviewed_networks = stored.networks.clone();
        let old_reviewed_deployments = vec![DeploymentConfig::ethereum_mainnet()];
        let initial = enrollment::effective_protected_config_with(
            stored.clone(),
            &old_reviewed_networks,
            &old_reviewed_deployments,
        )
        .unwrap();
        let mut runtime = loader.load(initial).await.unwrap();
        assert_eq!(loader.loads.load(Ordering::SeqCst), 1);
        assert!(
            runtime
                .engine
                .definitions()
                .unwrap()
                .iter()
                .all(|definition| definition.name != "ramses.quote")
        );

        let mut store = Store::open(&loader.database).unwrap();
        let record = OperationRecord {
            id: "reviewed-wallet-pending".into(),
            request_id: "reviewed-wallet-request".into(),
            fingerprint: "reviewed-wallet-fingerprint".into(),
            fingerprint_version: 1,
            operation: "synthetic.write".into(),
            input: json!({}),
            status: gofin_domain::Status::Submitted,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: Some(Submission {
                kind: "synthetic".into(),
                reference: "reviewed-wallet-original-submission".into(),
                data: json!({"nonce":7}),
            }),
            dispatch_evidence: None,
            position_baseline: None,
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

        let reviewed = enrollment::effective_protected_config(stored).unwrap();
        for chain_id in [8453, 42161, 4663] {
            let before = runtime
                .configuration()
                .networks
                .iter()
                .find(|network| network.chain_id == chain_id)
                .unwrap();
            assert_eq!(before.rpc_urls.len(), 1);
            let after = reviewed
                .networks
                .iter()
                .find(|network| network.chain_id == chain_id)
                .unwrap();
            assert_eq!(
                after.rpc_urls,
                gofin_networks::by_chain_id(chain_id).unwrap().rpc_urls
            );
            assert_eq!(after.rpc_urls[0], before.rpc_urls[0]);
        }
        assert_eq!(
            reviewed.networks.len(),
            NetworkConfig::reviewed_evm_networks().len()
        );
        let prior = runtime.clone();
        refresh_runtime(&mut runtime, reviewed.clone(), 501, &loader)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&runtime, &prior));
        assert_eq!(loader.loads.load(Ordering::SeqCst), 2);
        let definitions = runtime.engine.definitions().unwrap();
        assert!(
            definitions
                .iter()
                .any(|definition| definition.name == "ramses.quote")
        );
        assert!(
            definitions
                .iter()
                .any(|definition| definition.name == "ramses.swap")
        );
        assert_eq!(runtime.engine.operations().unwrap(), vec![record.clone()]);

        let refreshed = runtime.clone();
        refresh_runtime(&mut runtime, reviewed, 501, &loader)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&runtime, &refreshed));
        assert_eq!(loader.loads.load(Ordering::SeqCst), 2);
        assert_eq!(runtime.engine.operations().unwrap(), vec![record]);
        drop(runtime);
        drop(prior);
        drop(refreshed);
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
                bridge: None,
            },
        )
        .unwrap();
        runtime.require_account_identity_verification();
        for request in [
            BrokerRequest::DaemonStart,
            BrokerRequest::Connections,
            BrokerRequest::Doctor,
        ] {
            assert!(runtime.handle(request).await.ok);
        }
        for request in [BrokerRequest::Execute {
            request: gofin_domain::OperationRequest {
                request_id: "synthetic-blocked".into(),
                operation: "binance.transfer".into(),
                input: json!({}),
                run_id: None,
            },
        }] {
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
                    connection_metadata: vec![],
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
            },
        )
        .unwrap();
        runtime.require_account_identity_verification();
        for request in [
            BrokerRequest::DaemonStart,
            BrokerRequest::Doctor,
            BrokerRequest::Connections,
        ] {
            assert!(runtime.handle(request).await.ok);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let identity_request = BrokerRequest::Execute {
            request: OperationRequest {
                request_id: "synthetic-identity-check".into(),
                operation: "binance.transfer".into(),
                input: json!({}),
                run_id: None,
            },
        };
        assert_eq!(
            runtime
                .verify_account_identities(&identity_request)
                .await
                .unwrap_err()
                .code,
            "VENUE_PERMISSION"
        );
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert!(
            runtime
                .account_identity_check_pending
                .load(Ordering::SeqCst)
        );
        assert!(runtime.handle(BrokerRequest::DaemonStart).await.ok);
        fail.store(false, Ordering::SeqCst);
        runtime
            .verify_account_identities(&identity_request)
            .await
            .unwrap();
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
    async fn selected_identity_checks_cache_only_requested_accounts_and_skip_other_venues() {
        let mut configuration = binance_only_config();
        let mut second = configuration.connections[0].clone();
        if let ConnectionConfig::Binance {
            name,
            resource,
            physical_identity,
            ..
        } = &mut second
        {
            *name = "synthetic-second".into();
            *physical_identity = "202".into();
            *resource = ResourceId("binance:uid:202:SPOT".into());
        }
        configuration.connections.push(second);
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = compose_runtime(
            configuration,
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![Arc::new(ReadAdapter {
                    captures: Arc::new(AtomicUsize::new(0)),
                    pricing_snapshot: None,
                    pricing_preflight: None,
                    identity_probe: Some((Arc::new(AtomicBool::new(false)), calls.clone())),
                    connection_metadata: vec![],
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
            },
        )
        .unwrap();
        runtime.require_account_identity_verification();
        let selected = |account: &str| BrokerRequest::Read {
            operation: "binance.balances".into(),
            input: json!({"account":account}),
        };
        runtime
            .verify_account_identities(&selected("synthetic-master"))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            runtime
                .account_identity_check_pending
                .load(Ordering::SeqCst)
        );
        runtime
            .verify_account_identities(&selected("synthetic-master"))
            .await
            .unwrap();
        runtime
            .verify_account_identities(&BrokerRequest::Read {
                operation: "wallet.balances".into(),
                input: json!({"wallet":"synthetic-wallet"}),
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        runtime
            .verify_account_identities(&selected("synthetic-second"))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            !runtime
                .account_identity_check_pending
                .load(Ordering::SeqCst)
        );
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
                bridge: applications.bridge,
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
    fn solana_only_cex_composition_keeps_exact_custody_and_one_deposit_definition() {
        let address = "So11111111111111111111111111111111111111112";
        let mut config = binance_only_config();
        config.networks.clear();
        config.deployments.clear();
        config.connections.push(ConnectionConfig::Solana {
            name: "synthetic-solana".into(),
            resource: ResourceId(format!("solana:mainnet-beta:{address}")),
            address: address.into(),
            credential_ref: None,
            signing_enabled: false,
            network: "solana:mainnet-beta".into(),
        });
        config.connections.push(ConnectionConfig::Kraken {
            default_wallet_account_id: None,
            name: "synthetic-kraken".into(),
            resource: ResourceId("kraken:account:synthetickraken".into()),
            credential_ref: "550e8400-e29b-41d4-a716-446655440001".into(),
            physical_identity: "synthetickraken".into(),
            physical_identity_verified: true,
        });
        config.validate().unwrap();
        let destinations = configured_binance_destinations(&config).unwrap();
        assert_eq!(destinations.len(), 1);
        assert_eq!(destinations[0].name, "synthetic-solana");
        assert_eq!(destinations[0].address, address);
        assert_eq!(
            destinations[0].resource.0,
            format!("solana:mainnet-beta:{address}")
        );
        assert!(destinations[0].assets.is_empty());
        let applications = build_protected_applications(&config, Arc::new(UnusedCredentials))
            .expect("Solana funding composition must not require an EVM observer");
        let definitions = applications
            .adapters
            .iter()
            .flat_map(|adapter| adapter.definitions())
            .collect::<Vec<_>>();
        for operation in [
            "wallet.transfer",
            "binance.deposit",
            "binance.withdraw",
            "kraken.deposit",
        ] {
            assert_eq!(
                definitions.iter().filter(|d| d.name == operation).count(),
                1,
                "{operation} must have exactly one dispatcher"
            );
        }
    }

    #[test]
    fn binance_payout_destinations_cover_reusable_chains_and_preserve_legacy_scope() {
        let address = "0x1111111111111111111111111111111111111111";
        let mut config = binance_only_config();
        config.networks = NetworkConfig::reviewed_evm_networks();
        assert_eq!(config.networks[0].confirmations, 2);
        assert_eq!(config.networks[1].confirmations, 1);
        assert_eq!(config.networks[2].confirmations, 1);
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
        assert_eq!(destinations.len(), gofin_networks::all().len() + 1);
        let reusable = destinations
            .iter()
            .filter(|d| d.name == "synthetic-reusable")
            .collect::<Vec<_>>();
        assert_eq!(
            reusable.iter().map(|d| d.chain_id).collect::<Vec<_>>(),
            vec![1, 8453, 42161, 4663, 10, 137, 43114, 5042, 999]
        );
        for destination in reusable {
            assert_eq!(
                destination.resource.0,
                format!("ethereum:{}:{address}", destination.chain_id)
            );
            assert_eq!(
                destination.assets[gofin_networks::by_chain_id(destination.chain_id)
                    .unwrap()
                    .native_symbol()]
                .network,
                format!("ethereum:{}", destination.chain_id)
            );
            assert_eq!(
                destination.confirmations,
                if destination.chain_id == 1 { 7 } else { 1 }
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
        assert_eq!(legacy.confirmations, 1);
        assert_eq!(
            legacy.assets.values().cloned().collect::<BTreeSet<_>>(),
            legacy_assets.into_iter().collect()
        );
        assert!(build_protected_applications(&config, Arc::new(UnusedCredentials)).is_ok());
    }

    #[test]
    fn base_profile_has_full_block_policy_wrapper_and_no_dapp_or_asset_whitelist() {
        let mut configured = config();
        configured.networks = NetworkConfig::reviewed_evm_networks();
        configured.validate().unwrap();
        let profile = configured
            .networks
            .iter()
            .find(|n| n.chain_id == 8453)
            .unwrap();
        assert_eq!(profile.id, "ethereum:8453");
        assert_eq!(
            profile.rpc_urls,
            vec!["https://mainnet.base.org", "https://base.drpc.org"]
        );
        assert_eq!(profile.confirmations, 1);
        assert!(profile.snapshot_tokens.is_empty());
        let wallet = gofin_ethereum::EthereumConfig::wallet_only(8453, 9, vec![]);
        assert_eq!(wallet.confirmations, 9);
        assert!(!wallet.require_finalized);
        assert_eq!(
            wallet.weth.as_deref(),
            Some("0x4200000000000000000000000000000000000006")
        );
        assert!(wallet.uniswap.is_none() && wallet.aave.is_none());
        // Existing protected configuration keeps only its approved networks;
        // startup never silently broadens a legacy wallet's scope.
        let legacy = config();
        assert_eq!(legacy.networks.len(), 1);
        assert_eq!(legacy.networks[0].id, "ethereum:1");
        legacy.validate().unwrap();
        configured
            .networks
            .iter_mut()
            .find(|n| n.chain_id == 8453)
            .unwrap()
            .id = "base:8453".into();
        assert_eq!(
            configured.validate().unwrap_err().code,
            "CONFIG_NETWORK_INVALID"
        );
    }

    #[test]
    fn base_reviewed_and_legacy_wallet_profiles_share_uniswap_aave_and_signer() {
        let address = "0x1111111111111111111111111111111111111111";
        for explicit in [false, true] {
            let mut config = config();
            config.networks = NetworkConfig::reviewed_evm_networks()
                .into_iter()
                .filter(|network| network.chain_id == 8453)
                .collect();
            config.deployments = if explicit {
                DeploymentConfig::reviewed_uniswap_networks()
                    .into_iter()
                    .filter(|deployment| deployment.network == "ethereum:8453")
                    .collect()
            } else {
                vec![]
            };
            config.connections = vec![ConnectionConfig::Evm {
                name: "synthetic-base".into(),
                resource: ResourceId(format!("evm:{address}")),
                address: address.into(),
                signing_enabled: false,
                credential_ref: None,
            }];
            config.validate().unwrap();
            let applications =
                build_protected_applications(&config, Arc::new(UnusedCredentials)).unwrap();
            let wallet = applications
                .adapters
                .iter()
                .find(|adapter| {
                    adapter
                        .definitions()
                        .iter()
                        .any(|definition| definition.name == "wallet.balances")
                })
                .unwrap();
            for name in [
                "aave.supply",
                "uniswap.quote",
                "uniswap.swap",
                "wallet.permit2.allowance",
            ] {
                assert!(
                    wallet
                        .definitions()
                        .iter()
                        .any(|definition| definition.name == name),
                    "{name}"
                );
            }
            assert!(
                wallet
                    .connections()
                    .iter()
                    .all(|connection| connection.address.as_deref() == Some(address))
            );
        }
        let deployment = amm_deployments::reviewed_uniswap(8453).unwrap();
        assert_eq!(
            deployment.router,
            "0x2626664c2603336e57b271c5c0b26f421741e481"
        );
        assert_ne!(
            deployment.factory,
            amm_deployments::reviewed_uniswap(1).unwrap().factory
        );
    }

    struct PayoutMetadataRpc {
        chain_id: u64,
        decimals: u8,
        deployed: bool,
        calls: Mutex<Vec<(String, Value)>>,
    }
    #[async_trait]
    impl gofin_evm::EvmRpc for PayoutMetadataRpc {
        async fn request(&self, method: &str, params: Value) -> gofin_domain::Result<Value> {
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
            gofin_ethereum::EthereumAdapter::new(
                gofin_ethereum::EthereumConfig::wallet_only(chain_id, 2, vec![]),
                vec![gofin_ethereum::WalletConnection {
                    name: "synthetic-payout".into(),
                    address: "0x1111111111111111111111111111111111111111".into(),
                    signing_enabled: false,
                }],
                Arc::new(gofin_evm::EvmRuntime::new(rpc, chain_id, None)),
            )
            .unwrap(),
        );
        EthereumPayoutVerifier::new(BTreeMap::from([(chain_id, adapter)])).unwrap()
    }

    #[tokio::test]
    async fn payout_native_asset_identity_cannot_borrow_eth_on_non_eth_networks() {
        use gofin_binance::BinanceRecipientVerifier;
        for (chain_id, coin) in [(137, "POL"), (43114, "AVAX"), (5042, "USDC"), (999, "HYPE")] {
            let rpc = Arc::new(PayoutMetadataRpc {
                chain_id,
                decimals: 18,
                deployed: true,
                calls: Mutex::new(vec![]),
            });
            let verifier = payout_metadata_verifier(chain_id, rpc);
            assert!(
                verifier
                    .resolve_asset(chain_id, "ETH", None)
                    .await
                    .unwrap()
                    .is_none()
            );
            let asset = verifier
                .resolve_asset(chain_id, coin, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(asset.network, format!("ethereum:{chain_id}"));
            assert_eq!(asset.reference, "native");
            assert_eq!(asset.decimals, 18);
        }
    }

    #[tokio::test]
    async fn payout_asset_resolution_verifies_generic_tokens_at_a_pinned_configured_chain() {
        use gofin_binance::BinanceRecipientVerifier;
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
    async fn base_payout_asset_resolution_pins_contract_metadata_and_preserves_network_identity() {
        use gofin_binance::BinanceRecipientVerifier;
        let rpc = Arc::new(PayoutMetadataRpc {
            chain_id: 8453,
            decimals: 7,
            deployed: true,
            calls: Mutex::new(vec![]),
        });
        let verifier = payout_metadata_verifier(8453, rpc.clone());
        assert!(verifier.supports_chain(8453));
        assert!(!verifier.supports_chain(1));
        let contract = "0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa";
        let asset = verifier
            .resolve_asset(8453, "UNLISTED", Some(contract))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            asset,
            AssetId {
                network: "ethereum:8453".into(),
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
        use gofin_binance::BinanceRecipientVerifier;
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
        use gofin_binance::BinanceRecipientVerifier;
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
    impl gofin_exchange::ccxt::CredentialProvider for PricingCredentials {
        fn credentials(
            &self,
            _: &str,
        ) -> Result<gofin_exchange::ccxt::Credentials, gofin_exchange::ccxt::CcxtError> {
            panic!("market-data enrollment must not obtain exchange credentials")
        }
    }
    impl BrokerCredentialProvider for PricingCredentials {
        fn usd_price_source(
            &self,
            credential: &gofin_credentials::CredentialRef,
            plan: &str,
        ) -> gofin_domain::Result<Arc<dyn gofin_pricing::UsdPriceSource>> {
            assert_eq!(credential.as_str(), "550e8400-e29b-41d4-a716-446655440000");
            assert_eq!(plan, "pro");
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(GoFinError::validation(
                    "synthetic unavailable pricing credential",
                ));
            }
            Ok(Arc::new(pricing::tests::FixedUsdSource {
                price: "1.02".into(),
            }))
        }
        fn envelope_signer(
            &self,
            _: &gofin_credentials::CredentialRef,
        ) -> gofin_domain::Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
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
                atoms: gofin_domain::Quantity::from_decimal("1", 18).unwrap(),
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
                    bridge: applications.bridge,
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
                    connection_metadata: vec![],
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
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
        let path = std::env::temp_dir().join(format!(
            "gofin-broker-daemon-{}-{nonce}",
            std::process::id()
        ));
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
            gofin_protocol::daemon::stop_requested_at(&marker, initial.daemon_marker_owner_uid)
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
            !gofin_protocol::daemon::stop_requested_at(&marker, restarted.daemon_marker_owner_uid)
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
                .any(|definition| definition.name == "gofin.snapshot")
        );
        assert!(runtime.observer.snapshot_identities().is_empty());
        assert!(
            runtime
                .observer
                .definitions()
                .iter()
                .any(|definition| definition.name == "gofin.snapshot")
        );
    }

    #[tokio::test]
    async fn production_composition_captures_position_baseline_with_current_operation_row() {
        let prepared = Arc::new(AtomicBool::new(false));
        let snapshots = Arc::new(AtomicUsize::new(0));
        let resource = ResourceId("solana:mainnet-beta:SYNTHETIC".into());
        let adapter = Arc::new(PositionBaselineCaptureAdapter {
            resource,
            prepared: prepared.clone(),
            snapshots: snapshots.clone(),
        });
        let runtime = compose_runtime(
            config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![adapter],
                authorizer: Arc::new(PositionBaselineAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
            },
        )
        .unwrap();

        let record = runtime
            .engine
            .execute(OperationRequest {
                request_id: "synthetic-position-baseline-after-row".into(),
                operation: "synthetic.position.close".into(),
                input: json!({}),
                run_id: None,
            })
            .await
            .unwrap();

        assert!(prepared.load(Ordering::SeqCst));
        assert_eq!(snapshots.load(Ordering::SeqCst), 1);
        assert_eq!(record.status, gofin_domain::Status::Failed);
        assert_eq!(
            record.error.as_ref().map(|error| error.code.as_str()),
            Some("SYNTHETIC_STOP_AFTER_BASELINE")
        );
        assert!(record.position_baseline.is_some());
    }

    #[tokio::test]
    async fn connection_view_deduplicates_only_exact_public_rows_not_shared_names() {
        let connection = |provider: &str, network: Option<&str>, details: Value| {
            gofin_domain::ConnectionMetadata {
                name: "shared-name".into(),
                provider: provider.into(),
                resource: ResourceId("shared:resource".into()),
                role: None,
                parent: None,
                network: network.map(str::to_owned),
                address: Some("0x0000000000000000000000000000000000000001".into()),
                credential_ref: None,
                signing_enabled: true,
                details,
            }
        };
        let account = connection("fixture-account", None, json!({"account":"main"}));
        let ethereum = connection(
            "fixture-wallet",
            Some("ethereum:1"),
            json!({"chain_id":1,"wallet":"shared-name"}),
        );
        let arbitrum = connection(
            "fixture-wallet",
            Some("arbitrum:42161"),
            json!({"chain_id":42161,"wallet":"shared-name"}),
        );
        let route = connection(
            "fixture-route",
            Some("ethereum:1"),
            json!({"route":"configured-destination"}),
        );
        let captures = Arc::new(AtomicUsize::new(0));
        let runtime = compose_runtime(
            config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters: vec![Arc::new(ReadAdapter {
                    captures,
                    pricing_snapshot: None,
                    pricing_preflight: None,
                    identity_probe: None,
                    connection_metadata: vec![
                        account.clone(),
                        account.clone(),
                        ethereum.clone(),
                        arbitrum.clone(),
                        route.clone(),
                    ],
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
            },
        )
        .unwrap();

        assert_eq!(runtime.observer.connections().len(), 5);
        assert_eq!(runtime.observer.authorization_identities().len(), 5);
        let response = runtime.handle(BrokerRequest::Connections).await;
        assert!(response.ok, "{:?}", response.error);
        let rows = response.result.unwrap().as_array().unwrap().clone();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter()
                .filter(|row| row["provider"] == "fixture-account")
                .count(),
            1
        );
        assert!(
            rows.iter().any(|row| {
                row["provider"] == "fixture-wallet" && row["network"] == "ethereum:1"
            })
        );
        assert!(rows.iter().any(|row| {
            row["provider"] == "fixture-wallet" && row["network"] == "arbitrum:42161"
        }));
        assert!(rows.iter().any(|row| row["provider"] == "fixture-route"));
    }

    #[test]
    fn solana_wallet_is_the_single_public_connection_for_its_jupiter_adapters() {
        let connection = |name: &str, provider: &str, role: &str, address: &str| {
            gofin_domain::ConnectionMetadata {
                name: name.into(),
                provider: provider.into(),
                resource: ResourceId(format!("solana:mainnet-beta:{address}")),
                role: Some(role.into()),
                parent: None,
                network: Some("solana:mainnet-beta".into()),
                address: Some(address.into()),
                credential_ref: None,
                signing_enabled: true,
                details: json!({"operation":role}),
            }
        };
        let wallet = connection("solana-dev", "solana", "wallet", "wallet-address");
        let rows = deduplicate_connections(vec![
            connection("solana-dev", "jupiter", "swap", "wallet-address"),
            connection("solana-dev", "jupiter", "perps", "wallet-address"),
            connection("solana-dev", "jupiter", "jlp", "wallet-address"),
            connection(
                "solana-dev",
                "jupiter-trigger",
                "limit_orders",
                "wallet-address",
            ),
            wallet.clone(),
            connection("other-wallet", "jupiter", "swap", "other-address"),
        ]);
        assert_eq!(
            rows,
            vec![
                wallet,
                connection("other-wallet", "jupiter", "swap", "other-address")
            ]
        );
    }

    #[tokio::test]
    async fn generic_gofin_snapshot_plan_read_does_not_count_its_own_journal_record() {
        struct PlanAuthorizer;
        #[async_trait]
        impl Authorizer for PlanAuthorizer {
            async fn authorize(
                &self,
                challenge: &gofin_domain::AuthorizationChallenge,
            ) -> gofin_domain::Result<gofin_domain::ApprovalAudit> {
                assert_eq!(challenge.purpose, "financial_plan");
                Ok(gofin_domain::ApprovalAudit {
                    mechanism: "synthetic-plan-read-approval".into(),
                    binding_hash: challenge.binding_hash.clone(),
                    challenge_nonce: challenge.nonce.clone(),
                    approved_at: now(),
                    expires_at: challenge.expires_at.clone(),
                })
            }
        }

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
                    connection_metadata: vec![],
                })],
                authorizer: Arc::new(PlanAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
            },
        )
        .unwrap();
        let mut request = gofin_domain::financial_plans::FinancialPlanAuthorizationRequest {
            request_id: "synthetic-snapshot-plan-approval".into(),
            plan: gofin_domain::financial_plans::FinancialPlanSpec {
                version: 1,
                name: "Synthetic snapshot read".into(),
                summary: None,
                steps: vec![gofin_domain::financial_plans::FinancialPlanStepSpec {
                    action_deadline_seconds: None,
                    step_id: "snapshot".into(),
                    operation: "gofin.snapshot".into(),
                    input: json!({}),
                    permitted_variations: json!({
                        "mode":"exact",
                        "maximum_debits":[],
                        "maximum_costs":[],
                        "possible_credits":[]
                    }),
                    completion: gofin_domain::financial_plans::FinancialStepCompletion {
                        operation_succeeded: true,
                        principal_spend: None,
                        minimum_net_receipt: None,
                    },
                    funding_dependencies: vec![],
                }],
                aggregate_bounds: gofin_domain::financial_plans::FinancialPlanBounds {
                    maximum_debits: vec![],
                    maximum_costs: vec![],
                },
            },
            valid_for_seconds: Some(600),
            replaces_revision_id: None,
            expected_review_hash: None,
        };
        let review = runtime
            .engine
            .preview_financial_plan(request.clone())
            .await
            .unwrap();
        request.expected_review_hash = Some(review.review_hash);
        let authorized = runtime
            .engine
            .authorize_financial_plan(request)
            .await
            .unwrap();
        let attempt = runtime
            .engine
            .execute_financial_plan_step(
                &authorized.plan_id,
                "snapshot",
                OperationRequest {
                    request_id: "synthetic-snapshot-plan-attempt".into(),
                    operation: "gofin.snapshot".into(),
                    input: json!({}),
                    run_id: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(attempt.status, gofin_domain::Status::Completed);
        let snapshot = attempt
            .last_reconciliation_evidence
            .as_ref()
            .and_then(|evidence| evidence.get("read_result"))
            .expect("completed plan read returns its snapshot result");
        assert_eq!(snapshot["complete"], true);
        assert_eq!(snapshot["pending"], json!([]));
        assert_eq!(snapshot["state"]["in_flight"], json!([]));
        assert_eq!(captures.load(Ordering::SeqCst), 1);
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
            planned_operations: vec![gofin_domain::PlanStep {
                operation: "fixture.write".into(),
                input: json!({}),
                maximum_fee_usd_micros: "0".into(),
                description: "synthetic route step".into(),
            }],
            borrowed_proceeds_reserve_usd_micros: "0".into(),
            return_cost_reserve_usd_micros: "0".into(),
            unwind: vec![gofin_domain::PlanStep {
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

    struct DelayedObserver {
        name: &'static str,
        delay: Duration,
        fail: bool,
        started: Arc<tokio::sync::Notify>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Adapter for DelayedObserver {
        fn definitions(&self) -> Vec<Definition> {
            vec![Definition {
                name: self.name.into(),
                description: "Synthetic delayed observer".into(),
                kind: OperationKind::Read,
                execution_actor: ExecutionActor::AgentReadable,
                input_schema: json!({"type":"object","additionalProperties":false}),
                output_schema: json!({"type":"object"}),
                error_schema: json!({"$ref":"GoFinError"}),
                examples: vec![],
                coverage: self.name.into(),
                completion: "Read only".into(),
                semantics: json!({}),
            }]
        }
        fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            vec![gofin_domain::ConnectionMetadata {
                name: self.name.into(),
                provider: if self.name.starts_with("binance:") {
                    "binance"
                } else {
                    "synthetic"
                }
                .into(),
                resource: ResourceId(self.name.into()),
                role: None,
                parent: None,
                network: Some(
                    if self.name.starts_with("binance:") {
                        "binance"
                    } else {
                        self.name
                    }
                    .into(),
                ),
                address: None,
                credential_ref: None,
                signing_enabled: false,
                details: json!({}),
            }]
        }
        fn snapshot_identities(&self) -> Vec<gofin_domain::ConnectionMetadata> {
            if self.name.starts_with("global.") || self.name.starts_with("sidecar.") {
                vec![]
            } else {
                self.connections()
            }
        }
        fn snapshot_is_global(&self) -> bool {
            self.name.starts_with("global.")
        }
        async fn snapshot_scoped(
            &self,
            scope: &SnapshotScope,
        ) -> gofin_domain::Result<StateSnapshot> {
            assert!(
                scope.is_full()
                    || self.snapshot_is_global()
                    || snapshot_connection_matches(scope, &self.connections()[0])
            );
            self.snapshot().await
        }
        async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
            self.read(self.name, &json!({})).await?;
            Ok(StateSnapshot {
                state: FinancialState::default(),
                observed_at: now(),
                complete: true,
                coverage: if self.name.starts_with("binance:") {
                    vec![self.name.into(), format!("{}:USD_M", self.name)]
                } else {
                    vec![self.name.into()]
                },
                pending: vec![],
            })
        }
        async fn read(&self, _: &str, _: &Value) -> gofin_domain::Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            tokio::time::sleep(self.delay).await;
            if self.fail {
                return Err(error(
                    "SYNTHETIC_RPC_FAILED",
                    "read",
                    "Synthetic RPC failed.",
                    "Retry the synthetic observer.",
                ));
            }
            Ok(json!({"synthetic":true}))
        }
        async fn prepare(&self, _: &str, _: &Value) -> gofin_domain::Result<PreparedOperation> {
            Err(GoFinError::validation("Read only"))
        }
        async fn revalidate(
            &self,
            _: &PreparedOperation,
        ) -> gofin_domain::Result<PreparedOperation> {
            Err(GoFinError::validation("Read only"))
        }
        async fn submit(
            &self,
            _: &PreparedOperation,
            _: Arc<dyn SubmissionJournal>,
        ) -> gofin_domain::Result<Submission> {
            panic!("Read observer cannot submit")
        }
        async fn reconcile(&self, _: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
            Err(GoFinError::validation("Read only"))
        }
    }

    fn delayed_runtime(adapters: Vec<Arc<dyn Adapter>>) -> Arc<BrokerRuntime> {
        compose_runtime(
            config(),
            RuntimeDependencies {
                store: Store::in_memory().unwrap(),
                adapters,
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
                bridge: None,
            },
        )
        .unwrap()
    }

    async fn socket_request(path: &Path, request: BrokerRequest) -> BrokerResponse {
        let mut client = UnixStream::connect(path).await.unwrap();
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        client.write_all(&bytes).await.unwrap();
        let mut response = vec![];
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut response)
            .await
            .unwrap();
        serde_json::from_slice(&response).unwrap()
    }

    #[tokio::test]
    async fn listener_recovers_orphans_but_parallel_composition_cannot_cancel_active_preparation() {
        struct PausedPreparation {
            started: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        #[async_trait]
        impl Adapter for PausedPreparation {
            fn definitions(&self) -> Vec<Definition> {
                vec![Definition {
                    name: "synthetic.write".into(),
                    description: "Isolated paused preparation".into(),
                    kind: OperationKind::Write,
                    execution_actor: ExecutionActor::AgentInvocable,
                    input_schema: json!({"type":"object"}),
                    output_schema: json!({}),
                    error_schema: json!({}),
                    examples: vec![],
                    coverage: "Synthetic only".into(),
                    completion: "Never submits".into(),
                    semantics: json!({}),
                }]
            }
            async fn read(&self, _: &str, _: &Value) -> gofin_domain::Result<Value> {
                unreachable!()
            }
            async fn prepare(&self, _: &str, _: &Value) -> gofin_domain::Result<PreparedOperation> {
                self.started.notify_one();
                self.release.notified().await;
                Err(GoFinError::new(
                    "SYNTHETIC_PREPARATION_STOP",
                    "preflight",
                    "Test finished preparation.",
                    "No submission.",
                ))
            }
            async fn revalidate(
                &self,
                _: &PreparedOperation,
            ) -> gofin_domain::Result<PreparedOperation> {
                unreachable!()
            }
            async fn submit(
                &self,
                _: &PreparedOperation,
                _: Arc<dyn SubmissionJournal>,
            ) -> gofin_domain::Result<Submission> {
                panic!("No fixture submission")
            }
            async fn reconcile(&self, _: &OperationRecord) -> gofin_domain::Result<Reconciliation> {
                unreachable!()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("journal.sqlite");
        let mut store = Store::open(&database).unwrap();
        let orphan = json!({"id":"orphan","request_id":"orphan-request","fingerprint":"orphan-fp",
            "fingerprint_version":1,"operation":"synthetic.write","input":{},
            "status":"awaiting_approval","created_at":now(),"updated_at":now()});
        store
            .create_request(
                "orphan",
                "orphan-request",
                "orphan-fp",
                "synthetic.write",
                None,
                "awaiting_approval",
                &orphan,
                &now(),
            )
            .unwrap();
        drop(store);
        let adapter = Arc::new(PausedPreparation {
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        let compose = || {
            compose_runtime(
                config(),
                RuntimeDependencies {
                    store: Store::open(&database).unwrap(),
                    adapters: vec![adapter.clone()],
                    authorizer: Arc::new(ReadOnlyAuthorizer),
                    valuator: Arc::new(FixtureValuator),
                    bridge: None,
                },
            )
            .unwrap()
        };
        let runtime = compose();
        assert_eq!(
            runtime.engine.status("orphan", false).await.unwrap().status,
            gofin_domain::Status::AwaitingApproval
        );
        let socket = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let serving = tokio::spawn(serve_listener(listener, runtime.clone(), unsafe {
            libc::geteuid()
        }));
        assert!(socket_request(&socket, BrokerRequest::Operations).await.ok);
        assert_eq!(
            runtime.engine.status("orphan", false).await.unwrap().status,
            gofin_domain::Status::Cancelled
        );
        let execute_socket = socket.clone();
        let execute = tokio::spawn(async move {
            socket_request(
                &execute_socket,
                BrokerRequest::Execute {
                    request: OperationRequest {
                        request_id: "active-request".into(),
                        operation: "synthetic.write".into(),
                        input: json!({}),
                        run_id: None,
                    },
                },
            )
            .await
        });
        adapter.started.notified().await;
        let active = runtime
            .engine
            .operations()
            .unwrap()
            .into_iter()
            .find(|row| row.request_id == "active-request")
            .unwrap();
        assert_eq!(active.status, gofin_domain::Status::AwaitingApproval);
        let other_runtime = compose();
        assert_eq!(
            other_runtime
                .engine
                .status(&active.id, false)
                .await
                .unwrap(),
            active
        );
        drop(other_runtime);
        adapter.release.notify_one();
        let result = execute.await.unwrap();
        assert!(result.ok, "{:?}", result.error);
        let result = result.result.unwrap();
        assert_eq!(result["error"]["code"], "SYNTHETIC_PREPARATION_STOP");
        assert!(result["submission"].is_null());
        assert!(socket_request(&socket, BrokerRequest::DaemonStop).await.ok);
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn latency_slow_snapshot_and_unrelated_read() {
        let path = PathBuf::from(format!("/tmp/gofin-latency-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&path).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let runtime = delayed_runtime(vec![Arc::new(DelayedObserver {
            name: "slow.rpc",
            delay: Duration::from_millis(180),
            fail: true,
            started: started.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        })]);
        let serving = tokio::spawn(serve_listener(listener, runtime, unsafe {
            libc::geteuid()
        }));
        let slow_path = path.clone();
        let slow =
            tokio::spawn(async move { socket_request(&slow_path, BrokerRequest::Snapshot).await });
        started.notified().await;
        let fast_started = std::time::Instant::now();
        let fast = socket_request(&path, BrokerRequest::Capabilities).await;
        let elapsed = fast_started.elapsed();
        println!(
            "unrelated read behind 180ms failing observer: {:?}",
            elapsed
        );
        assert!(fast.ok);
        let snapshot = slow.await.unwrap().result.unwrap();
        assert_eq!(snapshot["complete"], false);
        assert!(socket_request(&path, BrokerRequest::DaemonStop).await.ok);
        serving.await.unwrap().unwrap();
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn latency_independent_snapshot_observers_sequential_control_and_concurrent() {
        let runtime = delayed_runtime(
            ["first.rpc", "second.rpc"]
                .into_iter()
                .map(|name| {
                    Arc::new(DelayedObserver {
                        name,
                        delay: Duration::from_millis(90),
                        fail: false,
                        started: Arc::new(tokio::sync::Notify::new()),
                        calls: Arc::new(AtomicUsize::new(0)),
                    }) as Arc<dyn Adapter>
                })
                .collect(),
        );
        let before_start = std::time::Instant::now();
        for adapter in &runtime.adapters {
            adapter.snapshot().await.unwrap();
        }
        let sequential = before_start.elapsed();
        let after_start = std::time::Instant::now();
        assert!(runtime.snapshot().await.unwrap().complete);
        let concurrent = after_start.elapsed();
        println!(
            "independent observers sequential control: {sequential:?}; concurrent: {concurrent:?}"
        );
        assert!(concurrent < sequential.saturating_sub(Duration::from_millis(40)));
    }

    #[tokio::test]
    async fn snapshot_observers_run_concurrently_and_report_errors_with_named_partial_coverage() {
        let adapters = ["fast.rpc", "slow.rpc"]
            .into_iter()
            .map(|name| {
                Arc::new(DelayedObserver {
                    name,
                    delay: Duration::from_millis(if name == "fast.rpc" { 10 } else { 180 }),
                    fail: name == "slow.rpc",
                    started: Arc::new(tokio::sync::Notify::new()),
                    calls: Arc::new(AtomicUsize::new(0)),
                }) as Arc<dyn Adapter>
            })
            .collect();
        let runtime = delayed_runtime(adapters);
        let snapshot = runtime
            .observer
            .observe(&SnapshotScope::default())
            .await
            .unwrap();
        assert!(!snapshot.complete);
        assert!(snapshot.coverage.contains(&"fast.rpc".into()));
        assert!(
            snapshot
                .pending
                .iter()
                .any(|pending| pending.contains("slow.rpc@slow.rpc:SYNTHETIC_RPC_FAILED"))
        );
        assert!(!snapshot.coverage.contains(&"slow.rpc".into()));
        // Machine partial results retain the normal stable response shape.
        let json = serde_json::to_value(BrokerResponse::success(
            serde_json::to_value(snapshot).unwrap(),
        ))
        .unwrap();
        assert_eq!(json["result"]["complete"], false);
        assert!(json["result"]["pending"].is_array());
    }

    #[tokio::test]
    async fn scoped_snapshot_avoids_unrelated_rpc_and_fails_closed_for_missing_selectors() {
        let slow_calls = Arc::new(AtomicUsize::new(0));
        let fast_calls = Arc::new(AtomicUsize::new(0));
        let runtime = delayed_runtime(vec![
            Arc::new(DelayedObserver {
                name: "fast.rpc",
                delay: Duration::ZERO,
                fail: false,
                started: Arc::new(tokio::sync::Notify::new()),
                calls: fast_calls.clone(),
            }),
            Arc::new(DelayedObserver {
                name: "slow.rpc",
                delay: Duration::from_millis(180),
                fail: true,
                started: Arc::new(tokio::sync::Notify::new()),
                calls: slow_calls.clone(),
            }),
        ]);
        let request = |resources: Value| BrokerRequest::Read {
            operation: "gofin.snapshot".into(),
            input: json!({"resources":resources}),
        };
        let good = runtime
            .handle(request(json!(["fast.rpc"])))
            .await
            .result
            .unwrap();
        assert_eq!(good["complete"], true);
        assert_eq!(slow_calls.load(Ordering::SeqCst), 0);
        let mixed = runtime
            .handle(request(json!(["fast.rpc", "missing.rpc"])))
            .await
            .result
            .unwrap();
        assert_eq!(mixed["complete"], false);
        assert!(
            mixed["pending"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str().unwrap().contains("missing.rpc"))
        );
        let mixed_networks = runtime
            .handle(BrokerRequest::Read {
                operation: "gofin.snapshot".into(),
                input: json!({"networks":["fast.rpc","missing.rpc"]}),
            })
            .await
            .result
            .unwrap();
        assert_eq!(mixed_networks["complete"], false);
        assert_eq!(slow_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fast_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn grouped_evm_snapshot_selects_only_matching_protected_address_and_enabled_network() {
        let address = "0x1111111111111111111111111111111111111111";
        let connection = gofin_domain::ConnectionMetadata {
            name: "synthetic-wallet".into(),
            provider: "evm".into(),
            resource: ResourceId(format!("ethereum:42161:{address}")),
            role: None,
            parent: None,
            network: Some("ethereum:42161".into()),
            address: Some(address.into()),
            credential_ref: None,
            signing_enabled: false,
            details: json!({}),
        };
        let mut scope = SnapshotScope {
            resources: vec![ResourceId(format!("evm:{address}"))],
            networks: vec!["ethereum:42161".into()],
            assets: vec![],
        };
        assert!(snapshot_connection_matches(&scope, &connection));
        scope.networks = vec!["ethereum:1".into()];
        assert!(!snapshot_connection_matches(&scope, &connection));
        scope.networks.clear();
        scope.resources = vec![ResourceId(
            "evm:0x2222222222222222222222222222222222222222".into(),
        )];
        assert!(!snapshot_connection_matches(&scope, &connection));
        scope.resources = vec![ResourceId(format!("evm:{address}"))];
        scope.assets = vec![AssetId {
            network: "ethereum:1".into(),
            reference: "native".into(),
            decimals: 18,
        }];
        assert!(!snapshot_connection_matches(&scope, &connection));
        // A grouped address without an enabled physical network cannot be used
        // as financial observer coverage merely because its label matches.
        let mut no_network = connection;
        no_network.network = None;
        scope.assets.clear();
        assert!(!snapshot_connection_matches(&scope, &no_network));
    }

    #[tokio::test]
    async fn futures_resource_scope_selects_its_binance_parent_observer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = delayed_runtime(vec![Arc::new(DelayedObserver {
            name: "binance:uid:synthetic:SPOT",
            delay: Duration::ZERO,
            fail: false,
            started: Arc::new(tokio::sync::Notify::new()),
            calls: calls.clone(),
        })]);
        let snapshot = runtime.handle(BrokerRequest::Read {
            operation: "gofin.snapshot".into(),
            input: json!({"resources":["binance:uid:synthetic:SPOT:USD_M"],"networks":["binance"]}),
        }).await.result.unwrap();
        assert_eq!(snapshot["complete"], true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            snapshot["coverage"]
                .as_array()
                .unwrap()
                .contains(&json!("binance:uid:synthetic:SPOT:USD_M"))
        );
    }

    #[tokio::test]
    async fn snapshots_include_global_evidence_and_scope_skips_unrelated_routing_sidecars() {
        let global_calls = Arc::new(AtomicUsize::new(0));
        let sidecar_calls = Arc::new(AtomicUsize::new(0));
        let runtime = delayed_runtime(vec![
            Arc::new(DelayedObserver {
                name: "fast.rpc",
                delay: Duration::ZERO,
                fail: false,
                started: Arc::new(tokio::sync::Notify::new()),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(DelayedObserver {
                name: "global.pending",
                delay: Duration::ZERO,
                fail: false,
                started: Arc::new(tokio::sync::Notify::new()),
                calls: global_calls.clone(),
            }),
            Arc::new(DelayedObserver {
                name: "sidecar.routing",
                delay: Duration::ZERO,
                fail: false,
                started: Arc::new(tokio::sync::Notify::new()),
                calls: sidecar_calls.clone(),
            }),
        ]);
        // Routing sidecars still contribute protected authorization bindings;
        // absence of snapshot identities excludes them only from scoped reads.
        assert!(!runtime.adapters[2].authorization_identities().is_empty());
        assert!(runtime.adapters[2].snapshot_identities().is_empty());
        let scoped = runtime
            .handle(BrokerRequest::Read {
                operation: "gofin.snapshot".into(),
                input: json!({"resources":["fast.rpc"]}),
            })
            .await
            .result
            .unwrap();
        assert_eq!(scoped["complete"], true);
        assert!(
            scoped["coverage"]
                .as_array()
                .unwrap()
                .contains(&json!("global.pending"))
        );
        assert_eq!(sidecar_calls.load(Ordering::SeqCst), 0);
        let full = runtime.snapshot().await.unwrap();
        assert!(full.complete);
        assert!(full.coverage.contains(&"global.pending".into()));
        assert_eq!(global_calls.load(Ordering::SeqCst), 2);
        assert_eq!(sidecar_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn metadata_less_failed_observer_keeps_full_snapshot_incomplete() {
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = delayed_runtime(vec![Arc::new(DelayedObserver {
            name: "sidecar.failed",
            delay: Duration::ZERO,
            fail: true,
            started: Arc::new(tokio::sync::Notify::new()),
            calls: calls.clone(),
        })]);
        assert!(runtime.adapters[0].snapshot_identities().is_empty());
        let full = runtime.snapshot().await.unwrap();
        assert!(!full.complete);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            full.pending
                .iter()
                .any(|pending| pending.contains("sidecar.failed:SYNTHETIC_RPC_FAILED"))
        );
    }

    #[tokio::test]
    async fn cancelling_snapshot_aborts_active_observers_and_queued_slots() {
        struct ActiveObserver(Arc<AtomicUsize>);
        impl Drop for ActiveObserver {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        struct TrackedObserver {
            inner: DelayedObserver,
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Adapter for TrackedObserver {
            fn definitions(&self) -> Vec<Definition> {
                self.inner.definitions()
            }
            fn connections(&self) -> Vec<gofin_domain::ConnectionMetadata> {
                self.inner.connections()
            }
            async fn snapshot(&self) -> gofin_domain::Result<StateSnapshot> {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                let _active = ActiveObserver(self.active.clone());
                self.inner.snapshot().await
            }
            async fn read(&self, operation: &str, input: &Value) -> gofin_domain::Result<Value> {
                self.inner.read(operation, input).await
            }
            async fn prepare(
                &self,
                operation: &str,
                input: &Value,
            ) -> gofin_domain::Result<PreparedOperation> {
                self.inner.prepare(operation, input).await
            }
            async fn revalidate(
                &self,
                prepared: &PreparedOperation,
            ) -> gofin_domain::Result<PreparedOperation> {
                self.inner.revalidate(prepared).await
            }
            async fn submit(
                &self,
                prepared: &PreparedOperation,
                journal: Arc<dyn SubmissionJournal>,
            ) -> gofin_domain::Result<Submission> {
                self.inner.submit(prepared, journal).await
            }
            async fn reconcile(
                &self,
                record: &OperationRecord,
            ) -> gofin_domain::Result<Reconciliation> {
                self.inner.reconcile(record).await
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let runtime = delayed_runtime(
            [
                "slow0.rpc",
                "slow1.rpc",
                "slow2.rpc",
                "slow3.rpc",
                "slow4.rpc",
                "slow5.rpc",
                "slow6.rpc",
                "slow7.rpc",
            ]
            .into_iter()
            .map(|name| {
                Arc::new(TrackedObserver {
                    inner: DelayedObserver {
                        name,
                        delay: Duration::from_secs(1),
                        fail: false,
                        started: Arc::new(tokio::sync::Notify::new()),
                        calls: calls.clone(),
                    },
                    active: active.clone(),
                    peak: peak.clone(),
                }) as Arc<dyn Adapter>
            })
            .collect(),
        );
        // Only the caller cancels this read. The four waiting adapters must
        // never start after cancellation, and all active futures must drop.
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                runtime.observer.observe(&SnapshotScope::default()),
            )
            .await
            .is_err()
        );
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(peak.load(Ordering::SeqCst), MAX_READ_OBSERVERS);
        assert_eq!(calls.load(Ordering::SeqCst), MAX_READ_OBSERVERS);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reconciliation_and_quote_reads_do_not_join_the_read_concurrency_lane() {
        for operation in ["bridge.quotes", "bridge.status", "bridge.recovery"] {
            assert!(!concurrent_read_request(&BrokerRequest::Read {
                operation: operation.into(),
                input: json!({})
            }));
        }
        assert!(!concurrent_read_request(&BrokerRequest::PlanStatus {
            plan_id: "synthetic".into()
        }));
        assert!(!concurrent_read_request(&BrokerRequest::RevokeBoundary {
            id: "synthetic".into()
        }));
        assert!(concurrent_read_request(&BrokerRequest::Read {
            operation: "wallet.balances".into(),
            input: json!({})
        }));
    }

    #[tokio::test]
    async fn status_and_revocation_bypass_queued_work_while_stop_still_drains() {
        let runtime = runtime();
        let held_dispatch = runtime.mutation_gate.lock().await;
        let mutators = [
            BrokerRequest::RevokeBoundary {
                id: "synthetic-missing".into(),
            },
            BrokerRequest::DaemonStop,
        ];
        let mut tasks = Vec::new();
        for request in mutators {
            let runtime = runtime.clone();
            tasks.push(tokio::spawn(async move { runtime.handle(request).await }));
            tokio::task::yield_now().await;
        }
        assert!(runtime.handle(BrokerRequest::Capabilities).await.ok);
        let revoke = tokio::time::timeout(Duration::from_secs(1), tasks.remove(0))
            .await
            .expect("revocation must bypass unrelated queued work")
            .unwrap();
        assert!(!revoke.ok); // Missing grant is reported promptly.
        assert!(!tasks[0].is_finished()); // Service stop still drains mutations.
        let status = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.handle(BrokerRequest::PlanStatus {
                plan_id: "missing".into(),
            }),
        )
        .await
        .expect("stored status must remain responsive");
        assert!(!status.ok);
        drop(held_dispatch);
        assert!(tasks.remove(0).await.unwrap().ok);
    }

    #[tokio::test]
    async fn active_read_pins_runtime_revision_and_daemon_stop_drains_it() {
        let path = PathBuf::from(format!("/tmp/gofin-drain-{}.sock", uuid::Uuid::new_v4()));
        let started = Arc::new(tokio::sync::Notify::new());
        let runtime = delayed_runtime(vec![Arc::new(DelayedObserver {
            name: "slow.rpc",
            delay: Duration::from_millis(100),
            fail: false,
            started: started.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        })]);
        let listener = UnixListener::bind(&path).unwrap();
        let serving = tokio::spawn(serve_listener(listener, runtime, unsafe {
            libc::geteuid()
        }));
        let read_path = path.clone();
        let read =
            tokio::spawn(async move { socket_request(&read_path, BrokerRequest::Snapshot).await });
        started.notified().await;
        assert!(socket_request(&path, BrokerRequest::DaemonStop).await.ok);
        assert!(
            !serving.is_finished(),
            "stop must drain the active observer"
        );
        assert!(read.await.unwrap().ok);
        serving.await.unwrap().unwrap();
        fs::remove_file(path).unwrap();

        let started = Arc::new(tokio::sync::Notify::new());
        let shared = Arc::new(RwLock::new(delayed_runtime(vec![Arc::new(
            DelayedObserver {
                name: "slow.rpc",
                delay: Duration::from_millis(70),
                fail: false,
                started: started.clone(),
                calls: Arc::new(AtomicUsize::new(0)),
            },
        )])));
        let (mut client, server) = UnixStream::pair().unwrap();
        let shared_dispatch = shared.clone();
        let dispatch = tokio::spawn(async move {
            serve_connection_with_timeouts(
                server,
                &shared_dispatch,
                unsafe { libc::geteuid() },
                IpcTimeouts::PRODUCTION,
            )
            .await
            .unwrap();
        });
        client
            .write_all(b"{\"method\":\"snapshot\"}\n")
            .await
            .unwrap();
        started.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), shared.write())
                .await
                .is_err(),
            "configuration reload must wait for the active runtime revision"
        );
        let mut bytes = vec![];
        tokio::io::AsyncReadExt::read_to_end(&mut client, &mut bytes)
            .await
            .unwrap();
        dispatch.await.unwrap();
        *shared.write().await = delayed_runtime(vec![]);
    }

    #[tokio::test]
    async fn idle_slow_and_oversized_peers_cannot_block_the_next_request() {
        let path =
            std::path::PathBuf::from(format!("/tmp/gofin-broker-{}.sock", uuid::Uuid::new_v4()));
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
            .write_all(&vec![b'a'; gofin_protocol::MAX_FRAME_BYTES + 1])
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
        config.networks[0].rpc_urls = vec!["http://127.0.0.1:8545".into()];
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
    fn standalone_subaccount_config_needs_no_master_connection() {
        let mut standalone = config();
        standalone.connections.push(ConnectionConfig::Binance {
            name: "synthetic-child".into(),
            resource: ResourceId("binance:uid:314159:SPOT".into()),
            credential_ref: "550e8400-e29b-41d4-a716-446655440000".into(),
            role: "sub".into(),
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
            physical_identity: "314159".into(),
            physical_identity_verified: true,
        });
        standalone.validate().unwrap();

        if let ConnectionConfig::Binance { email, .. } = &mut standalone.connections[0] {
            *email = Some("synthetic@example.invalid".into());
        }
        assert_eq!(
            standalone.validate().unwrap_err().code,
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
        let no_preview = gofin_domain::OperationRecord {
            id: "synthetic-pending".into(),
            request_id: "synthetic-request".into(),
            fingerprint: "synthetic-fingerprint".into(),
            fingerprint_version: 1,
            operation: "synthetic.write".into(),
            input: json!({}),
            status: gofin_domain::Status::Submitted,
            created_at: now(),
            updated_at: now(),
            preview: None,
            approval: None,
            submission: None,
            dispatch_evidence: None,
            position_baseline: None,
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

    #[test]
    fn pending_hyperliquid_owner_guard_covers_all_write_families_and_delegated_scopes() {
        let owner = "0x1111111111111111111111111111111111111111";
        for (index, operation) in [
            "hyperliquid.order",
            "hyperliquid.account.invalidate_nonce",
            "hyperliquid.margin",
            "hyperliquid.account.deposit",
        ]
        .into_iter()
        .enumerate()
        {
            let record = gofin_domain::OperationRecord {
                id: format!("pending-{index}"),
                request_id: format!("request-{index}"),
                fingerprint: format!("fingerprint-{index}"),
                fingerprint_version: 1,
                operation: operation.into(),
                input: json!({}),
                status: gofin_domain::Status::Submitted,
                created_at: now(),
                updated_at: now(),
                preview: Some(gofin_domain::PreparedOperation {
                    operation: operation.into(),
                    input: json!({}),
                    binding: if operation == "hyperliquid.account.deposit" {
                        json!({"owner":owner,"account":"owner-account"})
                    } else {
                        json!({"account":{"owner":owner,"execution_scope":{"type":"subaccount","address":"0x2222222222222222222222222222222222222222"}}})
                    },
                    summary: "synthetic pending action".into(),
                    checks: vec![],
                    warnings: vec![],
                    observed_at: now(),
                    expires_at: now(),
                    valuation: None,
                    intent: gofin_domain::FinancialIntent::default(),
                    operation_version: 1,
                    action_deadline: None,
                }),
                approval: None,
                submission: None,
                dispatch_evidence: None,
                position_baseline: None,
                effects: None,
                last_reconciliation_evidence: None,
                error: None,
                run_id: None,
            };
            let value = serde_json::to_value(record).unwrap();
            assert!(record_reserves_hyperliquid_owner(&value, owner).unwrap());
            assert!(
                !record_reserves_hyperliquid_owner(
                    &value,
                    "0x3333333333333333333333333333333333333333"
                )
                .unwrap()
            );
        }
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
                request: gofin_domain::OperationRequest {
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
    #[test]
    fn reviewed_rollups_default_to_one_sequenced_block() {
        let networks = NetworkConfig::reviewed_evm_networks();
        for chain_id in [42161, 4663] {
            let network = networks
                .iter()
                .find(|network| network.chain_id == chain_id)
                .unwrap();
            assert!(gofin_ethereum::EthereumConfig::is_reviewed_sequencer_rollup(chain_id));
            assert_eq!(network.confirmations, 1);
        }
        assert_eq!(
            networks
                .iter()
                .find(|network| network.chain_id == 1)
                .unwrap()
                .confirmations,
            2
        );
    }
}
