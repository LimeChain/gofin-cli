//! Protected composition and local protocol endpoint for Fin.
//!
//! Financial callers can access only typed protocol methods. Production startup
//! is rooted at the installed paths; the separately named composition entry
//! point exists for synthetic adapter/credential harnesses.

pub mod deposit;
pub mod enrollment;
pub use deposit::BinanceEthereumDepositAdapter;

use async_trait::async_trait;
use chrono::Utc;
use fin_authorization::{MacOsAuthorizer, validate_installation, validate_metadata};
use fin_domain::{
    Adapter, Amount, AssetId, AssetPrice, Authorizer, Definition, ExecutionActor, FinError,
    FinancialState, OperationKind, OperationRecord, PreparedOperation, Reconciliation, ResourceId,
    StateSnapshot, Submission, SubmissionJournal, now,
};
use fin_engine::{Engine, EngineOptions, RunAllocation, RunPlan, RunResult};
use fin_protocol::{BrokerRequest, BrokerResponse, RunBeginRequest, encode_response, read_request};
use fin_store::Store;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
    path::Path,
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

fn binance_ed25519_signature(private_key_pem: &str, message: &[u8]) -> fin_domain::Result<String> {
    use ed25519_dalek::{Signer as _, SigningKey, pkcs8::DecodePrivateKey};
    let key = SigningKey::from_pkcs8_pem(private_key_pem).map_err(|_| {
        error(
            "SIGNING_FAILED",
            "signing",
            "Binance Ed25519 key format is invalid.",
            "Re-enroll a valid Ed25519 PKCS#8 private key.",
        )
    })?;
    let signature = key.sign(message);
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()))
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
    Ethereum {
        name: String,
        resource: ResourceId,
        credential_ref: Option<String>,
        network: String,
        address: String,
        signing_enabled: bool,
        assets: Vec<AssetId>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrokerPolicy {
    pub require_run_for_writes: bool,
    pub max_movement_usd_micros: String,
    pub max_run_allocation_usd_micros: String,
    pub preview_ttl_seconds: u64,
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
        }
    }
}

impl DeploymentConfig {
    pub fn ethereum_mainnet() -> Self {
        let defaults = fin_ethereum::EthereumConfig::default();
        Self {
            network: "ethereum:1".into(),
            weth: defaults.weth,
            snapshot_tokens: defaults.snapshot_tokens,
            uniswap: defaults.uniswap,
            aave: defaults.aave,
        }
    }
}

