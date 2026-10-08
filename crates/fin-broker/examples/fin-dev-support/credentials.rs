use fin_broker::BrokerCredentialProvider;
use fin_credentials::{CredentialRef, SecretMaterial};
use fin_domain::{FinError, Result};
use fin_exchange::ccxt::{CcxtError, CcxtErrorKind, CredentialProvider, Credentials};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroizing;

pub struct DevelopmentCredentialProvider {
    directory: PathBuf,
}

pub struct ImportCredentialProvider {
    materials: BTreeMap<String, (Zeroizing<String>, Zeroizing<String>)>,
}

impl ImportCredentialProvider {
    pub fn new(entries: Vec<(String, String, String)>) -> Result<Self> {
        let mut materials = BTreeMap::new();
        for (handle, api_key, secret) in entries {
            if materials
                .insert(handle, (Zeroizing::new(api_key), Zeroizing::new(secret)))
                .is_some()
            {
                return Err(FinError::validation(
                    "Duplicate development credential handle.",
                ));
            }
        }
        Ok(Self { materials })
    }
}

impl CredentialProvider for ImportCredentialProvider {
    fn credentials(&self, reference: &str) -> std::result::Result<Credentials, CcxtError> {
        self.materials
            .get(reference)
            .map(|(api_key, secret)| Credentials {
                api_key: Zeroizing::new(api_key.to_string()),
                secret: Zeroizing::new(secret.to_string()),
            })
            .ok_or_else(|| CcxtError::new(CcxtErrorKind::PermissionDenied))
    }
}

impl DevelopmentCredentialProvider {
    pub fn new(directory: PathBuf) -> Result<Self> {
        let metadata = fs::symlink_metadata(&directory).map_err(|_| unavailable())?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(unix)]
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o777 != 0o700
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(unavailable());
        }
        Ok(Self { directory })
    }

    fn material<T>(
        &self,
        reference: &CredentialRef,
        callback: impl FnOnce(&SecretMaterial) -> Result<T>,
    ) -> Result<T> {
        let path = self.directory.join(format!("{}.json", reference.as_str()));
        let metadata = fs::symlink_metadata(&path).map_err(|_| unavailable())?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(unix)]
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o777 != 0o600
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(unavailable());
        }
        if metadata.len() > 32_768 {
            return Err(unavailable());
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(&path).map_err(|_| unavailable())?;
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(32_769)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() > 32_768 {
            return Err(unavailable());
        }
        let material: SecretMaterial = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        material.validate_present()?;
        callback(&material)
    }
}

impl CredentialProvider for DevelopmentCredentialProvider {
    fn credentials(&self, raw: &str) -> std::result::Result<Credentials, CcxtError> {
        let denied = || CcxtError::new(CcxtErrorKind::PermissionDenied);
        let reference = CredentialRef::new(raw).map_err(|_| denied())?;
        self.material(&reference, |material| {
            let (api_key, secret) = match material {
                SecretMaterial::BinanceHmac { api_key, secret } => (api_key, secret),
                SecretMaterial::BinanceRsa {
                    api_key,
                    private_key,
                }
                | SecretMaterial::BinanceEd25519 {
                    api_key,
                    private_key,
                } => (api_key, private_key),
                _ => return Err(FinError::validation("Credential type mismatch.")),
            };
            Ok(Credentials {
                api_key: Zeroizing::new(api_key.clone()),
                secret: Zeroizing::new(secret.clone()),
            })
        })
        .map_err(|_| denied())
    }
}

impl BrokerCredentialProvider for DevelopmentCredentialProvider {
    fn envelope_signer(
        &self,
        reference: &CredentialRef,
    ) -> Result<(String, Arc<dyn fin_evm::EnvelopeSigner>)> {
        self.material(reference, |material| match material {
            SecretMaterial::EvmPrivateKey { private_key } => {
                let signer = fin_evm::LocalEnvelopeSigner::from_private_key(private_key)?;
                Ok((
                    format!("{:#x}", signer.address()),
                    Arc::new(signer) as Arc<dyn fin_evm::EnvelopeSigner>,
                ))
            }
            _ => Err(FinError::validation("Credential type mismatch.")),
        })
    }

    fn usd_price_source(
        &self,
        reference: &CredentialRef,
        plan: &str,
    ) -> Result<Arc<dyn fin_pricing::UsdPriceSource>> {
        self.material(reference, |material| match material {
            SecretMaterial::CoinGeckoApiKey {
                api_key,
                plan: stored,
            } if stored == plan => {
                let plan = match plan {
                    "demo" => fin_pricing::ApiPlan::Demo,
                    "pro" => fin_pricing::ApiPlan::Pro,
                    _ => return Err(FinError::validation("Pricing plan mismatch.")),
                };
                Ok(
                    Arc::new(fin_pricing::CoinGeckoPrices::with_api_key(plan, api_key)?)
                        as Arc<dyn fin_pricing::UsdPriceSource>,
                )
            }
            _ => Err(FinError::validation("Credential type mismatch.")),
        })
    }
}

pub fn create_store(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err(unavailable());
            }
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .map_err(|_| unavailable())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(directory).map_err(|_| unavailable())?;
        }
        Err(_) => return Err(unavailable()),
    }
    Ok(())
}

pub fn write_material(directory: &Path, material: &SecretMaterial) -> Result<CredentialRef> {
    material.validate_present()?;
    let reference = CredentialRef::new(&uuid::Uuid::new_v4().hyphenated().to_string())?;
    let bytes = Zeroizing::new(serde_json::to_vec(material).map_err(|_| unavailable())?);
    let path = directory.join(format!("{}.json", reference.as_str()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&path).map_err(|_| unavailable())?;
    use std::io::Write;
    if file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .is_err()
        || fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .is_err()
    {
        let _ = fs::remove_file(path);
        return Err(unavailable());
    }
    Ok(reference)
}

fn unavailable() -> FinError {
    FinError::new(
        "DEV_CREDENTIAL_UNAVAILABLE",
        "credentials",
        "Development credentials could not be safely accessed.",
        "Re-import the dedicated development credentials file.",
    )
}
