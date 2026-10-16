use gofin_broker::BrokerCredentialProvider;
use gofin_credentials::{CredentialRef, SecretMaterial};
use gofin_domain::{GoFinError, Result};
use gofin_exchange::ccxt::{CcxtError, CcxtErrorKind, CredentialProvider, Credentials};
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
                return Err(GoFinError::validation(
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
                SecretMaterial::BinanceHmac { api_key, secret }
                | SecretMaterial::KrakenApi { api_key, secret }
                | SecretMaterial::KrakenFuturesApi { api_key, secret } => (api_key, secret),
                SecretMaterial::BinanceRsa {
                    api_key,
                    private_key,
                }
                | SecretMaterial::BinanceEd25519 {
                    api_key,
                    private_key,
                } => (api_key, private_key),
                _ => return Err(GoFinError::validation("Credential type mismatch.")),
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
    fn coinbase_auth(
        &self,
        credential: &CredentialRef,
    ) -> Result<gofin_exchange::coinbase::CoinbaseAuth> {
        use gofin_exchange::coinbase::CoinbaseAuth as A;
        self.material(credential, |material| match material {
            SecretMaterial::CoinbaseCdpEd25519 {
                api_key,
                private_key_base64,
            } => Ok(A::CdpEd25519 {
                api_key: api_key.clone(),
                private_key_base64: private_key_base64.clone(),
            }),
            SecretMaterial::CoinbaseCdpEcdsa {
                api_key,
                private_key_pem,
            } => Ok(A::CdpEcdsa {
                api_key: api_key.clone(),
                private_key_pem: private_key_pem.clone(),
            }),
            SecretMaterial::CoinbaseExchangeHmac {
                api_key,
                secret,
                passphrase,
            } => Ok(A::ExchangeHmac {
                api_key: api_key.clone(),
                secret: secret.clone(),
                passphrase: passphrase.clone(),
            }),
            SecretMaterial::CoinbaseInternationalHmac {
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

    fn hyperliquid_signer(
        &self,
        credential: &str,
        owner: &str,
    ) -> Result<gofin_exchange::ccxt::HyperliquidSigner> {
        let reference = CredentialRef::new(credential)?;
        self.material(&reference, |material| match material {
            SecretMaterial::EvmPrivateKey { private_key } => {
                let parsed = gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)
                    .map_err(|_| {
                        GoFinError::new(
                            "CREDENTIAL_INVALID",
                            "credentials",
                            "Development EVM key material is invalid.",
                            "Re-import a valid disposable EVM private key.",
                        )
                    })?;
                let derived = format!("{:#x}", parsed.address()).to_ascii_lowercase();
                if !derived.eq_ignore_ascii_case(owner) {
                    return Err(GoFinError::new(
                        "SIGNER_ADDRESS_MISMATCH",
                        "signing",
                        "The development key does not derive the configured Hyperliquid owner.",
                        "Correct the owner or re-import the disposable development key.",
                    ));
                }
                Ok(gofin_exchange::ccxt::HyperliquidSigner {
                    private_key: Zeroizing::new(private_key.clone()),
                    wallet_address: derived,
                })
            }
            _ => Err(GoFinError::new(
                "CREDENTIAL_TYPE_MISMATCH",
                "credentials",
                "Development credential is not an EVM wallet key.",
                "Import the disposable EVM key for this Hyperliquid owner.",
            )),
        })
    }

    fn kraken_nonces(&self) -> Result<Arc<dyn gofin_exchange::kraken::NonceAllocator>> {
        Ok(Arc::new(gofin_broker::kraken::DurableKrakenNonces::open(
            &self.directory.join("nonces.sqlite"),
        )?))
    }
    fn envelope_signer(
        &self,
        reference: &CredentialRef,
    ) -> Result<(String, Arc<dyn gofin_evm::EnvelopeSigner>)> {
        self.material(reference, |material| match material {
            SecretMaterial::EvmPrivateKey { private_key } => {
                let signer = gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)?;
                Ok((
                    format!("{:#x}", signer.address()),
                    Arc::new(signer) as Arc<dyn gofin_evm::EnvelopeSigner>,
                ))
            }
            _ => Err(GoFinError::validation("Credential type mismatch.")),
        })
    }

    fn solana_signer(&self, reference: &CredentialRef) -> Result<Arc<gofin_solana::SolanaSigner>> {
        self.material(reference, |material| match material {
            SecretMaterial::SolanaPrivateKey { private_key } => {
                Ok(Arc::new(gofin_solana::SolanaSigner::parse(private_key)?))
            }
            _ => Err(GoFinError::validation("Credential type mismatch.")),
        })
    }

    fn jupiter_api_key(&self, reference: &CredentialRef) -> Result<Zeroizing<String>> {
        self.material(reference, |material| match material {
            SecretMaterial::JupiterApiKey { api_key } => Ok(Zeroizing::new(api_key.clone())),
            _ => Err(GoFinError::validation("Credential type mismatch.")),
        })
    }

    fn relay_api_key(&self, reference: &CredentialRef) -> Result<Zeroizing<String>> {
        self.material(reference, |material| match material {
            SecretMaterial::RelayApiKey { api_key } => Ok(Zeroizing::new(api_key.clone())),
            _ => Err(GoFinError::validation("Credential type mismatch.")),
        })
    }

    fn usd_price_source(
        &self,
        reference: &CredentialRef,
        plan: &str,
    ) -> Result<Arc<dyn gofin_pricing::UsdPriceSource>> {
        self.material(reference, |material| match material {
            SecretMaterial::CoinGeckoApiKey {
                api_key,
                plan: stored,
            } if stored == plan => {
                let plan = match plan {
                    "demo" => gofin_pricing::ApiPlan::Demo,
                    "pro" => gofin_pricing::ApiPlan::Pro,
                    _ => return Err(GoFinError::validation("Pricing plan mismatch.")),
                };
                Ok(
                    Arc::new(gofin_pricing::CoinGeckoPrices::with_api_key(plan, api_key)?)
                        as Arc<dyn gofin_pricing::UsdPriceSource>,
                )
            }
            _ => Err(GoFinError::validation("Credential type mismatch.")),
        })
    }
}

#[cfg(test)]
mod hyperliquid_signer_tests {
    use super::*;

    fn development_provider() -> (tempfile::TempDir, DevelopmentCredentialProvider) {
        let directory = tempfile::tempdir().unwrap();
        create_store(directory.path()).unwrap();
        let provider = DevelopmentCredentialProvider::new(directory.path().to_path_buf()).unwrap();
        (directory, provider)
    }

    #[test]
    fn hyperliquid_signer_accepts_matching_synthetic_evm_key() {
        let (directory, provider) = development_provider();
        let private_key = "01".repeat(32);
        let material = SecretMaterial::EvmPrivateKey {
            private_key: private_key.clone(),
        };
        let reference = write_material(directory.path(), &material).unwrap();
        let expected_address = format!(
            "{:#x}",
            gofin_evm::LocalEnvelopeSigner::from_private_key(&private_key)
                .unwrap()
                .address()
        );

        let signer = provider
            .hyperliquid_signer(reference.as_str(), &expected_address.to_uppercase())
            .unwrap();
        assert_eq!(signer.wallet_address, expected_address);
        assert_eq!(signer.private_key.as_str(), private_key);
    }

    #[test]
    fn hyperliquid_signer_rejects_wrong_owner_and_non_evm_material() {
        let (directory, provider) = development_provider();
        let key_reference = write_material(
            directory.path(),
            &SecretMaterial::EvmPrivateKey {
                private_key: "02".repeat(32),
            },
        )
        .unwrap();
        let result = provider.hyperliquid_signer(
            key_reference.as_str(),
            "0x0000000000000000000000000000000000000001",
        );
        assert!(matches!(result, Err(error) if error.code == "SIGNER_ADDRESS_MISMATCH"));

        let other_reference = write_material(
            directory.path(),
            &SecretMaterial::BinanceHmac {
                api_key: "SYNTHETIC-API-KEY".into(),
                secret: "SYNTHETIC-SECRET".into(),
            },
        )
        .unwrap();
        let result = provider.hyperliquid_signer(
            other_reference.as_str(),
            "0x0000000000000000000000000000000000000001",
        );
        assert!(matches!(result, Err(error) if error.code == "CREDENTIAL_TYPE_MISMATCH"));
    }

    #[test]
    fn hyperliquid_signer_sanitizes_invalid_scalar_error() {
        let (directory, provider) = development_provider();
        let reference = write_material(
            directory.path(),
            &SecretMaterial::EvmPrivateKey {
                private_key: "00".repeat(32),
            },
        )
        .unwrap();
        let result = provider.hyperliquid_signer(
            reference.as_str(),
            "0x0000000000000000000000000000000000000001",
        );
        assert!(matches!(result, Err(error) if error.code == "CREDENTIAL_INVALID"));
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

fn unavailable() -> GoFinError {
    GoFinError::new(
        "DEV_CREDENTIAL_UNAVAILABLE",
        "credentials",
        "Development credentials could not be safely accessed.",
        "Re-import the dedicated development credentials file.",
    )
}
