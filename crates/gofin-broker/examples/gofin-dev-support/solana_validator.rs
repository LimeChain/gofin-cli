//! Explicit local-validator transport for the development broker.
//!
//! This fixture is only composed by `gofin-dev-broker --mode synthetic
//! --fixture solana-validator`. It accepts loopback RPC and a synthetic wallet
//! key supplied by the ignored local-validator harness; the installed broker
//! has no path to this module.

use async_trait::async_trait;
use gofin_broker::{
    BrokerConfigV2, BrokerCredentialProvider, BrokerPolicy, ConnectionConfig, DeploymentConfig,
    NetworkConfig,
};
use gofin_credentials::{CredentialRef, SecretMaterial};
use gofin_domain::{AssetId, GoFinError, ResourceId, Result};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use gofin_solana::{MAINNET_GENESIS_HASH, SolanaRpc, SolanaSigner};
use reqwest::{Client, Url, redirect::Policy};
use serde_json::{Value, json};
use std::{
    fs,
    net::IpAddr,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Arc,
    time::Duration,
};
use zeroize::Zeroizing;

const PRIMARY_REFERENCE_FILE: &str = "solana-validator-primary-credential-ref";
const MAX_RPC_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const SYNTHETIC_FIXTURE_DIRECTORY: &str = "../../.context/meteora-validator";

pub fn applications(
    environment_dir: &Path,
) -> std::result::Result<(BrokerConfigV2, gofin_broker::ProtectedApplications), String> {
    let endpoint = std::env::var("GOFIN_METEORA_VALIDATOR_RPC")
        .map_err(|_| "local-validator fixture requires GOFIN_METEORA_VALIDATOR_RPC")?;
    let rpc = Arc::new(LocalValidatorRpc::new(&endpoint)?);
    let key_path = std::env::var("GOFIN_METEORA_VALIDATOR_WALLET_KEY")
        .map_err(|_| "local-validator fixture requires a synthetic wallet key path")?;
    let key_path = Path::new(&key_path);
    if !key_path.is_absolute()
        || fs::symlink_metadata(key_path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err("synthetic validator key path must be an absolute plain file path".into());
    }
    let key_path = key_path
        .canonicalize()
        .map_err(|_| "synthetic local-validator wallet key is unavailable")?;
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(SYNTHETIC_FIXTURE_DIRECTORY)
        .canonicalize()
        .map_err(|_| "synthetic local-validator fixtures are unavailable")?;
    if !key_path.starts_with(&fixture_root) {
        return Err(
            "synthetic validator key must be inside the ignored validator fixture directory".into(),
        );
    }
    let key_text = read_private_file(&key_path, 256)
        .map_err(|_| "synthetic local-validator wallet key is unavailable")?;
    let expected_signer = SolanaSigner::parse(key_text.trim())
        .map_err(|_| "synthetic local-validator wallet key is invalid")?;
    let credentials_dir = environment_dir.join("credentials");
    crate::dev_credentials::create_store(&credentials_dir)
        .map_err(|_| "development credential store is unavailable")?;
    let reference = primary_reference(&credentials_dir, environment_dir, key_text.trim())?;
    let credentials = Arc::new(
        crate::dev_credentials::DevelopmentCredentialProvider::new(credentials_dir)
            .map_err(|_| "development credential store is unavailable")?,
    );
    let signer = credentials
        .solana_signer(&reference)
        .map_err(|_| "synthetic validator wallet credential could not be loaded")?;
    if signer.address() != expected_signer.address() {
        return Err("stored synthetic validator wallet differs from the selected key".into());
    }

    let address = signer.address().to_owned();
    let mut approver_uid = unsafe { libc::geteuid() };
    if approver_uid < 501 {
        approver_uid = 501;
    }
    let mut policy = BrokerPolicy::beta();
    // The lifecycle fixture deliberately moves more than the default $100
    // development ceiling across several real protocol actions. Keep a finite
    // fixture-only cap with enough room for the complete synthetic sequence.
    policy.max_movement_usd_micros = "500000000".into();
    policy.max_run_allocation_usd_micros = "2000000000".into();
    let config = BrokerConfigV2 {
        version: 2,
        networks: Vec::<NetworkConfig>::new(),
        deployments: Vec::<DeploymentConfig>::new(),
        connections: vec![ConnectionConfig::Solana {
            name: "meteora-validator".into(),
            resource: ResourceId(format!("solana:mainnet-beta:{address}")),
            address,
            credential_ref: Some(reference.as_str().to_owned()),
            signing_enabled: true,
            network: "solana:mainnet-beta".into(),
        }],
        policy,
        approver_uid,
        extensions: Default::default(),
    };
    let source: Arc<dyn UsdPriceSource> = Arc::new(LocalValidatorPrices);
    let applications = gofin_broker::build_applications_with_network_transports_and_solana_prices(
        &config,
        credentials,
        source.clone(),
        Default::default(),
        Some(rpc),
        None,
        Some(source),
    )
    .map_err(|_| "local-validator applications could not be initialized")?;
    Ok((config, applications))
}

