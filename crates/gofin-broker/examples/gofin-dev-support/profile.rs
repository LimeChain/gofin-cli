use crate::{dev_credentials, dotenv, identity};
use gofin_broker::{
    BrokerConfigV2, BrokerPolicy, ConnectionConfig, DeploymentConfig, NetworkConfig,
};
use gofin_credentials::{CredentialRef, SecretMaterial};
use gofin_domain::{Quantity, ResourceId};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
};
use zeroize::Zeroizing;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialReferences {
    #[serde(default)]
    pub kraken: Option<String>,
    #[serde(default)]
    pub krakenfutures: Option<String>,
    pub binance: Option<String>,
    #[serde(default)]
    pub binance_sub: Option<String>,
    pub evm_primary: Option<String>,
    pub evm_secondary: Option<String>,
    #[serde(default)]
    pub solana: Option<String>,
    #[serde(default)]
    pub jupiter: Option<String>,
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub coinbase: Option<String>,
    #[serde(default)]
    pub coinbaseexchange: Option<String>,
    #[serde(default)]
    pub coinbaseinternational: Option<String>,
}

/// Optional per-development-environment ceilings. These can only tighten the
/// beta defaults and are persisted with the environment, independently of its
/// disposable credentials.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyLimits {
    pub max_movement_usd_micros: Quantity,
    pub max_allocation_usd_micros: Quantity,
}

