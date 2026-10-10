use crate::{dev_credentials, dotenv, identity};
use fin_broker::{BrokerConfigV2, BrokerPolicy, ConnectionConfig, DeploymentConfig, NetworkConfig};
use fin_credentials::{CredentialRef, SecretMaterial};
use fin_domain::{Quantity, ResourceId};
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
    pub binance: Option<String>,
    #[serde(default)]
    pub binance_sub: Option<String>,
    pub evm_primary: Option<String>,
    pub evm_secondary: Option<String>,
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
        let maximum = fin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS;
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
pub struct EnvironmentConfig {
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
    /// development profile. Missing chains retain the reviewed defaults.
    #[serde(default)]
    pub rpc_urls: BTreeMap<u64, Vec<String>>,
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
    pub policy_limits: Option<PolicyLimits>,
}

fn synthetic() -> String {
    "synthetic".into()
}

impl EnvironmentConfig {
    pub fn credentials_is_empty(&self) -> bool {
        self.credentials.binance.is_none()
            && self.credentials.binance_sub.is_none()
            && self.credentials.evm_primary.is_none()
            && self.credentials.evm_secondary.is_none()
            && self.credentials.binance_sub.is_none()
    }
    pub fn empty(environment_id: &str) -> Self {
        Self {
            format: "fin-dev-config".into(),
            environment_id: environment_id.into(),
            version: 1,
            mode: "synthetic".into(),
            credentials: CredentialReferences::default(),
            policy: None,
            rpc_urls: BTreeMap::new(),
            binance_uid: None,
            binance_sub_uid: None,
            binance_sub_email: None,
            binance_sub_email_matched_by_venue_id: false,
            evm_addresses: Vec::new(),
            policy_limits: None,
        }
    }
    pub fn read(path: &Path, environment_id: &str) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::empty(environment_id));
        }
        let raw =
            fs::read(path).map_err(|_| "development configuration could not be read".to_owned())?;
        let config: Self = serde_json::from_slice(&raw)
            .map_err(|_| "development configuration is invalid".to_owned())?;
        if config.environment_id != environment_id
            || config.version != 1
            || !matches!(
                config.format.as_str(),
                "fin-dev-config" | "fin-dev-synthetic-config"
            )
        {
            return Err("development configuration identity mismatch".into());
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
    let values = dotenv::parse(source)?;
    let mut config = EnvironmentConfig::read(config_path, environment_id)?;
    let old = config.clone();
    let mut additions: Vec<(String, SecretMaterial)> = Vec::new();
    let mut ephemeral = Vec::new();
    let main_material = make_binance_material(
        &values,
        "FIN_DEV_BINANCE_API_KEY",
        "FIN_DEV_BINANCE_SECRET",
        "FIN_DEV_BINANCE_PRIVATE_KEY_PEM",
        "main Binance",
    )?;
    let sub_material = make_binance_material(
        &values,
        "FIN_DEV_BINANCE_SUB_API_KEY",
        "FIN_DEV_BINANCE_SUB_SECRET",
        "FIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM",
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
    let has_main = additions.iter().any(|(label, _)| label == "binance");
    let has_sub = additions.iter().any(|(label, _)| label == "binance_sub");
    let main_uid_input = values.get("FIN_DEV_BINANCE_UID").map(|v| v.to_string());
    let sub_uid_input = values.get("FIN_DEV_BINANCE_SUB_UID").map(|v| v.to_string());
    let mut sub_email = values
        .get("FIN_DEV_BINANCE_SUB_EMAIL")
        .map(|v| v.to_string());
    let mut sub_email_matched_by_id = false;
    if !has_main && main_uid_input.is_some() {
        return Err("FIN_DEV_BINANCE_UID requires a main Binance credential".into());
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
                .ok_or("Binance import requires FIN_DEV_BINANCE_UID or --discover-identities")?;
            if !canonical_uid(&main_uid) {
                return Err("Binance account UID is invalid".into());
            }
            let sub_uid = if has_sub {
                let uid = sub_uid_input.ok_or("Binance subaccount import requires FIN_DEV_BINANCE_SUB_UID or --discover-identities")?;
                if !canonical_uid(&uid) {
                    return Err("Binance subaccount UID is invalid".into());
                }
                sub_email.as_ref().filter(|email| !email.trim().is_empty())
                    .ok_or("Binance subaccount import requires FIN_DEV_BINANCE_SUB_EMAIL or --discover-identities")?;
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
            "FIN_DEV_EVM_PRIVATE_KEY",
            "FIN_DEV_EVM_ADDRESS",
        ),
        (
            "evm_secondary",
            "FIN_DEV_EVM_SECONDARY_PRIVATE_KEY",
            "FIN_DEV_EVM_SECONDARY_ADDRESS",
        ),
    ] {
        if let Some(private_key) = values.get(key_name) {
            let signer = fin_evm::LocalEnvelopeSigner::from_private_key(private_key)
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
            "binance" => config.credentials.binance = Some(reference.as_str().into()),
            "binance_sub" => config.credentials.binance_sub = Some(reference.as_str().into()),
            "evm_primary" => config.credentials.evm_primary = Some(reference.as_str().into()),
            "evm_secondary" => config.credentials.evm_secondary = Some(reference.as_str().into()),
            _ => unreachable!(),
        }
    }
    config.mode = "live".into();
    config.format = "fin-dev-config".into();
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
        (&config.credentials.binance, &old_references.binance),
        (&config.credentials.binance_sub, &old_references.binance_sub),
        (&config.credentials.evm_primary, &old_references.evm_primary),
        (
            &config.credentials.evm_secondary,
            &old_references.evm_secondary,
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

pub fn broker_config(config: &EnvironmentConfig) -> Result<BrokerConfigV2, String> {
    let mut connections = Vec::new();
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
        if let (Some(reference), Some(address)) = (reference, address) {
            if address.is_empty() {
                return Err("EVM wallet address is missing".into());
            }
            connections.push(ConnectionConfig::Evm {
                name: name.into(),
                resource: ResourceId(format!("evm:{}", address.to_ascii_lowercase())),
                credential_ref: Some(reference.clone()),
                address: address.clone(),
                signing_enabled: true,
            });
        }
    }
    let mut networks = NetworkConfig::reviewed_evm_networks();
    for chain_id in config.rpc_urls.keys() {
        if ![1, 42161, 4663, 8453].contains(chain_id) {
            return Err("development RPC override names an unsupported chain".into());
        }
    }
    for network in &mut networks {
        if let Some(rpc_urls) = config.rpc_urls.get(&network.chain_id) {
            if rpc_urls.is_empty() {
                return Err("development RPC override must include a public HTTPS endpoint".into());
            }
            network.rpc_urls = rpc_urls.clone();
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
        deployments: DeploymentConfig::reviewed_uniswap_networks(),
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
        legacy.as_object_mut().unwrap().remove("policy");
        legacy.as_object_mut().unwrap().remove("rpc_urls");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let loaded = EnvironmentConfig::read(&path, "env").unwrap();
        assert!(loaded.policy.is_none());
        assert!(loaded.rpc_urls.is_empty());
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
            fin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS.to_string()
        );
        assert_eq!(
            config.policy.max_run_allocation_usd_micros,
            fin_domain::policy::DEFAULT_POLICY_LIMIT_USD_MICROS.to_string()
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
        let credential_dir = dir.path().join("credentials");
        let dotenv_path = dir.path().join("keys.env");
        let mut initial = EnvironmentConfig::empty("env");
        initial.policy = Some(custom_development_policy());
        initial
            .rpc_urls
            .insert(1, vec!["https://archive.example.invalid".into()]);
        initial.write_atomic(&config_path).unwrap();
        let source = format!(
            "FIN_DEV_BINANCE_API_KEY=synthetic-main-api-key\nFIN_DEV_BINANCE_PRIVATE_KEY_PEM={ED25519_PEM}\nFIN_DEV_BINANCE_UID=100\nFIN_DEV_BINANCE_SUB_API_KEY=synthetic-sub-api-key\nFIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM={ED25519_PEM}\nFIN_DEV_BINANCE_SUB_UID=200\nFIN_DEV_BINANCE_SUB_EMAIL=child@example.invalid\n"
        );
        fs::write(&dotenv_path, source).unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(&config_path, "env", &dotenv_path, &credential_dir, false)
            .await
            .unwrap();

        let saved = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(saved.binance_uid.as_deref(), Some("100"));
        assert_eq!(saved.binance_sub_uid.as_deref(), Some("200"));
        assert_eq!(
            saved.binance_sub_email.as_deref(),
            Some("child@example.invalid")
        );
        assert_eq!(saved.policy, Some(custom_development_policy()));
        assert_eq!(
            saved.rpc_urls.get(&1),
            Some(&vec!["https://archive.example.invalid".into()])
        );
        let config = broker_config(&saved).unwrap();
        assert_eq!(config.policy, custom_development_policy());
        assert!(config.validate().is_ok());
        let credentials =
            dev_credentials::DevelopmentCredentialProvider::new(credential_dir).unwrap();
        use fin_exchange::ccxt::CredentialProvider;
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

    #[tokio::test]
    async fn import_persists_only_handles_and_uses_private_files() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let credential_dir = dir.path().join("credentials");
        let dotenv_path = dir.path().join("keys.env");
        fs::write(&dotenv_path, "FIN_DEV_EVM_PRIVATE_KEY=0000000000000000000000000000000000000000000000000000000000000001\n").unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(&config_path, "env", &dotenv_path, &credential_dir, false)
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
    async fn address_mismatch_fails_without_persisting() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let dotenv_path = dir.path().join("keys.env");
        fs::write(&dotenv_path, "FIN_DEV_EVM_PRIVATE_KEY=0000000000000000000000000000000000000000000000000000000000000001\nFIN_DEV_EVM_ADDRESS=0x1111111111111111111111111111111111111111\n").unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            import(
                &config_path,
                "env",
                &dotenv_path,
                &dir.path().join("credentials"),
                false,
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
            "FIN_DEV_BINANCE_UID=100\nFIN_DEV_EVM_PRIVATE_KEY=0000000000000000000000000000000000000000000000000000000000000001\n",
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
        fs::write(&dotenv_path, format!("FIN_DEV_EVM_PRIVATE_KEY={key}\n")).unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        import(&config_path, "env", &dotenv_path, &credential_dir, false)
            .await
            .unwrap();
        let imported = EnvironmentConfig::read(&config_path, "env").unwrap();
        assert_eq!(imported.policy_limits, profile.policy_limits);
        assert!(imported.credentials.evm_primary.is_some());
        let old_config = fs::read(&config_path).unwrap();
        let old_credential_files = fs::read_dir(&credential_dir).unwrap().count();

        fs::write(
            &dotenv_path,
            format!("FIN_DEV_EVM_SECONDARY_PRIVATE_KEY={key}\n"),
        )
        .unwrap();
        fs::set_permissions(&dotenv_path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = import(&config_path, "env", &dotenv_path, &credential_dir, false)
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