impl BrokerPolicy {
    pub fn beta() -> Self {
        Self {
            require_run_for_writes: true,
            max_movement_usd_micros: "100000000".into(),
            max_run_allocation_usd_micros: "100000000".into(),
            preview_ttl_seconds: 90,
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
        if self.networks.is_empty() || self.deployments.is_empty() {
            return Err(error(
                "CONFIG_NETWORKS_REQUIRED",
                "configuration",
                "At least one network and one reviewed deployment are required.",
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
        for network in &self.networks {
            if network.id.is_empty()
                || network.chain_id == 0
                || network.confirmations == 0
                || network.rpc_urls.is_empty()
                || !networks.insert(network.id.clone())
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
        for value in [
            &self.policy.max_movement_usd_micros,
            &self.policy.max_run_allocation_usd_micros,
        ] {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(error(
                    "CONFIG_POLICY_INVALID",
                    "configuration",
                    "Financial policy bounds must be exact integer USD micros.",
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

pub trait BrokerCredentialProvider: fin_binance::BinanceSigner + Send + Sync {
    fn envelope_signer(
        &self,
        credential: &fin_credentials::CredentialRef,
    ) -> fin_domain::Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)>;
}

/// Root-owned credentials stay inside the protected provider. API keys are
/// copied only into the adapter's request header; private bytes are never
/// formatted, logged, returned to the protocol, or stored by the adapter.
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

    fn credential_ref(
        handle: &fin_binance::CredentialHandle,
    ) -> fin_domain::Result<fin_credentials::CredentialRef> {
        fin_credentials::CredentialRef::new(handle.as_str())
    }
}

#[async_trait]
impl fin_binance::BinanceSigner for ProtectedCredentialProvider {
    async fn api_key(
        &self,
        credential: &fin_binance::CredentialHandle,
    ) -> Result<String, fin_binance::TransportError> {
        let reference = Self::credential_ref(credential)
            .map_err(|_| fin_binance::TransportError::PermissionDenied)?;
        self.store
            .with_material(&reference, |material| match material {
                fin_credentials::SecretMaterial::BinanceHmac { api_key, .. }
                | fin_credentials::SecretMaterial::BinanceRsa { api_key, .. }
                | fin_credentials::SecretMaterial::BinanceEd25519 { api_key, .. } => {
                    Ok(api_key.clone())
                }
                fin_credentials::SecretMaterial::EthereumPrivateKey { .. } => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "credentials",
                    "Credential is not a Binance API key.",
                    "Select the enrolled Binance credential reference.",
                )),
            })
            .map_err(|_| fin_binance::TransportError::PermissionDenied)
    }

    async fn sign(
        &self,
        credential: &fin_binance::CredentialHandle,
        encoded_parameters: &[u8],
    ) -> Result<fin_binance::BinanceSignature, fin_binance::TransportError> {
        let reference = Self::credential_ref(credential)
            .map_err(|_| fin_binance::TransportError::PermissionDenied)?;
        self.store
            .with_material(&reference, |material| {
                use rsa::signature::{SignatureEncoding as _, Signer as _};
                match material {
                    fin_credentials::SecretMaterial::BinanceHmac { secret, .. } => {
                        use hmac::{Hmac, Mac};
                        type HmacSha256 = Hmac<sha2::Sha256>;
                        let mut mac =
                            HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| {
                                error(
                                    "SIGNING_FAILED",
                                    "signing",
                                    "Binance request signing failed.",
                                    "Check the enrolled credential.",
                                )
                            })?;
                        mac.update(encoded_parameters);
                        Ok(hex::encode(mac.finalize().into_bytes()))
                    }
                    fin_credentials::SecretMaterial::BinanceRsa { private_key, .. } => {
                        use rsa::{
                            RsaPrivateKey, pkcs1::DecodeRsaPrivateKey, pkcs1v15::SigningKey,
                            pkcs8::DecodePrivateKey,
                        };
                        let key = RsaPrivateKey::from_pkcs8_pem(private_key)
                            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(private_key))
                            .map_err(|_| {
                                error(
                                    "SIGNING_FAILED",
                                    "signing",
                                    "Binance RSA key format is invalid.",
                                    "Re-enroll a PKCS#8 or PKCS#1 RSA private key.",
                                )
                            })?;
                        let signature =
                            SigningKey::<sha2::Sha256>::new(key).sign(encoded_parameters);
                        use base64::Engine as _;
                        Ok(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()))
                    }
                    fin_credentials::SecretMaterial::BinanceEd25519 { private_key, .. } => {
                        binance_ed25519_signature(private_key, encoded_parameters)
                    }
                    fin_credentials::SecretMaterial::EthereumPrivateKey { .. } => Err(error(
                        "CREDENTIAL_TYPE_MISMATCH",
                        "credentials",
                        "Credential is not a Binance signing key.",
                        "Select the enrolled Binance credential reference.",
                    )),
                }
            })
            .map(fin_binance::BinanceSignature::from_provider)
            .map_err(|_| fin_binance::TransportError::PermissionDenied)
    }
}

impl BrokerCredentialProvider for ProtectedCredentialProvider {
    fn envelope_signer(
        &self,
        credential: &fin_credentials::CredentialRef,
    ) -> fin_domain::Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)> {
        self.store
            .with_material(credential, |material| match material {
                fin_credentials::SecretMaterial::EthereumPrivateKey { private_key } => {
                    let signer =
                        Arc::new(fin_evm::LocalEnvelopeSigner::from_private_key(private_key)?);
                    let address = format!("{:#x}", signer.address());
                    Ok((address, signer as Arc<dyn fin_evm::EnvelopeSigner>))
                }
                fin_credentials::SecretMaterial::BinanceHmac { .. }
                | fin_credentials::SecretMaterial::BinanceRsa { .. }
                | fin_credentials::SecretMaterial::BinanceEd25519 { .. } => Err(error(
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

pub struct PublicBinancePrices {
    client: reqwest::Client,
}

impl PublicBinancePrices {
    pub fn new() -> fin_domain::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(12))
            .build()
            .map_err(|_| {
                error(
                    "PRICE_UNAVAILABLE",
                    "configuration",
                    "Public market data transport could not be initialized.",
                    "Check broker networking.",
                )
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl fin_binance::BinancePriceProvider for PublicBinancePrices {
    async fn quote_usd(&self, coin: &str) -> fin_domain::Result<fin_binance::PriceQuote> {
        if coin.is_empty()
            || coin.len() > 24
            || !coin.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            return Err(error(
                "PRICE_ASSET_UNSUPPORTED",
                "valuation",
                "Asset has no safe Binance market symbol.",
                "Use an explicitly configured, supported asset symbol.",
            ));
        }
        let url = format!("https://api.binance.com/api/v3/ticker/price?symbol={coin}USDT");
        let response = self.client.get(url).send().await.map_err(|_| {
            error(
                "PRICE_UNAVAILABLE",
                "valuation",
                "Public Binance market quote is unavailable.",
                "Retry a read after the market data endpoint is reachable.",
            )
        })?;
        if !response.status().is_success() {
            return Err(error(
                "PRICE_UNAVAILABLE",
                "valuation",
                "Public Binance market quote was rejected.",
                "Use an asset with a listed USDT market.",
            ));
        }
        let value: Value = response.json().await.map_err(|_| {
            error(
                "PRICE_DATA_INVALID",
                "valuation",
                "Market quote response is malformed.",
                "Retry later; no financial quantity was rounded.",
            )
        })?;
        let price = value
            .get("price")
            .and_then(Value::as_str)
            .filter(|text| {
                !text.is_empty()
                    && text.len() <= 64
                    && text
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'.')
            })
            .ok_or_else(|| {
                error(
                    "PRICE_DATA_INVALID",
                    "valuation",
                    "Market quote did not contain an exact decimal string.",
                    "Use a supported market pair.",
                )
            })?;
        fin_domain::Quantity::from_decimal(price, 18).map_err(|_| {
            error(
                "PRICE_DATA_INVALID",
                "valuation",
                "Market quote precision is unsupported.",
                "Use a supported market pair with exact decimal precision.",
            )
        })?;
        Ok(fin_binance::PriceQuote {
            usd_per_unit: price.into(),
            observed_at: now(),
            source: format!("Binance {coin}USDT public ticker; USDT/USD parity assumption"),
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
    let mut networks_by_id = BTreeMap::<String, &NetworkConfig>::new();
    let mut eth_rpcs = BTreeMap::<u64, Arc<dyn fin_evm::EvmRpc>>::new();
    for network in &config.networks {
        let rpc = Arc::new(fin_evm::HttpRpc::new(
            network.rpc_urls.clone(),
            network.chain_id,
        )?);
        eth_rpcs.insert(network.chain_id, rpc.clone());
        networks_by_id.insert(network.id.clone(), network);
    }
    let mut ethereum_adapters = BTreeMap::<String, Arc<fin_ethereum::EthereumAdapter>>::new();
    for deployment in &config.deployments {
        let network = networks_by_id
            .get(&deployment.network)
            .expect("validated deployment network");
        let mut wallets = Vec::new();
        let mut signer_map = BTreeMap::new();
        for connection in &config.connections {
            let ConnectionConfig::Ethereum {
                name,
                network: wallet_network,
                address,
                signing_enabled,
                credential_ref,
                ..
            } = connection
            else {
                continue;
            };
            if wallet_network != &network.id {
                continue;
            }
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
        let ethereum_config = fin_ethereum::EthereumConfig {
            chain_id: network.chain_id,
            confirmations: network.confirmations,
            weth: deployment.weth.clone(),
            snapshot_tokens: deployment.snapshot_tokens.clone(),
            uniswap: deployment.uniswap.clone(),
            aave: deployment.aave.clone(),
        };
        let adapter = Arc::new(fin_ethereum::EthereumAdapter::new(
            ethereum_config,
            wallets,
            runtime,
        )?);
        ethereum_adapters.insert(network.id.clone(), adapter);
    }

    let mut adapters: Vec<Arc<dyn Adapter>> = ethereum_adapters
        .values()
        .cloned()
        .map(|adapter| adapter as Arc<dyn Adapter>)
        .collect();
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
        let mut destinations = Vec::new();
        for connection in &config.connections {
            if let ConnectionConfig::Ethereum {
                name,
                resource,
                network,
                address,
                assets,
                ..
            } = connection
                && networks_by_id
                    .get(network)
                    .is_some_and(|network| network.chain_id == 1)
            {
                let assets = assets
                    .iter()
                    .filter(|asset| asset.network == "ethereum:1" && asset.reference == "native")
                    .cloned()
                    .map(|asset| ("ETH".to_owned(), asset))
                    .collect::<BTreeMap<_, _>>();
                if !assets.is_empty() {
                    destinations.push(fin_binance::EthereumDestination {
                        name: name.clone(),
                        resource: resource.clone(),
                        address: address.clone(),
                        chain_id: 1,
                        assets,
                        confirmations: 2,
                    });
                }
            }
        }
        if accounts.is_empty() {
            return Err(error(
                "BINANCE_CONNECTIONS_INVALID",
                "configuration",
                "Configured Binance connections could not be mapped to protected account handles.",
                "Repair credential references through protected enrollment.",
            ));
        }
        let signer: Arc<dyn fin_binance::BinanceSigner> = credentials.clone();
        let transport = Arc::new(
            fin_binance::BinanceHttpTransport::new("https://api.binance.com", signer).map_err(
                |_| {
                    error(
                        "BINANCE_TRANSPORT_UNAVAILABLE",
                        "startup",
                        "Fixed Binance transport could not be initialized.",
                        "Check the protected broker installation.",
                    )
                },
            )?,
        );
        let price_provider = Arc::new(PublicBinancePrices::new()?);
        let payout_verifier = Arc::new(EthereumPayoutVerifier::new(
            ethereum_adapters
                .values()
                .map(|adapter| (adapter.config.chain_id, adapter.clone()))
                .collect(),
        )?);
        let adapter = Arc::new(fin_binance::BinanceAdapter::new(
            fin_binance::BinanceConfig {
                accounts,
                destinations,
            },
            transport,
            Some(price_provider.clone()),
            Some(payout_verifier),
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
    let valuator: Arc<dyn AssetValuator> = Arc::new(CompositeAssetValuator {
        binance: Arc::new(PublicBinancePrices::new()?),
        ethereum: ethereum_adapters,
    });
    Ok(ProtectedApplications { adapters, valuator })
}

pub struct CompositeAssetValuator {
    binance: Arc<PublicBinancePrices>,
    ethereum: BTreeMap<String, Arc<fin_ethereum::EthereumAdapter>>,
}

#[async_trait]
impl AssetValuator for CompositeAssetValuator {
    async fn usd_micros(&self, amount: &Amount) -> fin_domain::Result<(String, String, String)> {
        if amount.asset.network == "binance" {
            let quote = fin_binance::BinancePriceProvider::quote_usd(
                &*self.binance,
                &amount.asset.reference,
            )
            .await?;
            let price = decimal_ratio(&quote.usd_per_unit)?;
            let numerator = amount
                .atoms
                .to_string()
                .parse::<num_bigint::BigUint>()
                .map_err(|_| {
                    error(
                        "INVALID_AMOUNT",
                        "valuation",
                        "Asset amount is malformed.",
                        "Use exact base-unit amount text.",
                    )
                })?
                * price.0
                * num_bigint::BigUint::from(1_000_000u64);
            let denominator = pow10(amount.asset.decimals as u32) * pow10(price.1);
            let micros =
                (numerator + (&denominator - num_bigint::BigUint::from(1u8))) / denominator;
            return Ok((micros.to_string(), quote.observed_at, quote.source));
        }
        let network = amount
            .asset
            .network
            .strip_prefix("ethereum:")
            .ok_or_else(|| {
                error(
                    "PRICE_ASSET_UNSUPPORTED",
                    "valuation",
                    "Asset network has no reviewed valuation source.",
                    "Use a configured Binance or Ethereum asset.",
                )
            })?;
        let adapter = self
            .ethereum
            .get(&format!("ethereum:{network}"))
            .ok_or_else(|| {
                error(
                    "PRICE_ASSET_UNSUPPORTED",
                    "valuation",
                    "Asset network has no configured Ethereum oracle.",
                    "Use an asset on a configured network.",
                )
            })?;
        let (value, source) = adapter
            .quote_usd(&amount.asset.reference, amount.atoms.clone())
            .await?;
        Ok((value.to_string(), now(), source))
    }
}

fn decimal_ratio(value: &str) -> fin_domain::Result<(num_bigint::BigUint, u32)> {
    if value.is_empty()
        || value.starts_with('-')
        || value.contains(['e', 'E', '+'])
        || value.matches('.').count() > 1
    {
        return Err(error(
            "PRICE_DATA_INVALID",
            "valuation",
            "Price is not a canonical exact decimal.",
            "Use market data with exact decimal strings.",
        ));
    }
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    let digits = format!("{whole}{fractional}");
    let coefficient = digits.parse::<num_bigint::BigUint>().map_err(|_| {
        error(
            "PRICE_DATA_INVALID",
            "valuation",
            "Price is not an exact unsigned decimal.",
            "Use market data with exact decimal strings.",
        )
    })?;
    if fractional.len() > 18 {
        return Err(error(
            "PRICE_DATA_INVALID",
            "valuation",
            "Price precision exceeds the reviewed bound.",
            "Use market data with at most 18 fractional places.",
        ));
    }
    Ok((coefficient, fractional.len() as u32))
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
    valuator: Arc<dyn AssetValuator>,
    accepting: AtomicBool,
    protected_config_check: AtomicBool,
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
        require_run_for_writes: config.policy.require_run_for_writes,
        max_movement_and_cost_usd_micros: config.policy.max_movement_usd_micros.clone(),
        max_run_allocation_usd_micros: config.policy.max_run_allocation_usd_micros.clone(),
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
        valuator: dependencies.valuator,
        accepting: AtomicBool::new(true),
        protected_config_check: AtomicBool::new(false),
    }))
}

/// Compose the installed runtime only after fixed-path root installation,
/// protected store/config validation, and trusted credential setup succeed.
pub async fn protected_runtime() -> fin_domain::Result<Arc<BrokerRuntime>> {
    validate_installation()?;
    let config = load_protected_config()?;
    let store = open_protected_store()?;
    let credentials = Arc::new(ProtectedCredentialProvider::open()?);
    let applications = build_protected_applications(&config, credentials)?;
    for connection in &config.connections {
        let ConnectionConfig::Binance { name, .. } = connection else {
            continue;
        };
        let adapter = applications
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
    Ok(runtime)
}

impl BrokerRuntime {
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
        match request {
            R::Capabilities => Ok(
                json!({"version":2,"methods":protocol_methods(),"maxFrameBytes":fin_protocol::MAX_FRAME_BYTES}),
            ),
            R::Connections => {
                let connections = self
                    .adapters
                    .iter()
                    .flat_map(|adapter| adapter.connections())
                    .collect::<Vec<_>>();
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
            R::RunBegin { request } => {
                serde_json::to_value(self.begin_run(request).await?).map_err(|_| internal_error())
            }
            R::RunStatus { id } => {
                serde_json::to_value(self.engine.run(&id)?).map_err(|_| internal_error())
            }
            R::RunReplan { id, remaining } => {
                serde_json::to_value(self.replan_run(id, remaining).await?)
                    .map_err(|_| internal_error())
            }
            R::RunFinish { id } => {
                let final_snapshot = self.snapshot().await?;
                serde_json::to_value(self.engine.finish_run(&id, final_snapshot, Utc::now())?)
                    .map_err(|_| internal_error())
            }
            R::DaemonStart => Ok(json!({"status":"already_running_or_socket_activated"})),
            R::DaemonStop => {
                self.accepting.store(false, Ordering::SeqCst);
                Ok(json!({"status":"stopping_after_current_request"}))
            }
        }
    }

    pub async fn snapshot(&self) -> fin_domain::Result<StateSnapshot> {
        self.observer.snapshot().await
    }

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
        if allocation_value > parse_atoms(&self.config.policy.max_run_allocation_usd_micros)? {
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
        let route_prices = self.trusted_asset_prices(&current, &[]).await?;
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

fn protocol_methods() -> Vec<&'static str> {
    vec![
        "capabilities",
        "connections",
        "doctor",
        "read",
        "preview",
        "execute",
        "status",
        "operations",
        "snapshot",
        "run_begin",
        "run_status",
        "run_finish",
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
    runtime: &BrokerRuntime,
    expected_uid: u32,
) -> fin_domain::Result<()> {
    serve_connection_with_timeouts(stream, runtime, expected_uid, IpcTimeouts::PRODUCTION).await
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
    runtime: &BrokerRuntime,
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
        let current = load_protected_config()?;
        let expected =
            serde_json::to_value(runtime.configuration()).map_err(|_| internal_error())?;
        let observed = serde_json::to_value(current).map_err(|_| internal_error())?;
        if expected != observed {
            runtime.accepting.store(false, Ordering::SeqCst);
            let failure = BrokerResponse::failure(error(
                "CONFIG_CHANGED_RESTART_REQUIRED",
                "configuration",
                "Protected configuration changed after this broker loaded it.",
                "Allow the broker to stop and restart through launchd with the updated configuration.",
            ));
            write_ipc_response(&mut stream, &failure, timeouts.response_write).await?;
            drop(guard);
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
    runtime: Arc<BrokerRuntime>,
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
            serve_connection_with_timeouts(stream, &runtime, expected_uid, timeouts).await
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
    if store.has_active_run().map_err(|_| {
        error(
            "STORE_UNAVAILABLE",
            "configuration",
            "Active run state could not be inspected.",
            "Preserve every mapping until the broker journal is available.",
        )
    })? {
        return Ok(true);
    }
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
    use fin_domain::{
        Amount, OperationKind, PreparedOperation, Quantity, ResourceAmount, Submission,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn binance_ed25519_pkcs8_signs_raw_request_bytes_as_base64() {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        // RFC 8032 test seed; synthetic only. The standard PKCS#8 encoding is
        // also the format consumed by OpenSSL and Node's crypto.createPrivateKey.
        let pem = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g\n-----END PRIVATE KEY-----\n";
        let key = ed25519_dalek::SigningKey::from_pkcs8_pem(pem).unwrap();
        let empty = binance_ed25519_signature(pem, b"").unwrap();
        use base64::Engine as _;
        assert_eq!(
            hex::encode(
                base64::engine::general_purpose::STANDARD
                    .decode(empty)
                    .unwrap()
            ),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155".to_owned()
                + "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
        let request = b"coin=ETH&network=ETH&timestamp=1";
        let encoded = binance_ed25519_signature(pem, request).unwrap();
        let signature: [u8; 64] = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap()
            .try_into()
            .unwrap();
        use ed25519_dalek::Verifier as _;
        key.verifying_key()
            .verify(request, &ed25519_dalek::Signature::from_bytes(&signature))
            .unwrap();
    }

    struct ReadAdapter {
        captures: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Adapter for ReadAdapter {
        fn definitions(&self) -> Vec<Definition> {
            vec![
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
            ]
        }
        async fn snapshot(&self) -> fin_domain::Result<StateSnapshot> {
            self.captures.fetch_add(1, Ordering::SeqCst);
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
        async fn read(&self, _: &str, _: &Value) -> fin_domain::Result<Value> {
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
                })],
                authorizer: Arc::new(ReadOnlyAuthorizer),
                valuator: Arc::new(FixtureValuator),
            },
        )
        .unwrap();
        (runtime, captures)
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
}