impl PolicyLimits {
    fn validate(&self) -> Result<(), String> {
        let maximum = gofin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS;
        for (name, value) in [
            ("max_movement_usd_micros", &self.max_movement_usd_micros),
            ("max_allocation_usd_micros", &self.max_allocation_usd_micros),
        ] {
            let parsed = value.as_str().parse::<u64>().map_err(|_| {
                format!("development {name} must be a finite positive USD-micros quantity")
            })?;
            if parsed == 0 || parsed > maximum {
                return Err(format!(
                    "development {name} must be between 1 and {maximum} USD micros"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenDevelopmentAccount {
    pub product: String,
    pub name: String,
    pub account_id: String,
    pub credential_ref: String,
    #[serde(default)]
    pub default_wallet_account_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    #[serde(default)]
    pub kraken_accounts: BTreeMap<String, KrakenDevelopmentAccount>,
    #[serde(default)]
    pub kraken_account_id: Option<String>,
    #[serde(default)]
    pub kraken_default_wallet_account_id: Option<String>,
    #[serde(default)]
    pub krakenfutures_account_id: Option<String>,
    pub format: String,
    pub environment_id: String,
    pub version: u32,
    #[serde(default = "synthetic")]
    pub mode: String,
    #[serde(default)]
    pub credentials: CredentialReferences,
    /// Optional same-user development policy override. Legacy development
    /// environments omit this field and retain the beta policy defaults.
    #[serde(default)]
    pub policy: Option<BrokerPolicy>,
    /// Optional explicit per-chain public RPC endpoints for this disposable
    /// development profile. Missing chains retain the reviewed defaults. Explicit
    /// all-chain numeric loopback overrides select owned-fork transport injection.
    #[serde(default)]
    pub rpc_urls: BTreeMap<u64, Vec<String>>,
    /// Optional subset of reviewed EVM networks for disposable development.
    /// `None` preserves the historical all-reviewed-networks behavior.
    #[serde(default)]
    pub reviewed_chain_ids: Option<Vec<u64>>,
    #[serde(default)]
    pub binance_uid: Option<String>,
    #[serde(default)]
    pub binance_sub_uid: Option<String>,
    #[serde(default)]
    pub binance_sub_email: Option<String>,
    #[serde(default)]
    pub binance_sub_email_matched_by_venue_id: bool,
    #[serde(default)]
    pub evm_addresses: Vec<String>,
    #[serde(default)]
    pub solana_address: Option<String>,
    #[serde(default)]
    pub policy_limits: Option<PolicyLimits>,
    #[serde(default)]
    pub coinbase_portfolio_id: Option<String>,
    #[serde(default)]
    pub coinbaseexchange_portfolio_id: Option<String>,
    #[serde(default)]
    pub coinbaseinternational_portfolio_id: Option<String>,
}

fn synthetic() -> String {
    "synthetic".into()
}

impl EnvironmentConfig {
    pub fn credentials_is_empty(&self) -> bool {
        self.kraken_accounts.is_empty()
            && self.credentials.kraken.is_none()
            && self.credentials.krakenfutures.is_none()
            && self.credentials.binance.is_none()
            && self.credentials.binance_sub.is_none()
            && self.credentials.evm_primary.is_none()
            && self.credentials.evm_secondary.is_none()
            && self.credentials.solana.is_none()
            && self.credentials.jupiter.is_none()
            && self.credentials.relay.is_none()
            && self.credentials.coinbase.is_none()
            && self.credentials.coinbaseexchange.is_none()
            && self.credentials.coinbaseinternational.is_none()
    }
    pub fn empty(environment_id: &str) -> Self {
        Self {
            format: "gofin-dev-config".into(),
            kraken_accounts: BTreeMap::new(),
            kraken_account_id: None,
            kraken_default_wallet_account_id: None,
            krakenfutures_account_id: None,
            environment_id: environment_id.into(),
            version: 2,
            mode: "synthetic".into(),
            credentials: CredentialReferences::default(),
            policy: None,
            rpc_urls: BTreeMap::new(),
            reviewed_chain_ids: None,
            binance_uid: None,
            binance_sub_uid: None,
            binance_sub_email: None,
            binance_sub_email_matched_by_venue_id: false,
            evm_addresses: Vec::new(),
            solana_address: None,
            policy_limits: None,
            coinbase_portfolio_id: None,
            coinbaseexchange_portfolio_id: None,
            coinbaseinternational_portfolio_id: None,
        }
    }
    pub fn read(path: &Path, environment_id: &str) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::empty(environment_id));
        }
        let raw =
            fs::read(path).map_err(|_| "development configuration could not be read".to_owned())?;
        let mut config: Self = serde_json::from_slice(&raw)
            .map_err(|_| "development configuration is invalid".to_owned())?;
        if config.environment_id != environment_id
            || !matches!(config.version, 1 | 2)
            || !matches!(
                config.format.as_str(),
                "gofin-dev-config" | "fin-dev-config" | "fin-dev-synthetic-config"
            )
        {
            return Err("development configuration identity mismatch".into());
        }
        config.format = "gofin-dev-config".into();
        // Version 2 is an additive profile migration. Serde defaults preserve
        // all older credential references, policy, RPC scope and identities.
        if config.version == 1 {
            config.version = 2;
        }
        if let Some(limits) = &config.policy_limits {
            limits.validate()?;
        }
        Ok(config)
    }
    pub fn write_atomic(&self, path: &Path) -> Result<(), String> {
        let bytes = Zeroizing::new(
            serde_json::to_vec_pretty(self)
                .map_err(|_| "development configuration could not be encoded".to_owned())?,
        );
        let parent = path.parent().ok_or("configuration path has no parent")?;
        let temp = parent.join(format!(".config.{}.tmp", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options
            .open(&temp)
            .map_err(|_| "development configuration could not be written".to_owned())?;
        use std::io::Write;
        let save = file
            .write_all(&bytes)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all());
        if save.is_err() {
            let _ = fs::remove_file(&temp);
            return Err("development configuration could not be written".into());
        }
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(|_| {
            let _ = fs::remove_file(&temp);
            "development configuration permissions could not be set".to_owned()
        })?;
        fs::rename(&temp, path).map_err(|_| {
            let _ = fs::remove_file(&temp);
            "development configuration could not be replaced".to_owned()
        })?;
        let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
        Ok(())
    }
}

pub async fn import(
    config_path: &Path,
    environment_id: &str,
    env_file: &Path,
    credential_dir: &Path,
    discover_identities: bool,
    provider: Option<&str>,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(env_file)
        .map_err(|_| "credential file could not be read".to_owned())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > 32 * 1024
        || metadata.mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("credential file must be a private regular file of supported size".into());
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(env_file)
        .map_err(|_| "credential file could not be read".to_owned())?;
    use std::io::Read;
    let mut source = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.take(32 * 1024 + 1)
        .read_to_end(&mut source)
        .map_err(|_| "credential file could not be read".to_owned())?;
    if source.len() > 32 * 1024 {
        return Err("credential file exceeds the supported size".into());
    }
    let source =
        std::str::from_utf8(&source).map_err(|_| "credential file must be UTF-8".to_owned())?;
    if let Some(provider) = provider
        && !matches!(
            provider,
            "binance"
                | "evm"
                | "solana"
                | "relay"
                | "coinbase"
                | "coinbaseexchange"
                | "coinbaseinternational"
        )
    {
        return Err("--provider must be binance, evm, solana, relay or a Coinbase product".into());
    }
    if discover_identities && provider.is_some_and(|provider| provider != "binance") {
        return Err("--discover-identities is available only for Binance imports".into());
    }
    let values = dotenv::parse_selected(source, provider)?;
    let mut config = EnvironmentConfig::read(config_path, environment_id)?;
    let old = config.clone();
    let mut additions: Vec<(String, SecretMaterial)> = Vec::new();
    let mut ephemeral = Vec::new();
    for (provider_name, prefix) in [
        ("coinbase", "GOFIN_DEV_COINBASE"),
        ("coinbaseexchange", "GOFIN_DEV_COINBASEEXCHANGE"),
        ("coinbaseinternational", "GOFIN_DEV_COINBASEINTERNATIONAL"),
    ] {
        let key = values.get(&format!("{prefix}_API_KEY"));
        let secret = values.get(&format!("{prefix}_SECRET"));
        let pem = values.get(&format!("{prefix}_PRIVATE_KEY_PEM"));
        let passphrase = values.get(&format!("{prefix}_PASSPHRASE"));
        let kind = values
            .get(&format!("{prefix}_CREDENTIAL_TYPE"))
            .map(|v| v.as_str());
        let selector = values
            .get(&format!("{prefix}_PORTFOLIO_ID"))
            .map(|v| v.to_string());
        let present = [
            key.is_some(),
            secret.is_some(),
            pem.is_some(),
            passphrase.is_some(),
            kind.is_some(),
            selector.is_some(),
        ]
        .iter()
        .any(|v| *v);
        if !present {
            continue;
        }
        let key = key.ok_or_else(|| format!("{prefix}_API_KEY is required"))?;
        let material = match provider_name {
            "coinbase" => {
                match kind.unwrap_or(if pem.is_some() { "ecdsa" } else { "ed25519" }) {
                    "ed25519" if secret.is_some() && pem.is_none() && passphrase.is_none() => {
                        SecretMaterial::CoinbaseCdpEd25519 {
                            api_key: key.to_string(),
                            private_key_base64: secret.unwrap().to_string(),
                        }
                    }
                    "ecdsa" if pem.is_some() && secret.is_none() && passphrase.is_none() => {
                        SecretMaterial::CoinbaseCdpEcdsa {
                            api_key: key.to_string(),
                            private_key_pem: pem.unwrap().to_string(),
                        }
                    }
                    _ => return Err(
                        "Coinbase credentials require explicit ed25519 base64 secret or ecdsa PEM"
                            .into(),
                    ),
                }
            }
            "coinbaseexchange" | "coinbaseinternational"
                if secret.is_some() && passphrase.is_some() && pem.is_none() && kind.is_none() =>
            {
                if provider_name == "coinbaseexchange" {
                    SecretMaterial::CoinbaseExchangeHmac {
                        api_key: key.to_string(),
                        secret: secret.unwrap().to_string(),
                        passphrase: passphrase.unwrap().to_string(),
                    }
                } else {
                    SecretMaterial::CoinbaseInternationalHmac {
                        api_key: key.to_string(),
                        secret: secret.unwrap().to_string(),
                        passphrase: passphrase.unwrap().to_string(),
                    }
                }
            }
            _ => {
                return Err(
                    "Coinbase Exchange and International require API key, secret and passphrase"
                        .into(),
                );
            }
        };
        material
            .validate_present()
            .map_err(|_| "Coinbase credential format is invalid")?;
        let auth = match &material {
            SecretMaterial::CoinbaseCdpEd25519 {
                api_key,
                private_key_base64,
            } => gofin_exchange::coinbase::CoinbaseAuth::CdpEd25519 {
                api_key: api_key.clone(),
                private_key_base64: private_key_base64.clone(),
            },
            SecretMaterial::CoinbaseCdpEcdsa {
                api_key,
                private_key_pem,
            } => gofin_exchange::coinbase::CoinbaseAuth::CdpEcdsa {
                api_key: api_key.clone(),
                private_key_pem: private_key_pem.clone(),
            },
            SecretMaterial::CoinbaseExchangeHmac {
                api_key,
                secret,
                passphrase,
            } => gofin_exchange::coinbase::CoinbaseAuth::ExchangeHmac {
                api_key: api_key.clone(),
                secret: secret.clone(),
                passphrase: passphrase.clone(),
            },
            SecretMaterial::CoinbaseInternationalHmac {
                api_key,
                secret,
                passphrase,
            } => gofin_exchange::coinbase::CoinbaseAuth::InternationalHmac {
                api_key: api_key.clone(),
                secret: secret.clone(),
                passphrase: passphrase.clone(),
            },
            _ => unreachable!(),
        };
        let client = gofin_exchange::coinbase::CoinbaseTransport::new(auth)
            .map_err(|_| "Coinbase transport could not be prepared")?;
        let (identity, observed_portfolio) =
            dev_coinbase_identity(&client, provider_name, selector.as_deref()).await?;
        if selector
            .as_ref()
            .is_some_and(|portfolio| portfolio != &observed_portfolio)
        {
            return Err(
                "selected Coinbase portfolio does not match the authenticated portfolio identity"
                    .into(),
            );
        }
        let identity_key = match provider_name {
            "coinbase" => &old.coinbase_portfolio_id,
            "coinbaseexchange" => &old.coinbaseexchange_portfolio_id,
            _ => &old.coinbaseinternational_portfolio_id,
        };
        if identity_key
            .as_ref()
            .is_some_and(|saved| saved != &identity)
        {
            return Err("reimported Coinbase portfolio differs from the saved environment; account swaps are refused".into());
        }
        match provider_name {
            "coinbase" => config.coinbase_portfolio_id = Some(identity),
            "coinbaseexchange" => config.coinbaseexchange_portfolio_id = Some(identity),
            _ => config.coinbaseinternational_portfolio_id = Some(identity),
        }
        additions.push((provider_name.into(), material));
    }
    let main_material = make_binance_material(
        &values,
        "GOFIN_DEV_BINANCE_API_KEY",
        "GOFIN_DEV_BINANCE_SECRET",
        "GOFIN_DEV_BINANCE_PRIVATE_KEY_PEM",
        "main Binance",
    )?;
    let sub_material = make_binance_material(
        &values,
        "GOFIN_DEV_BINANCE_SUB_API_KEY",
        "GOFIN_DEV_BINANCE_SUB_SECRET",
        "GOFIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM",
        "subaccount Binance",
    )?;
    if let Some((material, key, transport_secret)) = main_material {
        ephemeral.push(("dev-import-main".into(), key, transport_secret));
        additions.push(("binance".into(), material));
    }
    if let Some((material, key, transport_secret)) = sub_material {
        ephemeral.push(("dev-import-sub".into(), key, transport_secret));
        additions.push(("binance_sub".into(), material));
    }
    for (label, prefix) in [
        ("kraken", "GOFIN_DEV_KRAKEN"),
        ("krakenfutures", "GOFIN_DEV_KRAKENFUTURES"),
    ] {
        let key = values.get(&format!("{prefix}_API_KEY"));
        let secret = values.get(&format!("{prefix}_SECRET"));
        let identity = values.get(&format!("{prefix}_ACCOUNT_ID"));
        if key.is_none() && secret.is_none() && identity.is_none() {
            continue;
        }
        let key = key.ok_or_else(|| format!("{prefix}_API_KEY is required"))?;
        let secret = secret.ok_or_else(|| format!("{prefix}_SECRET is required"))?;
        let identity = identity
            .ok_or_else(|| format!("{prefix}_ACCOUNT_ID is required for offline import"))?;
        if identity.is_empty()
            || identity.len() > 128
            || !identity
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err("Kraken account ID is invalid".into());
        }
        let old_identity = if label == "kraken" {
            &old.kraken_account_id
        } else {
            &old.krakenfutures_account_id
        };
        if old_identity
            .as_ref()
            .is_some_and(|v| v != identity.as_str())
        {
            return Err(
                "Kraken reimport cannot change an existing physical account identity".into(),
            );
        }
        let material = if label == "kraken" {
            SecretMaterial::KrakenApi {
                api_key: key.to_string(),
                secret: secret.to_string(),
            }
        } else {
            SecretMaterial::KrakenFuturesApi {
                api_key: key.to_string(),
                secret: secret.to_string(),
            }
        };
        material
            .validate_present()
            .map_err(|_| "Kraken API key/secret format is invalid")?;
        if label == "kraken" {
            config.kraken_account_id = Some(identity.to_string());
        } else {
            config.krakenfutures_account_id = Some(identity.to_string());
        }
        additions.push((label.into(), material));
    }
    if let Some(id) = values.get("GOFIN_DEV_KRAKEN_DEFAULT_WALLET_ACCOUNT_ID") {
        if !additions.iter().any(|(label, _)| label == "kraken") {
            return Err(
                "GOFIN_DEV_KRAKEN_DEFAULT_WALLET_ACCOUNT_ID requires a Spot credential import"
                    .into(),
            );
        }
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err("Kraken default wallet account ID is invalid".into());
        }
        if old
            .kraken_default_wallet_account_id
            .as_ref()
            .is_some_and(|previous| previous != id.as_str())
        {
            return Err("Kraken reimport cannot change an existing default wallet alias".into());
        }
        config.kraken_default_wallet_account_id = Some(id.to_string());
    }
    for (product, prefix) in [
        ("kraken", "GOFIN_DEV_KRAKEN"),
        ("krakenfutures", "GOFIN_DEV_KRAKENFUTURES"),
    ] {
        for index in 1..=8 {
            let slot = format!("{product}_{index}");
            let prefix = format!("{prefix}_{index}");
            let key = values.get(&format!("{prefix}_API_KEY"));
            let secret = values.get(&format!("{prefix}_SECRET"));
            let id = values.get(&format!("{prefix}_ACCOUNT_ID"));
            let name = values.get(&format!("{prefix}_NAME"));
            let wallet = values.get(&format!("{prefix}_DEFAULT_WALLET_ACCOUNT_ID"));
            if [key, secret, id, name, wallet].iter().all(|v| v.is_none()) {
                continue;
            }
            let key = key.ok_or_else(|| format!("{prefix}_API_KEY is required"))?;
            let secret = secret.ok_or_else(|| format!("{prefix}_SECRET is required"))?;
            let id =
                id.ok_or_else(|| format!("{prefix}_ACCOUNT_ID is required for offline import"))?;
            let valid_id = |v: &str| {
                !v.is_empty()
                    && v.len() <= 128
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            };
            if !valid_id(id) {
                return Err("Indexed Kraken account ID is invalid".into());
            }
            let previous = old.kraken_accounts.get(&slot);
            if previous.is_some_and(|v| v.product != product || v.account_id != id.as_str()) {
                return Err(
                    "Kraken reimport cannot change an existing indexed physical account identity"
                        .into(),
                );
            }
            let name = name
                .map(|v| v.to_string())
                .or_else(|| previous.map(|v| v.name.clone()))
                .unwrap_or_else(|| format!("dev-{product}-{index}"));
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err("Indexed Kraken connection name is invalid".into());
            }
            if previous.is_some_and(|v| v.name != name) {
                return Err("Kraken reimport cannot rename an existing indexed connection".into());
            }
            let default_wallet_account_id = wallet
                .map(|v| v.to_string())
                .or_else(|| previous.and_then(|v| v.default_wallet_account_id.clone()));
            if default_wallet_account_id
                .as_deref()
                .is_some_and(|v| product != "kraken" || !valid_id(v))
            {
                return Err("Indexed Kraken default wallet alias is invalid".into());
            }
            if let Some(selected) = wallet
                && previous
                    .and_then(|v| v.default_wallet_account_id.as_deref())
                    .is_some_and(|old| old != selected.as_str())
            {
                return Err(
                    "Kraken reimport cannot change an existing indexed default wallet alias".into(),
                );
            }
            let material = if product == "kraken" {
                SecretMaterial::KrakenApi {
                    api_key: key.to_string(),
                    secret: secret.to_string(),
                }
            } else {
                SecretMaterial::KrakenFuturesApi {
                    api_key: key.to_string(),
                    secret: secret.to_string(),
                }
            };
            material
                .validate_present()
                .map_err(|_| "Indexed Kraken API key/secret format is invalid")?;
            config.kraken_accounts.insert(
                slot.clone(),
                KrakenDevelopmentAccount {
                    product: product.into(),
                    name,
                    account_id: id.to_string(),
                    credential_ref: previous
                        .map(|v| v.credential_ref.clone())
                        .unwrap_or_default(),
                    default_wallet_account_id,
                },
            );
            additions.push((slot, material));
        }
    }
    validate_kraken_group_identities(&config)?;
    let has_main = additions.iter().any(|(label, _)| label == "binance");
    let has_sub = additions.iter().any(|(label, _)| label == "binance_sub");
    let main_uid_input = values.get("GOFIN_DEV_BINANCE_UID").map(|v| v.to_string());
    let sub_uid_input = values
        .get("GOFIN_DEV_BINANCE_SUB_UID")
        .map(|v| v.to_string());
    let mut sub_email = values
        .get("GOFIN_DEV_BINANCE_SUB_EMAIL")
        .map(|v| v.to_string());
    let mut sub_email_matched_by_id = false;
    if !has_main && main_uid_input.is_some() {
        return Err("GOFIN_DEV_BINANCE_UID requires a main Binance credential".into());
    }
    if !has_sub && (sub_uid_input.is_some() || sub_email.is_some()) {
        return Err("Binance subaccount identity fields require a subaccount credential".into());
    }
    if has_sub && !has_main {
        return Err("a Binance subaccount import requires its matching main account key".into());
    }
    if has_main || has_sub {
        let (main_uid, sub_uid) = if discover_identities {
            if !has_main {
                return Err("--discover-identities requires the main Binance key".into());
            }
            if main_uid_input
                .as_deref()
                .is_some_and(|uid| !canonical_uid(uid))
                || sub_uid_input
                    .as_deref()
                    .is_some_and(|uid| !canonical_uid(uid))
            {
                return Err("configured Binance account UID is invalid".into());
            }
            let provider = Arc::new(
                dev_credentials::ImportCredentialProvider::new(ephemeral)
                    .map_err(|_| "temporary Binance credentials could not be prepared")?,
            );
            let api = identity::CcxtBinanceIdentityApi::new(provider)?;
            if has_sub {
                let resolved = identity::resolve_binance_identity(
                    &api,
                    sub_email.as_deref(),
                    main_uid_input.as_deref(),
                    sub_uid_input.as_deref(),
                )
                .await?;
                sub_email = Some(resolved.sub_email);
                sub_email_matched_by_id = resolved.sub_email_matched_by_venue_id;
                (resolved.main_uid, Some(resolved.sub_uid))
            } else {
                (
                    identity::resolve_main_uid(&api, main_uid_input.as_deref()).await?,
                    None,
                )
            }
        } else {
            let main_uid = main_uid_input
                .ok_or("Binance import requires GOFIN_DEV_BINANCE_UID or --discover-identities")?;
            if !canonical_uid(&main_uid) {
                return Err("Binance account UID is invalid".into());
            }
            let sub_uid = if has_sub {
                let uid = sub_uid_input.ok_or("Binance subaccount import requires GOFIN_DEV_BINANCE_SUB_UID or --discover-identities")?;
                if !canonical_uid(&uid) {
                    return Err("Binance subaccount UID is invalid".into());
                }
                sub_email.as_ref().filter(|email| !email.trim().is_empty())
                    .ok_or("Binance subaccount import requires GOFIN_DEV_BINANCE_SUB_EMAIL or --discover-identities")?;
                Some(uid)
            } else {
                None
            };
            (main_uid, sub_uid)
        };
        if old
            .binance_uid
            .as_ref()
            .is_some_and(|old_uid| old_uid != &main_uid)
            || old
                .binance_sub_uid
                .as_ref()
                .is_some_and(|old_uid| Some(old_uid) != sub_uid.as_ref())
            || old
                .binance_sub_email
                .as_ref()
                .is_some_and(|old_email| sub_email.as_ref() != Some(old_email))
        {
            return Err("reimported Binance identity differs from the saved environment; account swaps are refused".into());
        }
        if sub_uid.as_ref().is_some_and(|sub_uid| sub_uid == &main_uid) {
            return Err(
                "Binance main and subaccount keys resolve to the same physical account".into(),
            );
        }
        if has_main {
            config.binance_uid = Some(main_uid);
        }
        if let Some(uid) = sub_uid {
            config.binance_sub_uid = Some(uid);
        }
        if has_sub {
            config.binance_sub_email = sub_email;
            config.binance_sub_email_matched_by_venue_id = sub_email_matched_by_id;
        }
    }
    for (label, key_name, address_name) in [
        (
            "evm_primary",
            "GOFIN_DEV_EVM_PRIVATE_KEY",
            "GOFIN_DEV_EVM_ADDRESS",
        ),
        (
            "evm_secondary",
            "GOFIN_DEV_EVM_SECONDARY_PRIVATE_KEY",
            "GOFIN_DEV_EVM_SECONDARY_ADDRESS",
        ),
    ] {
        if let Some(private_key) = values.get(key_name) {
            let signer = gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)
                .map_err(|_| format!("{key_name} is invalid"))?;
            let derived = format!("{:#x}", signer.address()).to_ascii_lowercase();
            if values
                .get(address_name)
                .is_some_and(|supplied| !supplied.eq_ignore_ascii_case(&derived))
            {
                return Err(format!("{address_name} does not match its private key"));
            }
            additions.push((
                label.into(),
                SecretMaterial::EvmPrivateKey {
                    private_key: private_key.to_string(),
                },
            ));
            let index = if label == "evm_primary" { 0 } else { 1 };
            if old.evm_addresses.get(index).is_some_and(|old_address| {
                !old_address.is_empty() && !old_address.eq_ignore_ascii_case(&derived)
            }) {
                return Err("reimported EVM wallet differs from the saved environment; account swaps are refused".into());
            }
            while config.evm_addresses.len() <= index {
                config.evm_addresses.push(String::new());
            }
            config.evm_addresses[index] = derived;
        } else if values.contains_key(address_name) {
            return Err(format!("{address_name} requires its private key"));
        }
    }
    if let Some(private_key) = values.get("GOFIN_DEV_SOLANA_PRIVATE_KEY") {
        let signer = gofin_solana::SolanaSigner::parse(private_key)
            .map_err(|_| "GOFIN_DEV_SOLANA_PRIVATE_KEY is invalid".to_owned())?;
        let derived = signer.address().to_owned();
        if old
            .solana_address
            .as_ref()
            .is_some_and(|address| address != &derived)
        {
            return Err("reimported Solana wallet differs from the saved environment; account swaps are refused".into());
        }
        additions.push((
            "solana".into(),
            SecretMaterial::SolanaPrivateKey {
                private_key: private_key.to_string(),
            },
        ));
        config.solana_address = Some(derived);
    }
    if let Some(api_key) = values.get("GOFIN_DEV_RELAY_API_KEY") {
        let material = SecretMaterial::RelayApiKey {
            api_key: api_key.to_string(),
        };
        material
            .validate_present()
            .map_err(|_| "GOFIN_DEV_RELAY_API_KEY is invalid".to_owned())?;
        additions.push(("relay".into(), material));
    }
    if additions.is_empty() {
        return Err("credential file contains no complete supported connection".into());
    }
    dev_credentials::create_store(credential_dir)
        .map_err(|_| "development credential store is unavailable".to_owned())?;
    let old_references = config.credentials.clone();
    let mut written = Vec::new();
    for (label, material) in additions {
        let reference =
            dev_credentials::write_material(credential_dir, &material).map_err(|_| {
                for path in &written {
                    let _ = fs::remove_file(path);
                }
                "development credential could not be stored".to_owned()
            })?;
        written.push(credential_dir.join(format!("{}.json", reference.as_str())));
        match label.as_str() {
            "kraken" => config.credentials.kraken = Some(reference.as_str().into()),
            "krakenfutures" => config.credentials.krakenfutures = Some(reference.as_str().into()),
            "binance" => config.credentials.binance = Some(reference.as_str().into()),
            "binance_sub" => config.credentials.binance_sub = Some(reference.as_str().into()),
            "evm_primary" => config.credentials.evm_primary = Some(reference.as_str().into()),
            "evm_secondary" => config.credentials.evm_secondary = Some(reference.as_str().into()),
            "solana" => config.credentials.solana = Some(reference.as_str().into()),
            "relay" => config.credentials.relay = Some(reference.as_str().into()),
            "coinbase" => config.credentials.coinbase = Some(reference.as_str().into()),
            "coinbaseexchange" => {
                config.credentials.coinbaseexchange = Some(reference.as_str().into())
            }
            "coinbaseinternational" => {
                config.credentials.coinbaseinternational = Some(reference.as_str().into())
            }
            label if config.kraken_accounts.contains_key(label) => {
                config
                    .kraken_accounts
                    .get_mut(label)
                    .expect("validated indexed slot")
                    .credential_ref = reference.as_str().into()
            }
            _ => unreachable!(),
        }
    }
    config.mode = "live".into();
    config.format = "gofin-dev-config".into();
    if broker_config(&config)
        .and_then(|candidate| {
            candidate
                .validate()
                .map_err(|_| "development connection profile is invalid".to_owned())
        })
        .is_err()
    {
        for path in &written {
            let _ = fs::remove_file(path);
        }
        return Err("development connection profile is invalid; no changes were saved".into());
    }
    if let Err(error) = config.write_atomic(config_path) {
        for path in &written {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    for (new_ref, old_ref) in [
        (&config.credentials.kraken, &old_references.kraken),
        (
            &config.credentials.krakenfutures,
            &old_references.krakenfutures,
        ),
        (&config.credentials.binance, &old_references.binance),
        (&config.credentials.binance_sub, &old_references.binance_sub),
        (&config.credentials.evm_primary, &old_references.evm_primary),
        (
            &config.credentials.evm_secondary,
            &old_references.evm_secondary,
        ),
        (&config.credentials.solana, &old_references.solana),
        (&config.credentials.jupiter, &old_references.jupiter),
        (&config.credentials.relay, &old_references.relay),
        (&config.credentials.coinbase, &old_references.coinbase),
        (
            &config.credentials.coinbaseexchange,
            &old_references.coinbaseexchange,
        ),
        (
            &config.credentials.coinbaseinternational,
            &old_references.coinbaseinternational,
        ),
    ] {
        if new_ref != old_ref
            && let Some(reference) = old_ref
                .as_deref()
                .and_then(|value| CredentialRef::new(value).ok())
        {
            let _ = fs::remove_file(credential_dir.join(format!("{}.json", reference.as_str())));
        }
    }
    for (slot, prior) in &old.kraken_accounts {
        if config
            .kraken_accounts
            .get(slot)
            .is_some_and(|new| new.credential_ref != prior.credential_ref)
            && let Ok(reference) = CredentialRef::new(&prior.credential_ref)
        {
            let _ = fs::remove_file(credential_dir.join(format!("{}.json", reference.as_str())));
        }
    }
    Ok(())
}

fn make_binance_material(
    values: &BTreeMap<String, Zeroizing<String>>,
    key_name: &str,
    secret_name: &str,
    pem_name: &str,
    label: &str,
) -> Result<Option<(SecretMaterial, String, String)>, String> {
    let api_key = values.get(key_name);
    let secret = values.get(secret_name);
    let pem = values.get(pem_name);
    if api_key.is_none() && secret.is_none() && pem.is_none() {
        return Ok(None);
    }
    let api_key = api_key.ok_or_else(|| format!("{label} import requires {key_name}"))?;
    if secret.is_some() == pem.is_some() {
        return Err(format!(
            "{label} import requires exactly one of {secret_name} or {pem_name}"
        ));
    }
    let material = if let Some(secret) = secret {
        SecretMaterial::BinanceHmac {
            api_key: api_key.to_string(),
            secret: secret.to_string(),
        }
    } else {
        classify_binance_pem(api_key.to_string(), pem.unwrap().to_string())?
    };
    material
        .validate_present()
        .map_err(|_| format!("{label} credential format is invalid"))?;
    let transport_secret = match &material {
        SecretMaterial::BinanceHmac { secret, .. } => secret.clone(),
        SecretMaterial::BinanceRsa { private_key, .. }
        | SecretMaterial::BinanceEd25519 { private_key, .. } => private_key.clone(),
        _ => return Err(format!("{label} credential type is invalid")),
    };
    Ok(Some((material, api_key.to_string(), transport_secret)))
}

async fn dev_coinbase_identity(
    client: &gofin_exchange::coinbase::CoinbaseTransport,
    provider: &str,
    selector: Option<&str>,
) -> Result<(String, String), String> {
    use gofin_exchange::coinbase::{CoinbaseApi as _, CoinbaseEndpoint as E, CoinbaseRequest};
    async fn read(
        client: &gofin_exchange::coinbase::CoinbaseTransport,
        endpoint: E,
    ) -> Result<serde_json::Value, String> {
        let response = client
            .request(endpoint, CoinbaseRequest::get(), None)
            .await
            .map_err(|_| "Coinbase authenticated read failed")?;
        if !(200..300).contains(&response.status) {
            return Err("Coinbase authenticated read failed".into());
        }
        serde_json::from_slice(&response.body)
            .map_err(|_| "Coinbase returned invalid account metadata".into())
    }
    fn strings(value: &serde_json::Value, keys: &[&str]) -> Vec<String> {
        value
            .as_array()
            .or_else(|| {
                value
                    .get("portfolios")
                    .and_then(serde_json::Value::as_array)
            })
            .or_else(|| value.get("accounts").and_then(serde_json::Value::as_array))
            .or_else(|| value.get("data").and_then(serde_json::Value::as_array))
            .into_iter()
            .flatten()
            .filter_map(|row| {
                keys.iter()
                    .find_map(|k| row.get(*k).and_then(serde_json::Value::as_str))
            })
            .map(str::to_owned)
            .collect()
    }
    match provider {
        "coinbase" => {
            let permissions = read(client, E::KeyPermissions).await?;
            if permissions
                .get("can_view")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            {
                return Err("Coinbase key lacks account read permission".into());
            }
            let id = permissions
                .get("portfolio_uuid")
                .and_then(serde_json::Value::as_str)
                .ok_or("Coinbase permission response did not identify its portfolio")?
                .to_owned();
            let portfolios = read(client, E::Portfolios).await?;
            let ids = strings(&portfolios, &["portfolio_uuid", "uuid", "id"]);
            if !ids.iter().any(|seen| seen == &id) {
                return Err("Coinbase permissioned portfolio was not present in the authenticated portfolio list".into());
            }
            Ok((id.clone(), id))
        }
        "coinbaseexchange" => {
            let accounts = read(client, E::ExchangeAccounts).await?;
            let mut ids = strings(&accounts, &["profile_id"]);
            ids.sort();
            ids.dedup();
            let id = selector
                .filter(|selected| ids.iter().any(|known| known == selected))
                .map(str::to_owned)
                .or_else(|| (ids.len() == 1).then(|| ids[0].clone()))
                .ok_or("Coinbase Exchange returned no profile identity")?;
            Ok((id.clone(), id))
        }
        "coinbaseinternational" => {
            let portfolios = read(client, E::InternationalAccounts).await?;
            let mut ids = strings(&portfolios, &["portfolio_id", "id"]);
            ids.sort();
            ids.dedup();
            let id = selector
                .filter(|selected| ids.iter().any(|known| known == selected))
                .map(str::to_owned)
                .or_else(|| (ids.len() == 1).then(|| ids[0].clone()))
                .ok_or("Coinbase International returned no portfolio identity")?;
            Ok((id.clone(), id))
        }
        _ => Err("unsupported Coinbase product".into()),
    }
}

fn classify_binance_pem(api_key: String, private_key: String) -> Result<SecretMaterial, String> {
    use ed25519_dalek::pkcs8::DecodePrivateKey as _;
    if ed25519_dalek::SigningKey::from_pkcs8_pem(&private_key).is_ok() {
        return Ok(SecretMaterial::BinanceEd25519 {
            api_key,
            private_key,
        });
    }
    use rsa::{RsaPrivateKey, pkcs1::DecodeRsaPrivateKey};
    if RsaPrivateKey::from_pkcs8_pem(&private_key).is_ok()
        || RsaPrivateKey::from_pkcs1_pem(&private_key).is_ok()
    {
        return Ok(SecretMaterial::BinanceRsa {
            api_key,
            private_key,
        });
    }
    Err("Binance PEM is not a supported PKCS8 RSA or Ed25519 private key".into())
}

/// Development-only fork selection. A fork profile must cover every explicitly
/// selected chain with exactly one numeric loopback HTTP endpoint and no fallback.
/// These URLs never enter the production-validated BrokerConfigV2.
pub fn fork_rpc_urls(config: &EnvironmentConfig) -> Result<Option<BTreeMap<u64, String>>, String> {
    let has_http = config
        .rpc_urls
        .values()
        .flatten()
        .any(|text| reqwest::Url::parse(text).is_ok_and(|url| url.scheme() == "http"));
    if !has_http {
        return Ok(None);
    }
    let selected = config
        .reviewed_chain_ids
        .as_ref()
        .ok_or("owned fork transport requires an explicit reviewed development chain scope")?;
    if selected.is_empty()
        || selected
            .iter()
            .any(|chain| !gofin_networks::supports_chain(*chain))
        || selected
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != selected.len()
        || config
            .rpc_urls
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            != selected
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
    {
        return Err("owned fork endpoints must exactly cover the selected reviewed chains".into());
    }
    let mut endpoints = BTreeMap::new();
    for (chain, urls) in &config.rpc_urls {
        let [text] = urls.as_slice() else {
            return Err("owned fork transport requires exactly one endpoint per chain".into());
        };
        let url = reqwest::Url::parse(text).map_err(|_| "owned fork endpoint is invalid")?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
            || url.port().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err("owned fork endpoints require numeric loopback HTTP with an explicit port and no credentials, path, query or fragment".into());
        }
        endpoints.insert(*chain, text.clone());
    }
    Ok(Some(endpoints))
}

pub fn broker_config(config: &EnvironmentConfig) -> Result<BrokerConfigV2, String> {
    let mut connections = Vec::new();
    for (provider, name, reference, identity) in [
        (
            "coinbase",
            "dev-coinbase",
            config.credentials.coinbase.as_ref(),
            config.coinbase_portfolio_id.as_ref(),
        ),
        (
            "coinbaseexchange",
            "dev-coinbaseexchange",
            config.credentials.coinbaseexchange.as_ref(),
            config.coinbaseexchange_portfolio_id.as_ref(),
        ),
        (
            "coinbaseinternational",
            "dev-coinbaseinternational",
            config.credentials.coinbaseinternational.as_ref(),
            config.coinbaseinternational_portfolio_id.as_ref(),
        ),
    ] {
        if let Some(reference) = reference {
            let identity = identity.ok_or("Coinbase portfolio identity is missing")?;
            let resource = ResourceId(format!("{provider}:portfolio:{identity}"));
            connections.push(match provider {
                "coinbase" => ConnectionConfig::Coinbase {
                    name: name.into(),
                    resource,
                    credential_ref: reference.clone(),
                    physical_identity: identity.clone(),
                    physical_identity_verified: true,
                    portfolio_id: Some(identity.clone()),
                },
                "coinbaseexchange" => ConnectionConfig::CoinbaseExchange {
                    name: name.into(),
                    resource,
                    credential_ref: reference.clone(),
                    physical_identity: identity.clone(),
                    physical_identity_verified: true,
                    portfolio_id: Some(identity.clone()),
                },
                _ => ConnectionConfig::CoinbaseInternational {
                    name: name.into(),
                    resource,
                    credential_ref: reference.clone(),
                    physical_identity: identity.clone(),
                    physical_identity_verified: true,
                    portfolio_id: Some(identity.clone()),
                },
            });
        }
    }
    if let Some(reference) = &config.credentials.binance {
        let uid = config
            .binance_uid
            .as_deref()
            .ok_or("Binance account UID is missing")?;
        connections.push(ConnectionConfig::Binance {
            name: "dev-binance".into(),
            resource: ResourceId(format!("binance:uid:{uid}:SPOT")),
            credential_ref: reference.clone(),
            role: "main".into(),
            parent: None,
            email: None,
            wallet_type: "SPOT".into(),
            physical_identity: uid.into(),
            physical_identity_verified: true,
        });
    }
    if let Some(reference) = &config.credentials.binance_sub {
        let uid = config
            .binance_sub_uid
            .as_deref()
            .ok_or("Binance subaccount UID is missing")?;
        let email = config
            .binance_sub_email
            .as_deref()
            .ok_or("Binance subaccount email is missing")?;
        if config.credentials.binance.is_none() || config.binance_uid.is_none() {
            return Err("Binance subaccount profile requires its main account".into());
        }
        connections.push(ConnectionConfig::Binance {
            name: "dev-binance-sub".into(),
            resource: ResourceId(format!("binance:uid:{uid}:SPOT")),
            credential_ref: reference.clone(),
            role: "sub".into(),
            parent: Some("dev-binance".into()),
            email: Some(email.into()),
            wallet_type: "SPOT".into(),
            physical_identity: uid.into(),
            physical_identity_verified: true,
        });
    }
    for (reference, address, name) in [
        (
            config.credentials.evm_primary.as_ref(),
            config.evm_addresses.first(),
            "dev-wallet",
        ),
        (
            config.credentials.evm_secondary.as_ref(),
            config.evm_addresses.get(1),
            "dev-wallet-secondary",
        ),
    ] {
        if let Some(address) = address {
            if address.is_empty() {
                return Err("EVM wallet address is missing".into());
            }
            connections.push(ConnectionConfig::Evm {
                name: name.into(),
                resource: ResourceId(format!("evm:{}", address.to_ascii_lowercase())),
                // A public recipient can be observed without importing its key.
                // This grants no signing authority to the development broker.
                credential_ref: reference.cloned(),
                address: address.clone(),
                signing_enabled: reference.is_some(),
            });
        }
    }
    for (provider, reference, identity) in [
        (
            "kraken",
            config.credentials.kraken.as_ref(),
            config.kraken_account_id.as_ref(),
        ),
        (
            "krakenfutures",
            config.credentials.krakenfutures.as_ref(),
            config.krakenfutures_account_id.as_ref(),
        ),
    ] {
        if let Some(reference) = reference {
            let identity = identity.ok_or("Kraken account identity is missing")?;
            let resource = ResourceId(format!("{provider}:account:{identity}"));
            connections.push(if provider == "kraken" {
                ConnectionConfig::Kraken {
                    default_wallet_account_id: config.kraken_default_wallet_account_id.clone(),
                    name: "dev-kraken".into(),
                    resource,
                    credential_ref: reference.clone(),
                    physical_identity: identity.clone(),
                    physical_identity_verified: false,
                }
            } else {
                ConnectionConfig::KrakenFutures {
                    name: "dev-krakenfutures".into(),
                    resource,
                    credential_ref: reference.clone(),
                    physical_identity: identity.clone(),
                    physical_identity_verified: false,
                }
            });
        }
    }
    validate_kraken_group_identities(config)?;
    for group in config.kraken_accounts.values() {
        let resource = ResourceId(format!("{}:account:{}", group.product, group.account_id));
        connections.push(if group.product == "kraken" {
            ConnectionConfig::Kraken {
                name: group.name.clone(),
                resource,
                credential_ref: group.credential_ref.clone(),
                physical_identity: group.account_id.clone(),
                physical_identity_verified: false,
                default_wallet_account_id: group.default_wallet_account_id.clone(),
            }
        } else {
            ConnectionConfig::KrakenFutures {
                name: group.name.clone(),
                resource,
                credential_ref: group.credential_ref.clone(),
                physical_identity: group.account_id.clone(),
                physical_identity_verified: false,
            }
        });
    }
    if let (Some(reference), Some(address)) = (
        config.credentials.solana.as_ref(),
        config.solana_address.as_ref(),
    ) {
        connections.push(ConnectionConfig::Solana {
            name: "dev-solana-wallet".into(),
            resource: ResourceId(format!("solana:mainnet-beta:{address}")),
            address: address.clone(),
            credential_ref: Some(reference.clone()),
            signing_enabled: true,
            network: "solana:mainnet-beta".into(),
        });
    }
    if let Some(reference) = &config.credentials.relay {
        connections.push(ConnectionConfig::Relay {
            name: "dev-relay-api".into(),
            resource: ResourceId("relay:service".into()),
            credential_ref: reference.clone(),
        });
    }
    if let (Some(reference), Some(address)) = (
        config.credentials.evm_primary.as_ref(),
        config.evm_addresses.first(),
    ) && !address.is_empty()
    {
        connections.push(ConnectionConfig::Hyperliquid {
            name: "dev-hyperliquid".into(),
            resource: ResourceId(format!(
                "hyperliquid:mainnet:{}:account:primary",
                address.to_ascii_lowercase()
            )),
            owner: address.to_ascii_lowercase(),
            credential_ref: Some(reference.clone()),
            signing_enabled: true,
            execution_scope: gofin_hyperliquid::HyperliquidExecutionScope::default(),
            auto_derived: false,
        });
    }
    let reviewed_chain_ids: Vec<u64> = gofin_networks::all()
        .iter()
        .map(|network| network.chain_id)
        .collect();
    let selected_chain_ids = config
        .reviewed_chain_ids
        .as_deref()
        .unwrap_or(&reviewed_chain_ids);
    if selected_chain_ids.is_empty()
        || selected_chain_ids
            .iter()
            .any(|chain_id| !reviewed_chain_ids.contains(chain_id))
        || selected_chain_ids
            .iter()
            .enumerate()
            .any(|(index, chain_id)| selected_chain_ids[..index].contains(chain_id))
    {
        return Err(
            "development chain scope must be a nonempty unique subset of reviewed chains".into(),
        );
    }
    let mut networks: Vec<_> = NetworkConfig::reviewed_evm_networks()
        .into_iter()
        .filter(|network| selected_chain_ids.contains(&network.chain_id))
        .collect();
    for chain_id in config.rpc_urls.keys() {
        if !gofin_networks::supports_chain(*chain_id) {
            return Err("development RPC override names an unsupported chain".into());
        }
    }
    let fork_endpoints = fork_rpc_urls(config)?;
    for network in &mut networks {
        if let Some(rpc_urls) = config.rpc_urls.get(&network.chain_id) {
            if rpc_urls.is_empty() {
                return Err("development RPC override must include a public HTTPS endpoint".into());
            }
            if fork_endpoints.is_none() {
                network.rpc_urls = rpc_urls.clone();
            }
        }
    }
    let mut policy = config.policy.clone().unwrap_or_else(BrokerPolicy::beta);
    if let Some(limits) = &config.policy_limits {
        limits.validate()?;
        let movement = limits
            .max_movement_usd_micros
            .as_str()
            .parse::<u64>()
            .map_err(|_| "development movement ceiling is invalid")?;
        let allocation = limits
            .max_allocation_usd_micros
            .as_str()
            .parse::<u64>()
            .map_err(|_| "development allocation ceiling is invalid")?;
        let current_movement = policy
            .max_movement_usd_micros
            .parse::<u64>()
            .map_err(|_| "development movement policy is invalid")?;
        let current_allocation = policy
            .max_run_allocation_usd_micros
            .parse::<u64>()
            .map_err(|_| "development allocation policy is invalid")?;
        policy.max_movement_usd_micros = if current_movement == 0 {
            movement
        } else {
            current_movement.min(movement)
        }
        .to_string();
        policy.max_run_allocation_usd_micros = if current_allocation == 0 {
            allocation
        } else {
            current_allocation.min(allocation)
        }
        .to_string();
    }
    let runtime_config = BrokerConfigV2 {
        version: 2,
        networks,
        deployments: DeploymentConfig::reviewed_amm_networks()
            .into_iter()
            .filter(|deployment| {
                gofin_networks::all().iter().any(|network| {
                    network.network_id == deployment.network
                        && selected_chain_ids.contains(&network.chain_id)
                })
            })
            .collect(),
        connections,
        policy,
        approver_uid: unsafe { libc::geteuid() },
        extensions: BTreeMap::new(),
    };
    runtime_config
        .validate()
        .map_err(|_| "development connection profile is invalid".to_owned())?;
    Ok(runtime_config)
}

fn validate_kraken_group_identities(config: &EnvironmentConfig) -> Result<(), String> {
    if config.kraken_accounts.len() > 16 {
        return Err("Indexed Kraken account groups exceed the supported bound".into());
    }
    let mut identities = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    for (product, id, name) in [
        ("kraken", config.kraken_account_id.as_deref(), "dev-kraken"),
        (
            "krakenfutures",
            config.krakenfutures_account_id.as_deref(),
            "dev-krakenfutures",
        ),
    ] {
        if let Some(id) = id {
            identities.insert((product, id));
            names.insert(name);
        }
    }
    for (slot, group) in &config.kraken_accounts {
        if !matches!(group.product.as_str(), "kraken" | "krakenfutures")
            || !(1..=8).any(|index| slot == &format!("{}_{index}", group.product))
        {
            return Err("Indexed Kraken product or slot is invalid".into());
        }
        if !identities.insert((&group.product, &group.account_id)) {
            return Err(
                "Kraken credential groups cannot duplicate an existing product custody identity"
                    .into(),
            );
        }
        if !names.insert(&group.name) {
            return Err("Kraken credential groups need distinct connection names".into());
        }
    }
    Ok(())
}

fn canonical_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() <= 20
        && uid.bytes().all(|c| c.is_ascii_digit())
        && !uid.starts_with('0')
        && uid.parse::<u64>().is_ok_and(|n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    const ED25519_PEM: &str = "-----BEGIN PRIVATE KEY-----\\nMC4CAQAwBQYDK2VwBCIEIGyctEVql3srd/k3DvetsRG/NYWQTmkdCHWRv4pKWzst\\n-----END PRIVATE KEY-----";
    const RSA_PEM: &str = "-----BEGIN PRIVATE KEY-----\\nMIICdgIBADANBgkqhkiG9w0BAQEFAASCAmAwggJcAgEAAoGBAKtmufoMN/K7mD5r\\n3qnIp9uiI4zEhKT2WWEr2Jdv11s9oULC2+JHeZz3HIBtfQy2w2L0DrCoFW622rQ4\\n1jOkMnRmn0UBdLE9WuWujk94Z36512HyciJLcRS+eR1RDdr/9ahV51pEnK+AYfFN\\n2MePxwFGHDegm0O8c1qhS0EfsFnZAgMBAAECgYA42CXftsOrXenc8dtzxZdd8QUH\\nP3maWBVGc8suVio/GU5tHMDd58R+eTnbofQyDj74h2aD0grhoAUidiMcF12f0AVQ\\nfARoAAvttb5PRjMHa3ob0XmF7Elgw3jUmD0R8buuSlEsl0c04TppLhfkFP0eOHTN\\nJpsU/3PQMo42e2qm7QJBANpNYN19LHYqSM+JyA7O8fZMEMqnuOfXDUXvSzjtOPPt\\n7Ub5k9vrwYndlWqDmBTRAxWzpf2cq0Uxs1FYJ34Ink8CQQDI//ezQx962yES75lM\\nMZ/TrJZJbOBB3YGQ2nouQD5evcxlWh/G4lNbT8+4lrSriDGYJqOR9d9gVpZuA74J\\nVmNXAkEAqXoPLITkQqybMDw4S37QDDWLh7lkbODg1h1rmbtZTs61OjfCWacS7vRO\\n7o1RQT/vFCcPQ/AgayylmQ5GQqtwMwJAVGDgIyjydii3YbOjrxDF/55cGMbT/dJ3\\nKdANqozdofgbbhC/dkZcp+CBWwQpJDkwWKcf045cD51J2TVI+bKCUQJAUq54aKDL\\nWdtcSlCtJK5LBmQXf7xm/v6FdrEIAIQWL/1TjctFjTgYki8a0G+Fu3F6udRhBIGA\\nL/dD1B/ct/MoHA==\\n-----END PRIVATE KEY-----";

    fn custom_development_policy() -> BrokerPolicy {
        BrokerPolicy {
            require_run_for_writes: false,
            max_movement_usd_micros: "100000000".into(),
            max_run_allocation_usd_micros: "200000000".into(),
            preview_ttl_seconds: 90,
            allow_unpriced_exchange_assets: false,
        }
    }

    fn limits(movement: &str, allocation: &str) -> PolicyLimits {
        PolicyLimits {
            max_movement_usd_micros: Quantity::new(movement).unwrap(),
            max_allocation_usd_micros: Quantity::new(allocation).unwrap(),
        }
    }

    #[test]
    fn legacy_environment_defaults_to_beta_policy_and_saved_policy_is_used() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut legacy = serde_json::to_value(EnvironmentConfig::empty("env")).unwrap();
        legacy["format"] = serde_json::json!("fin-dev-synthetic-config");
        legacy.as_object_mut().unwrap().remove("policy");
        legacy.as_object_mut().unwrap().remove("rpc_urls");
        legacy.as_object_mut().unwrap().remove("reviewed_chain_ids");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let loaded = EnvironmentConfig::read(&path, "env").unwrap();
        assert!(loaded.policy.is_none());
        assert!(loaded.rpc_urls.is_empty());
        assert_eq!(loaded.reviewed_chain_ids, None);
        assert_eq!(broker_config(&loaded).unwrap().policy, BrokerPolicy::beta());

        let mut configured = loaded;
        configured.policy = Some(custom_development_policy());
        configured.write_atomic(&path).unwrap();
        let loaded = EnvironmentConfig::read(&path, "env").unwrap();
        let runtime_config = broker_config(&loaded).unwrap();
        assert_eq!(runtime_config.policy, custom_development_policy());
        assert!(runtime_config.validate().is_ok());
    }