fn primary_reference(
    credentials_dir: &Path,
    environment_dir: &Path,
    private_key: &str,
) -> std::result::Result<CredentialRef, String> {
    let ref_path = environment_dir.join(PRIMARY_REFERENCE_FILE);
    match read_private_file(&ref_path, 128) {
        Ok(contents) => CredentialRef::new(contents.trim())
            .map_err(|_| "local-validator credential reference is invalid".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let reference = crate::dev_credentials::write_material(
                credentials_dir,
                &SecretMaterial::SolanaPrivateKey {
                    private_key: private_key.to_owned(),
                },
            )
            .map_err(|_| "synthetic validator wallet credential could not be stored")?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let mut file = options
                .open(&ref_path)
                .map_err(|_| "local-validator credential reference could not be stored")?;
            use std::io::Write;
            file.write_all(reference.as_str().as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|_| "local-validator credential reference could not be stored")?;
            fs::File::open(environment_dir)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| "local-validator credential reference could not be stored")?;
            Ok(reference)
        }
        Err(_) => Err("local-validator credential reference is unavailable".into()),
    }
}

fn read_private_file(path: &Path, maximum_bytes: u64) -> std::io::Result<Zeroizing<String>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.mode() & 0o777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.len() > maximum_bytes
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private fixture file metadata is invalid",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    use std::io::Read;
    file.take(maximum_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "private fixture file is too large",
        ));
    }
    String::from_utf8(bytes.to_vec())
        .map(Zeroizing::new)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-8"))
}

struct LocalValidatorRpc {
    endpoint: String,
    client: Client,
}

impl LocalValidatorRpc {
    fn new(endpoint: &str) -> std::result::Result<Self, String> {
        if !is_loopback_http_endpoint(endpoint) {
            return Err("local-validator RPC must be an explicit HTTP loopback endpoint".into());
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| "local-validator HTTP client could not be initialized")?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            client,
        })
    }
}

fn is_loopback_http_endpoint(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else {
        return false;
    };
    let host_is_loopback = match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    };
    url.scheme() == "http"
        && host_is_loopback
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none()
}

#[async_trait]
impl SolanaRpc for LocalValidatorRpc {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        // The cloned accounts/programs retain mainnet identities. Solana's
        // installed HTTP transport pins this identity, so only this dev-only
        // wrapper synthesizes the expected genesis response; every operation,
        // simulation, and send still goes to the loopback validator.
        if method == "getGenesisHash" {
            return Ok(json!(MAINNET_GENESIS_HASH));
        }
        let response = self
            .client
            .post(&self.endpoint)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|_| validator_rpc_error())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_RPC_RESPONSE_BYTES as u64)
        {
            return Err(validator_rpc_error());
        }
        let body = response.bytes().await.map_err(|_| validator_rpc_error())?;
        if body.len() > MAX_RPC_RESPONSE_BYTES {
            return Err(validator_rpc_error());
        }
        let value: Value = serde_json::from_slice(&body).map_err(|_| validator_rpc_error())?;
        if let Some(rpc_error) = value.get("error").filter(|error| !error.is_null()) {
            let code = rpc_error
                .get("code")
                .and_then(Value::as_i64)
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".into());
            let message = rpc_error
                .get("message")
                .and_then(Value::as_str)
                .map(|message| message.chars().take(180).collect::<String>())
                .unwrap_or_else(|| "no RPC message".into());
            return Err(GoFinError::new(
                "SOLANA_RPC_ERROR",
                "rpc",
                format!("Local validator RPC {method} failed ({code}): {message}"),
                "Inspect the local validator log and synthetic fixture state.",
            ));
        }
        value.get("result").cloned().ok_or_else(validator_rpc_error)
    }
}

fn validator_rpc_error() -> GoFinError {
    GoFinError::new(
        "SOLANA_RPC_UNAVAILABLE",
        "rpc",
        "The isolated local validator did not return a valid RPC response.",
        "Check the loopback validator process and its ignored local evidence log.",
    )
}

struct LocalValidatorPrices;

#[async_trait]
impl UsdPriceSource for LocalValidatorPrices {
    async fn quote(&self, _asset: &AssetId) -> Result<UsdQuote> {
        let now = gofin_domain::now();
        Ok(UsdQuote {
            usd_per_unit: "1".into(),
            updated_at: now.clone(),
            observed_at: now,
            source: "synthetic local-validator test price".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{is_loopback_http_endpoint, read_private_file};
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn validator_transport_requires_loopback_http_without_redirect_surface() {
        assert!(is_loopback_http_endpoint("http://127.0.0.1:19001"));
        assert!(is_loopback_http_endpoint("http://[::1]:19001"));
        assert!(is_loopback_http_endpoint("http://localhost:19001/"));
        assert!(!is_loopback_http_endpoint("https://127.0.0.1:19001"));
        assert!(!is_loopback_http_endpoint("http://192.168.1.2:19001"));
        assert!(!is_loopback_http_endpoint("http://localhost:19001/path"));
        assert!(!is_loopback_http_endpoint(
            "http://localhost:19001/?redirect=http://example.com"
        ));
    }

    #[test]
    fn synthetic_key_reader_rejects_links_permissions_and_oversized_material() {
        let directory = tempfile::tempdir().unwrap();
        let private_file = directory.path().join("synthetic-key.json");
        fs::write(&private_file, "[1,2,3]").unwrap();
        fs::set_permissions(&private_file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_private_file(&private_file, 32).unwrap().as_str(),
            "[1,2,3]"
        );
        assert!(read_private_file(&private_file, 2).is_err());

        fs::set_permissions(&private_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_file(&private_file, 32).is_err());

        let link = directory.path().join("key-link.json");
        std::os::unix::fs::symlink(&private_file, &link).unwrap();
        assert!(read_private_file(&link, 32).is_err());
    }
}