    #[test]
    fn version_one_profile_migrates_additively_without_losing_saved_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut legacy = serde_json::to_value(EnvironmentConfig::empty("env")).unwrap();
        legacy["format"] = serde_json::json!("fin-dev-config");
        legacy["version"] = serde_json::json!(1);
        legacy.as_object_mut().unwrap().remove("solana_address");
        legacy["credentials"]
            .as_object_mut()
            .unwrap()
            .remove("solana");
        legacy["credentials"]
            .as_object_mut()
            .unwrap()
            .remove("jupiter");
        legacy["credentials"]["evm_primary"] =
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000");
        legacy["evm_addresses"] = serde_json::json!(["0x1111111111111111111111111111111111111111"]);
        legacy["policy_limits"] = serde_json::to_value(limits("123", "456")).unwrap();
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let migrated = EnvironmentConfig::read(&path, "env").unwrap();
        assert_eq!(migrated.version, 2);
        assert_eq!(migrated.format, "gofin-dev-config");
        assert_eq!(
            migrated.credentials.evm_primary.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            migrated.evm_addresses,
            ["0x1111111111111111111111111111111111111111"]
        );
        assert_eq!(migrated.policy_limits, Some(limits("123", "456")));
        assert!(migrated.credentials.solana.is_none());
        assert!(migrated.solana_address.is_none());
    }

    #[test]
    fn interrupted_version_one_upgrade_restarts_without_rewriting_pending_journal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        let journal_path = dir.path().join("operations.sqlite");
        let pending_journal = b"synthetic-pending-submission:keep-original-identity";

        let mut legacy = EnvironmentConfig::empty("env");
        legacy.version = 1;
        legacy.credentials = CredentialReferences {
            kraken: Some("550e8400-e29b-41d4-a716-446655440007".into()),
            krakenfutures: Some("550e8400-e29b-41d4-a716-446655440008".into()),
            binance: Some("550e8400-e29b-41d4-a716-446655440001".into()),
            binance_sub: Some("550e8400-e29b-41d4-a716-446655440002".into()),
            evm_primary: Some("550e8400-e29b-41d4-a716-446655440003".into()),
            evm_secondary: Some("550e8400-e29b-41d4-a716-446655440004".into()),
            solana: Some("550e8400-e29b-41d4-a716-446655440005".into()),
            jupiter: Some("550e8400-e29b-41d4-a716-446655440006".into()),
            relay: None,
            ..CredentialReferences::default()
        };
        legacy.kraken_accounts.insert(
            "kraken_1".into(),
            KrakenDevelopmentAccount {
                product: "kraken".into(),
                name: "primary spot account".into(),
                account_id: "spot-account".into(),
                credential_ref: "550e8400-e29b-41d4-a716-446655440009".into(),
                default_wallet_account_id: Some("wallet-default".into()),
            },
        );
        legacy.policy = Some(custom_development_policy());
        legacy
            .rpc_urls
            .insert(1, vec!["https://rpc.example.invalid".into()]);
        legacy.reviewed_chain_ids = Some(vec![1]);
        legacy.binance_uid = Some("100".into());
        legacy.binance_sub_uid = Some("200".into());
        legacy.binance_sub_email = Some("child@example.invalid".into());
        legacy.binance_sub_email_matched_by_venue_id = true;
        legacy.evm_addresses = vec!["0x1111111111111111111111111111111111111111".into()];
        legacy.solana_address = Some("11111111111111111111111111111111".into());
        legacy.policy_limits = Some(limits("123", "456"));
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        fs::write(&journal_path, pending_journal).unwrap();

        // Simulate termination after a partial next-version temp file was
        // written but before the atomic rename. The legacy profile remains the
        // restart source; the orphan is never interpreted as committed state.
        let interrupted_temp = dir.path().join(".config.interrupted.tmp");
        fs::write(&interrupted_temp, b"{\"version\":2,").unwrap();
        let restarted = EnvironmentConfig::read(&path, "env").unwrap();
        assert_eq!(restarted.version, 2);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap()["version"],
            1
        );
        assert_eq!(fs::read(&journal_path).unwrap(), pending_journal);

        restarted.write_atomic(&path).unwrap();
        assert!(interrupted_temp.exists());
        let recovered = EnvironmentConfig::read(&path, "env").unwrap();
        assert_eq!(recovered.version, 2);
        assert_eq!(
            recovered.credentials.binance.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440001")
        );
        assert_eq!(
            recovered.credentials.binance_sub.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440002")
        );
        assert_eq!(
            recovered.credentials.evm_primary.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440003")
        );
        assert_eq!(
            recovered.credentials.evm_secondary.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440004")
        );
        assert_eq!(
            recovered.credentials.solana.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440005")
        );
        assert_eq!(
            recovered.credentials.kraken.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440007")
        );
        assert_eq!(
            recovered.credentials.krakenfutures.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440008")
        );
        assert_eq!(
            recovered
                .kraken_accounts
                .get("kraken_1")
                .unwrap()
                .credential_ref,
            "550e8400-e29b-41d4-a716-446655440009"
        );
        assert_eq!(
            recovered
                .kraken_accounts
                .get("kraken_1")
                .unwrap()
                .default_wallet_account_id
                .as_deref(),
            Some("wallet-default")
        );
        assert_eq!(
            recovered.credentials.jupiter.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440006")
        );
        assert_eq!(recovered.policy, Some(custom_development_policy()));
        assert_eq!(
            recovered.rpc_urls.get(&1),
            Some(&vec!["https://rpc.example.invalid".into()])
        );
        assert_eq!(recovered.reviewed_chain_ids, Some(vec![1]));
        assert_eq!(recovered.binance_uid.as_deref(), Some("100"));
        assert_eq!(recovered.binance_sub_uid.as_deref(), Some("200"));
        assert_eq!(
            recovered.binance_sub_email.as_deref(),
            Some("child@example.invalid")
        );
        assert!(recovered.binance_sub_email_matched_by_venue_id);
        assert_eq!(
            recovered.evm_addresses,
            ["0x1111111111111111111111111111111111111111"]
        );
        assert_eq!(
            recovered.solana_address.as_deref(),
            Some("11111111111111111111111111111111")
        );
        assert_eq!(recovered.policy_limits, Some(limits("123", "456")));
        assert_eq!(fs::read(&journal_path).unwrap(), pending_journal);
    }

    #[test]
    fn owned_fork_transport_preserves_production_validated_profiles() {
        let mut config = EnvironmentConfig::empty("env");
        config.reviewed_chain_ids = Some(vec![1]);
        for endpoint in ["http://127.0.0.1:8545", "http://[::1]:8545"] {
            config.rpc_urls.insert(1, vec![endpoint.into()]);
            assert_eq!(fork_rpc_urls(&config).unwrap().unwrap()[&1], endpoint);
            let runtime = broker_config(&config).unwrap();
            runtime.validate().unwrap();
            assert_eq!(runtime.networks.len(), 1);
            assert_eq!(runtime.networks[0], NetworkConfig::ethereum_mainnet());
            assert!(gofin_evm::HttpRpc::new(runtime.networks[0].rpc_urls.clone(), 1).is_ok());
        }
    }

    #[tokio::test]
    async fn solana_only_import_preserves_evm_profile_and_refuses_wallet_rebinding() {
        use ed25519_dalek::SigningKey;
        use std::os::unix::fs::PermissionsExt;

        fn keypair(seed: u8) -> String {
            let signing = SigningKey::from_bytes(&[seed; 32]);
            let mut pair = [0u8; 64];
            pair[..32].copy_from_slice(&[seed; 32]);
            pair[32..].copy_from_slice(&signing.verifying_key().to_bytes());
            format!(
                "[{}]",
                pair.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
            )
        }

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let env_path = dir.path().join("selected.env");
        let mut old = EnvironmentConfig::empty("env");
        old.credentials.binance = Some("550e8400-e29b-41d4-a716-446655440001".into());
        old.binance_uid = Some("123".into());
        old.credentials.evm_primary = Some("550e8400-e29b-41d4-a716-446655440000".into());
        old.evm_addresses
            .push("0x1111111111111111111111111111111111111111".into());
        old.policy = Some(custom_development_policy());
        old.write_atomic(&config_path).unwrap();
        fs::write(
            &env_path,
            format!(
                "GOFIN_DEV_BINANCE_API_KEY=$(ignored)\nGOFIN_DEV_SOLANA_PRIVATE_KEY={}\n",
                keypair(7)
            ),
        )
        .unwrap();
        fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600)).unwrap();

        import(
            &config_path,
            "env",
            &env_path,
            &credential_dir,
            false,
            Some("solana"),
        )
        .await
        .unwrap();
        let migrated = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(
            migrated.credentials.evm_primary.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            migrated.credentials.binance.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440001")
        );
        assert_eq!(migrated.binance_uid.as_deref(), Some("123"));
        assert!(migrated.credentials.solana.is_some());
        assert_eq!(migrated.evm_addresses, old.evm_addresses);
        assert_eq!(migrated.policy, old.policy);
        let runtime = broker_config(&migrated).unwrap();
        assert!(runtime.connections.iter().any(|connection| matches!(connection, ConnectionConfig::Evm { name, .. } if name == "dev-wallet")));
        assert!(runtime.connections.iter().any(|connection| matches!(connection, ConnectionConfig::Solana { name, .. } if name == "dev-solana-wallet")));

        fs::write(&env_path, format!("SOLANA_PRIVATE_KEY={}\n", keypair(8))).unwrap();
        fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            import(
                &config_path,
                "env",
                &env_path,
                &credential_dir,
                false,
                Some("solana")
            )
            .await
            .is_err()
        );
        let after = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(after.credentials.solana, migrated.credentials.solana);
        assert_eq!(
            after.credentials.evm_primary,
            migrated.credentials.evm_primary
        );
        assert_eq!(after.credentials.binance, migrated.credentials.binance);
    }

    #[test]
    fn owned_fork_transport_rejects_ambiguous_endpoints_and_public_fallback() {
        let mut config = EnvironmentConfig::empty("env");
        config.reviewed_chain_ids = Some(vec![1]);
        for endpoint in [
            "http://localhost:8545",
            "http://0.0.0.0:8545",
            "http://example.invalid:8545",
            "http://127.0.0.1",
            "http://127.0.0.1:8545/path",
            "http://user:secret@127.0.0.1:8545",
            "http://127.0.0.1:8545?key=secret",
            "http://127.0.0.1:8545#fragment",
        ] {
            config.rpc_urls.insert(1, vec![endpoint.into()]);
            assert!(fork_rpc_urls(&config).is_err(), "{endpoint}");
            assert!(broker_config(&config).is_err(), "{endpoint}");
        }
        config.rpc_urls.insert(
            1,
            vec![
                "http://127.0.0.1:8545".into(),
                "https://eth.drpc.org".into(),
            ],
        );
        assert!(fork_rpc_urls(&config).is_err());
        config
            .rpc_urls
            .insert(1, vec!["http://127.0.0.1:8545".into()]);
        config
            .rpc_urls
            .insert(42161, vec!["https://arb1.arbitrum.io/rpc".into()]);
        assert!(fork_rpc_urls(&config).is_err());
        config.rpc_urls.remove(&42161);
        config.reviewed_chain_ids = Some(vec![1, 42161]);
        assert!(fork_rpc_urls(&config).is_err());
        config.reviewed_chain_ids = None;
        assert!(fork_rpc_urls(&config).is_err());
    }

    #[test]
    fn development_rpc_overrides_are_chain_scoped_and_validated() {
        let mut config = EnvironmentConfig::empty("env");
        let defaults = NetworkConfig::reviewed_evm_networks();
        let runtime = broker_config(&config).unwrap();
        assert_eq!(runtime.networks, defaults);

        config.rpc_urls.insert(
            1,
            vec![
                "https://eth.drpc.org".into(),
                "https://ethereum-rpc.publicnode.com".into(),
            ],
        );
        let runtime = broker_config(&config).unwrap();
        assert_eq!(runtime.networks[0].rpc_urls, config.rpc_urls[&1]);
        assert_eq!(runtime.networks[1], defaults[1]);
        assert_eq!(runtime.networks[2], defaults[2]);
        assert_eq!(runtime.networks[3], defaults[3]);
        config
            .rpc_urls
            .insert(8453, vec!["https://base-rpc.publicnode.com".into()]);
        let runtime = broker_config(&config).unwrap();
        let base = runtime
            .networks
            .iter()
            .find(|network| network.chain_id == 8453)
            .unwrap();
        assert_eq!(base.rpc_urls, config.rpc_urls[&8453]);
        assert_eq!(base.id, "ethereum:8453");
        assert_eq!(base.confirmations, 1);
        assert_eq!(runtime.networks[0].rpc_urls, config.rpc_urls[&1]);
        config.rpc_urls.insert(8453, Vec::new());
        assert!(broker_config(&config).is_err());
        config.rpc_urls.remove(&8453);

        config.rpc_urls.insert(42161, Vec::new());
        assert!(broker_config(&config).is_err());
        config.rpc_urls.remove(&42161);

        config
            .rpc_urls
            .insert(1, vec!["https://user:secret@example.invalid".into()]);
        assert!(broker_config(&config).is_err());
        config
            .rpc_urls
            .insert(31337, vec!["https://rpc.example.invalid".into()]);
        assert!(broker_config(&config).is_err());
    }

    #[test]
    fn legacy_profile_defaults_to_beta_limits_and_keeps_connections() {
        let mut legacy = serde_json::to_value(EnvironmentConfig::empty("env")).unwrap();
        legacy.as_object_mut().unwrap().remove("policy_limits");
        legacy["credentials"]["binance"] =
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000");
        legacy["binance_uid"] = serde_json::json!("100");
        let parsed: EnvironmentConfig = serde_json::from_value(legacy).unwrap();
        assert!(parsed.policy_limits.is_none());

        let config = broker_config(&parsed).unwrap();
        assert_eq!(
            config.policy.max_movement_usd_micros,
            gofin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS.to_string()
        );
        assert_eq!(
            config.policy.max_run_allocation_usd_micros,
            gofin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS.to_string()
        );
        assert!(matches!(
            config.connections.as_slice(),
            [ConnectionConfig::Binance { credential_ref, .. }]
                if credential_ref == "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn policy_limits_persist_and_tighten_only_the_development_broker_policy() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let mut profile = EnvironmentConfig::empty("env");
        profile.credentials.binance = Some("550e8400-e29b-41d4-a716-446655440000".into());
        profile.binance_uid = Some("100".into());
        profile.policy_limits = Some(limits("10000000", "100000000"));
        profile.write_atomic(&config_path).unwrap();

        let restored = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(restored.policy_limits, profile.policy_limits);
        assert_eq!(restored.credentials.binance, profile.credentials.binance);
        let broker = broker_config(&restored).unwrap();
        assert_eq!(broker.policy.max_movement_usd_micros, "10000000");
        assert_eq!(broker.policy.max_run_allocation_usd_micros, "100000000");
        assert_eq!(broker.connections.len(), 1);
    }

    #[test]
    fn zero_or_above_default_environment_policy_limits_fail_closed() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        for candidate in [limits("0", "100000000"), limits("10000000", "100000001")] {
            let mut profile = EnvironmentConfig::empty("env");
            profile.policy_limits = Some(candidate);
            profile.write_atomic(&config_path).unwrap();
            assert!(EnvironmentConfig::read(&config_path, "env").is_err());
            assert!(broker_config(&profile).is_err());
        }
    }

    #[test]
    fn reviewed_chain_scope_limits_networks_and_deployments_without_changing_credentials() {
        let mut config = EnvironmentConfig::empty("env");
        config.mode = "live".into();
        let reference = uuid::Uuid::from_u128(1).hyphenated().to_string();
        config.credentials.evm_primary = Some(reference.clone());
        config.evm_addresses = vec!["0x1111111111111111111111111111111111111111".into()];
        config.reviewed_chain_ids = Some(vec![42161]);
        // An unused override does not configure or initialize an out-of-scope
        // network, even if it is unusable. The selected chain remains reviewed.
        config.rpc_urls.insert(1, Vec::new());

        let runtime = broker_config(&config).unwrap();
        assert_eq!(runtime.networks.len(), 1);
        assert_eq!(runtime.networks[0].chain_id, 42161);
        let expected_deployments: Vec<_> = DeploymentConfig::reviewed_amm_networks()
            .into_iter()
            .filter(|deployment| deployment.network == "ethereum:42161")
            .map(|deployment| deployment.network)
            .collect();
        assert!(!expected_deployments.is_empty());
        let selected_deployments: Vec<_> = runtime
            .deployments
            .iter()
            .map(|deployment| deployment.network.clone())
            .collect();
        assert_eq!(selected_deployments, expected_deployments);
        let wallet = runtime
            .connections
            .iter()
            .find_map(|connection| match connection {
                ConnectionConfig::Evm {
                    name,
                    credential_ref,
                    ..
                } if name == "dev-wallet" => credential_ref.as_deref(),
                _ => None,
            });
        let hyperliquid = runtime
            .connections
            .iter()
            .find_map(|connection| match connection {
                ConnectionConfig::Hyperliquid {
                    name,
                    credential_ref,
                    ..
                } if name == "dev-hyperliquid" => credential_ref.as_deref(),
                _ => None,
            });
        assert_eq!(wallet, Some(reference.as_str()));
        assert_eq!(hyperliquid, Some(reference.as_str()));
    }

    #[test]
    fn reviewed_chain_scope_rejects_empty_duplicate_and_unreviewed_ids() {
        let mut config = EnvironmentConfig::empty("env");
        for invalid in [vec![], vec![42161, 42161], vec![31337]] {
            config.reviewed_chain_ids = Some(invalid);
            assert!(broker_config(&config).is_err());
        }
    }

    #[test]
    fn development_policy_limits_tighten_custom_policy_and_rpc_override_is_preserved() {
        let mut profile = EnvironmentConfig::empty("env");
        profile.policy = Some(custom_development_policy());
        profile.policy_limits = Some(limits("50000000", "100000000"));
        profile
            .rpc_urls
            .insert(1, vec!["https://eth.drpc.org".into()]);
        let runtime = broker_config(&profile).unwrap();
        assert_eq!(runtime.policy.max_movement_usd_micros, "50000000");
        assert_eq!(runtime.policy.max_run_allocation_usd_micros, "100000000");
        assert_eq!(runtime.networks[0].rpc_urls, profile.rpc_urls[&1]);

        profile.policy = Some(BrokerPolicy {
            max_movement_usd_micros: "0".into(),
            max_run_allocation_usd_micros: "0".into(),
            ..custom_development_policy()
        });
        profile.policy_limits = Some(limits("10000000", "20000000"));
        let runtime = broker_config(&profile).unwrap();
        assert_eq!(runtime.policy.max_movement_usd_micros, "10000000");
        assert_eq!(runtime.policy.max_run_allocation_usd_micros, "20000000");
    }

    #[test]
    fn primary_evm_dev_connection_also_enrolls_hyperliquid_with_same_signer_reference() {
        let mut config = EnvironmentConfig::empty("env");
        let reference = uuid::Uuid::from_u128(1).hyphenated().to_string();
        config.credentials.evm_primary = Some(reference.clone());
        config.evm_addresses = vec!["0x1111111111111111111111111111111111111111".into()];
        let broker = broker_config(&config).unwrap();
        let wallet_ref = broker
            .connections
            .iter()
            .find_map(|connection| match connection {
                ConnectionConfig::Evm {
                    name,
                    credential_ref,
                    ..
                } if name == "dev-wallet" => credential_ref.as_ref(),
                _ => None,
            })
            .unwrap();
        let hyperliquid_ref = broker
            .connections
            .iter()
            .find_map(|connection| match connection {
                ConnectionConfig::Hyperliquid {
                    name,
                    credential_ref,
                    ..
                } if name == "dev-hyperliquid" => credential_ref.as_ref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(wallet_ref, &reference);
        assert_eq!(hyperliquid_ref, &reference);
        assert_eq!(
            broker
                .connections
                .iter()
                .filter(|connection| matches!(connection, ConnectionConfig::Hyperliquid { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn public_evm_recipient_can_be_observed_without_importing_a_signer() {
        let mut config = EnvironmentConfig::empty("env");
        config.evm_addresses = vec!["0x1111111111111111111111111111111111111111".into()];
        let broker = broker_config(&config).unwrap();
        assert!(broker.connections.iter().any(|connection| matches!(connection,
            ConnectionConfig::Evm { name, address, credential_ref: None, signing_enabled: false, .. }
            if name == "dev-wallet" && address == "0x1111111111111111111111111111111111111111")));
        assert!(
            !broker
                .connections
                .iter()
                .any(|connection| matches!(connection, ConnectionConfig::Hyperliquid { .. }))
        );
        broker.validate().unwrap();
    }

    #[test]
    fn classifies_synthetic_ed25519_and_rsa_pem_keys() {
        assert!(matches!(
            classify_binance_pem("ed-fixture".into(), ED25519_PEM.replace("\\n", "\n")).unwrap(),
            SecretMaterial::BinanceEd25519 { .. }
        ));
        assert!(matches!(
            classify_binance_pem("rsa-fixture".into(), RSA_PEM.replace("\\n", "\n")).unwrap(),
            SecretMaterial::BinanceRsa { .. }
        ));
        assert!(classify_binance_pem("bad-fixture".into(), "not a key".into()).is_err());
    }

    #[tokio::test]
    async fn offline_import_roundtrips_main_and_sub_ed25519_credentials() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let journal_path = dir.path().join("operations.sqlite");
        let prior_journal = b"synthetic journal sentinel";
        fs::write(&journal_path, prior_journal).unwrap();
        let credential_dir = dir.path().join("credentials");
        let dotenv_path = dir.path().join("keys.env");
        let mut initial = EnvironmentConfig::empty("env");
        initial.policy = Some(custom_development_policy());
        initial.reviewed_chain_ids = Some(vec![42161]);
        initial
            .rpc_urls
            .insert(1, vec!["https://archive.example.invalid".into()]);
        initial.write_atomic(&config_path).unwrap();
        let source = format!(
            "GOFIN_DEV_BINANCE_API_KEY=synthetic-main-api-key\nGOFIN_DEV_BINANCE_PRIVATE_KEY_PEM={ED25519_PEM}\nGOFIN_DEV_BINANCE_UID=100\nGOFIN_DEV_BINANCE_SUB_API_KEY=synthetic-sub-api-key\nGOFIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM={ED25519_PEM}\nGOFIN_DEV_BINANCE_SUB_UID=200\nGOFIN_DEV_BINANCE_SUB_EMAIL=child@example.invalid\n"
        );
        fs::write(&dotenv_path, source).unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(
            &config_path,
            "env",
            &dotenv_path,
            &credential_dir,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(&journal_path).unwrap(), prior_journal);

        let saved = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(saved.binance_uid.as_deref(), Some("100"));
        assert_eq!(saved.binance_sub_uid.as_deref(), Some("200"));
        assert_eq!(
            saved.binance_sub_email.as_deref(),
            Some("child@example.invalid")
        );
        assert_eq!(saved.policy, Some(custom_development_policy()));
        assert_eq!(saved.reviewed_chain_ids, Some(vec![42161]));
        assert_eq!(
            saved.rpc_urls.get(&1),
            Some(&vec!["https://archive.example.invalid".into()])
        );
        let config = broker_config(&saved).unwrap();
        assert_eq!(config.policy, custom_development_policy());
        assert!(config.validate().is_ok());
        let credentials =
            dev_credentials::DevelopmentCredentialProvider::new(credential_dir).unwrap();
        use gofin_exchange::ccxt::CredentialProvider;
        for (reference, api_key) in [
            (
                saved.credentials.binance.as_deref().unwrap(),
                "synthetic-main-api-key",
            ),
            (
                saved.credentials.binance_sub.as_deref().unwrap(),
                "synthetic-sub-api-key",
            ),
        ] {
            let material = credentials.credentials(reference).unwrap();
            assert_eq!(material.api_key.as_str(), api_key);
            assert_eq!(
                material.secret.as_str(),
                ED25519_PEM.replace("\\n", "\n").replace("\\n", "\n")
            );
        }
    }

    fn indexed_fixture_file() -> String {
        [
            ("GOFIN_DEV_KRAKEN_1","synthetic-spot-only","spot-001","spot-fixture"),
            ("GOFIN_DEV_KRAKENFUTURES_1","synthetic-master-only","master-001","master-fixture"),
            ("GOFIN_DEV_KRAKENFUTURES_2","synthetic-child-only","child-001","child-fixture")
        ].into_iter().map(|(prefix,key,id,name)|format!("{prefix}_API_KEY={key}\n{prefix}_SECRET=c3ludGhldGljLW5vbi1saXZlLWtleQ==\n{prefix}_ACCOUNT_ID={id}\n{prefix}_NAME={name}\n")).collect::<String>()+"GOFIN_DEV_KRAKEN_1_DEFAULT_WALLET_ACCOUNT_ID=WDEFAULT\n"
    }
    #[tokio::test]
    async fn indexed_kraken_import_loads_master_child_and_spot_without_network_and_rotates_one_slot()
     {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let keys = dir.path().join("synthetic-only.env");
        let mut initial = EnvironmentConfig::empty("env");
        initial.policy = Some(custom_development_policy());
        initial.write_atomic(&config_path).unwrap();
        fs::write(&keys, indexed_fixture_file()).unwrap();
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o600)).unwrap();
        import(&config_path, "env", &keys, &credential_dir, false, None)
            .await
            .unwrap();
        let first = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(first.kraken_accounts.len(), 3);
        assert!(!first.credentials_is_empty());
        assert_eq!(first.policy, initial.policy);
        let broker = broker_config(&first).unwrap();
        assert_eq!(broker.connections.len(), 3);
        assert!(
            matches!(&broker.connections[0],ConnectionConfig::Kraken{name,default_wallet_account_id,..}if name=="spot-fixture"&&default_wallet_account_id.as_deref()==Some("WDEFAULT"))
        );
        assert!(broker.connections.iter().any(
            |c| matches!(c,ConnectionConfig::KrakenFutures{name,..}if name=="master-fixture")
        ));
        let provider =
            dev_credentials::DevelopmentCredentialProvider::new(credential_dir.clone()).unwrap();
        use gofin_exchange::ccxt::CredentialProvider;
        assert_eq!(
            provider
                .credentials(&first.kraken_accounts["krakenfutures_2"].credential_ref)
                .unwrap()
                .api_key
                .as_str(),
            "synthetic-child-only"
        );
        let old_child = first.kraken_accounts["krakenfutures_2"]
            .credential_ref
            .clone();
        fs::write(&keys,"GOFIN_DEV_KRAKENFUTURES_2_API_KEY=synthetic-rotated-child-only\nGOFIN_DEV_KRAKENFUTURES_2_SECRET=c3ludGhldGljLW5vbi1saXZlLWtleQ==\nGOFIN_DEV_KRAKENFUTURES_2_ACCOUNT_ID=child-001\n").unwrap();
        import(&config_path, "env", &keys, &credential_dir, false, None)
            .await
            .unwrap();
        let second = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(second.kraken_accounts.len(), 3);
        assert_eq!(
            second.kraken_accounts["krakenfutures_2"].name,
            "child-fixture"
        );
        assert_eq!(
            second.kraken_accounts["kraken_1"]
                .default_wallet_account_id
                .as_deref(),
            Some("WDEFAULT")
        );
        assert_eq!(
            second.kraken_accounts["krakenfutures_1"].credential_ref,
            first.kraken_accounts["krakenfutures_1"].credential_ref
        );
        assert!(!credential_dir.join(format!("{old_child}.json")).exists());
        assert!(
            provider
                .credentials(&second.kraken_accounts["krakenfutures_2"].credential_ref)
                .is_ok()
        );
    }
    #[tokio::test]
    async fn indexed_kraken_reimport_rejects_identity_name_default_alias_and_custody_duplicates_atomically()
     {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let keys = dir.path().join("synthetic-only.env");
        fs::write(&keys, indexed_fixture_file()).unwrap();
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o600)).unwrap();
        import(&config_path, "env", &keys, &credential_dir, false, None)
            .await
            .unwrap();
        let original = fs::read(&config_path).unwrap();
        let files = fs::read_dir(&credential_dir).unwrap().count();
        let source = indexed_fixture_file();
        for changed in [
            source.replace("ACCOUNT_ID=child-001", "ACCOUNT_ID=other-identity"),
            source.replace("NAME=child-fixture", "NAME=renamed-fixture"),
            source.replace("ID=WDEFAULT", "ID=WOTHER"),
            source.replace("ACCOUNT_ID=child-001", "ACCOUNT_ID=master-001"),
            source.replace("NAME=child-fixture", "NAME=master-fixture"),
        ] {
            fs::write(&keys, changed).unwrap();
            let error = import(&config_path, "env", &keys, &credential_dir, false, None)
                .await
                .unwrap_err();
            assert!(!error.contains("synthetic-child-only"));
            assert_eq!(fs::read(&config_path).unwrap(), original);
            assert_eq!(fs::read_dir(&credential_dir).unwrap().count(), files);
        }
    }
    #[tokio::test]
    async fn indexed_kraken_partial_groups_and_legacy_alias_duplicates_cannot_create_credentials() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let keys = dir.path().join("synthetic-only.env");
        fs::write(&keys, "GOFIN_DEV_KRAKENFUTURES_8_NAME=partial-fixture\n").unwrap();
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            import(&config_path, "env", &keys, &credential_dir, false, None)
                .await
                .is_err()
        );
        assert!(!config_path.exists());
        assert!(!credential_dir.exists());
        let source = indexed_fixture_file()
            + "GOFIN_DEV_KRAKEN_API_KEY=synthetic-legacy-only\nGOFIN_DEV_KRAKEN_SECRET=c3ludGhldGljLW5vbi1saXZlLWtleQ==\nGOFIN_DEV_KRAKEN_ACCOUNT_ID=spot-001\n";
        fs::write(&keys, source).unwrap();
        assert!(
            import(&config_path, "env", &keys, &credential_dir, false, None)
                .await
                .is_err()
        );
        assert!(!config_path.exists());
        assert!(!credential_dir.exists());
    }
    #[tokio::test]
    async fn offline_kraken_import_keeps_products_separate_and_refuses_identity_swaps() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let dotenv_path = dir.path().join("keys.env");
        let source = "GOFIN_DEV_KRAKEN_API_KEY=synthetic-spot-key\nGOFIN_DEV_KRAKEN_SECRET=c3ludGhldGljLW5vbi1saXZlLWtleQ==\nGOFIN_DEV_KRAKEN_ACCOUNT_ID=synthetic-spot-account\nGOFIN_DEV_KRAKEN_DEFAULT_WALLET_ACCOUNT_ID=WDEFAULT\nGOFIN_DEV_KRAKENFUTURES_API_KEY=synthetic-derivatives-key\nGOFIN_DEV_KRAKENFUTURES_SECRET=c3ludGhldGljLW5vbi1saXZlLWtleQ==\nGOFIN_DEV_KRAKENFUTURES_ACCOUNT_ID=synthetic-derivatives-account\n";
        fs::write(&dotenv_path, source).unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(
            &config_path,
            "env",
            &dotenv_path,
            &credential_dir,
            false,
            None,
        )
        .await
        .unwrap();
        let config = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert!(config.credentials.kraken.is_some());
        assert_eq!(
            config.kraken_default_wallet_account_id.as_deref(),
            Some("WDEFAULT")
        );
        assert!(config.credentials.krakenfutures.is_some());
        let broker = broker_config(&config).unwrap();
        assert_eq!(broker.connections.len(), 2);
        assert!(broker.connections.iter().all(|c| matches!(
            c,
            ConnectionConfig::Kraken {
                physical_identity_verified: false,
                ..
            } | ConnectionConfig::KrakenFutures {
                physical_identity_verified: false,
                ..
            }
        )));
        let raw = fs::read_to_string(&config_path).unwrap();
        assert!(!raw.contains("synthetic-spot-key"));
        assert!(!raw.contains("c3ludGhldGlj"));
        let before = fs::read(&config_path).unwrap();
        fs::write(
            &dotenv_path,
            source.replace("synthetic-spot-account", "different-account"),
        )
        .unwrap();
        assert!(
            import(
                &config_path,
                "env",
                &dotenv_path,
                &credential_dir,
                false,
                None
            )
            .await
            .is_err()
        );
        assert_eq!(before, fs::read(&config_path).unwrap());
        for invalid in [
            source.replace("WDEFAULT", "WOTHER"),
            source.replace("WDEFAULT", "bad/path"),
        ] {
            fs::write(&dotenv_path, invalid).unwrap();
            assert!(
                import(
                    &config_path,
                    "env",
                    &dotenv_path,
                    &credential_dir,
                    false,
                    None
                )
                .await
                .is_err()
            );
            assert_eq!(before, fs::read(&config_path).unwrap());
        }
        // Omitting optional metadata on credential replacement preserves the alias.
        fs::write(
            &dotenv_path,
            source.replace("GOFIN_DEV_KRAKEN_DEFAULT_WALLET_ACCOUNT_ID=WDEFAULT\n", ""),
        )
        .unwrap();
        import(
            &config_path,
            "env",
            &dotenv_path,
            &credential_dir,
            false,
            None,
        )
        .await
        .unwrap();
        let config = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(
            config.kraken_default_wallet_account_id.as_deref(),
            Some("WDEFAULT")
        );
        let selected = broker_config(&config)
            .unwrap()
            .connections
            .into_iter()
            .find(|c| matches!(c, ConnectionConfig::Kraken { .. }))
            .unwrap();
        assert!(
            matches!(selected,ConnectionConfig::Kraken{default_wallet_account_id:Some(id),..} if id=="WDEFAULT")
        );
    }

    #[tokio::test]
    async fn import_persists_only_handles_and_uses_private_files() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let dotenv_path = dir.path().join("keys.env");
        fs::write(&dotenv_path, "GOFIN_DEV_EVM_PRIVATE_KEY=0000000000000000000000000000000000000000000000000000000000000001\n").unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(
            &config_path,
            "env",
            &dotenv_path,
            &credential_dir,
            false,
            None,
        )
        .await
        .unwrap();
        let saved = fs::read_to_string(&config_path).unwrap();
        assert!(
            !saved.contains("0000000000000000000000000000000000000000000000000000000000000001")
        );
        let parsed = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(parsed.mode, "live");
        assert_eq!(
            fs::metadata(
                credential_dir.join(format!("{}.json", parsed.credentials.evm_primary.unwrap()))
            )
            .unwrap()
            .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn relay_import_keeps_key_private_and_preserves_legacy_profile() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let dotenv_path = dir.path().join("keys.env");
        fs::write(
            &dotenv_path,
            "GOFIN_DEV_EVM_PRIVATE_KEY=$(ignored)\nGOFIN_DEV_RELAY_API_KEY=SYNTHETIC-RELAY-KEY\n",
        )
        .unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(
            &config_path,
            "env",
            &dotenv_path,
            &credential_dir,
            false,
            Some("relay"),
        )
        .await
        .unwrap();
        let saved = fs::read_to_string(&config_path).unwrap();
        assert!(!saved.contains("SYNTHETIC-RELAY-KEY"));
        let parsed = EnvironmentConfig::read(&config_path, "env").unwrap();
        let reference = parsed.credentials.relay.clone().unwrap();
        assert!(CredentialRef::new(&reference).is_ok());
        assert!(matches!(
            broker_config(&parsed).unwrap().connections.iter().find(|connection| {
                matches!(connection, ConnectionConfig::Relay { .. })
            }),
            Some(ConnectionConfig::Relay { resource, .. }) if resource.0 == "relay:service"
        ));
        assert_eq!(
            fs::metadata(credential_dir.join(format!("{reference}.json")))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        let restarted =
            dev_credentials::DevelopmentCredentialProvider::new(credential_dir).unwrap();
        let key = gofin_broker::BrokerCredentialProvider::relay_api_key(
            &restarted,
            &CredentialRef::new(&reference).unwrap(),
        )
        .unwrap();
        assert_eq!(key.as_str(), "SYNTHETIC-RELAY-KEY");
    }

    #[tokio::test]
    async fn address_mismatch_fails_without_persisting() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let dotenv_path = dir.path().join("keys.env");
        fs::write(&dotenv_path, "GOFIN_DEV_EVM_PRIVATE_KEY=0000000000000000000000000000000000000000000000000000000000000001\nGOFIN_DEV_EVM_ADDRESS=0x1111111111111111111111111111111111111111\n").unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            import(
                &config_path,
                "env",
                &dotenv_path,
                &dir.path().join("credentials"),
                false,
                None,
            )
            .await
            .is_err()
        );
        assert!(!config_path.exists());
    }

    #[tokio::test]
    async fn binance_identity_fields_require_matching_credentials() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let dotenv_path = dir.path().join("keys.env");
        fs::write(
            &dotenv_path,
            "GOFIN_DEV_BINANCE_UID=100\nGOFIN_DEV_EVM_PRIVATE_KEY=0000000000000000000000000000000000000000000000000000000000000001\n",
        )
        .unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            import(
                &config_path,
                "env",
                &dotenv_path,
                &dir.path().join("credentials"),
                false,
                None,
            )
            .await
            .unwrap_err()
            .contains("requires a main Binance credential")
        );
        assert!(!config_path.exists());
    }

    #[tokio::test]
    async fn duplicate_wallet_import_preserves_existing_config_and_credentials() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let dotenv_path = dir.path().join("keys.env");
        let mut profile = EnvironmentConfig::empty("env");
        profile.policy_limits = Some(limits("10000000", "100000000"));
        profile.write_atomic(&config_path).unwrap();
        let key = "0000000000000000000000000000000000000000000000000000000000000001";
        fs::write(&dotenv_path, format!("GOFIN_DEV_EVM_PRIVATE_KEY={key}\n")).unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(
            &config_path,
            "env",
            &dotenv_path,
            &credential_dir,
            false,
            None,
        )
        .await
        .unwrap();
        let imported = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(imported.policy_limits, profile.policy_limits);
        assert!(imported.credentials.evm_primary.is_some());
        let old_config = fs::read(&config_path).unwrap();
        let old_credential_files = fs::read_dir(&credential_dir).unwrap().count();

        fs::write(
            &dotenv_path,
            format!("GOFIN_DEV_EVM_SECONDARY_PRIVATE_KEY={key}\n"),
        )
        .unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = import(
            &config_path,
            "env",
            &dotenv_path,
            &credential_dir,
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(!error.contains(key));
        assert_eq!(fs::read(&config_path).unwrap(), old_config);
        assert_eq!(
            fs::read_dir(&credential_dir).unwrap().count(),
            old_credential_files
        );
    }
}
