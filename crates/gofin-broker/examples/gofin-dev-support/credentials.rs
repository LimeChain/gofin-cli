use gofin_broker::BrokerCredentialProvider;
use gofin_credentials::{CredentialRef, SecretMaterial};
use gofin_domain::{GoFinError, Result};
use gofin_exchange::ccxt::{CcxtError, CcxtErrorKind, CredentialProvider, Credentials};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroizing;

pub struct DevelopmentCredentialProvider {
    directory: PathBuf,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DevelopmentPapertradeState {
    version: u32,
    #[serde(default)]
    sessions: Vec<gofin_credentials::PapertradeSessionRegistration>,
    #[serde(default)]
    runtime_records: BTreeMap<String, serde_json::Value>,
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

    fn with_papertrade_state<T>(
        &self,
        update: impl FnOnce(&mut DevelopmentPapertradeState) -> Result<T>,
    ) -> Result<T> {
        let lock_path = self.directory.join(".papertrade-runtime.lock");
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let lock = options.open(lock_path).map_err(|_| unavailable())?;
        let metadata = lock.metadata().map_err(|_| unavailable())?;
        #[cfg(unix)]
        if !metadata.is_file()
            || metadata.mode() & 0o777 != 0o600
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(unavailable());
        }
        // SAFETY: flock operates on this live file descriptor; the descriptor
        // remains owned by `lock` through the complete read/modify/write.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(unavailable());
        }
        let mut state = self.read_papertrade_state()?;
        let result = update(&mut state)?;
        self.write_papertrade_state(&state)?;
        Ok(result)
    }

    fn read_papertrade_state(&self) -> Result<DevelopmentPapertradeState> {
        let path = self.directory.join("papertrade-runtime-v1.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DevelopmentPapertradeState {
                    version: 1,
                    ..DevelopmentPapertradeState::default()
                });
            }
            Err(_) => return Err(unavailable()),
        };
        #[cfg(unix)]
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o777 != 0o600
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(unavailable());
        }
        if metadata.len() > 16 * 1024 * 1024 {
            return Err(unavailable());
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path).map_err(|_| unavailable())?;
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(unavailable());
        }
        let state: DevelopmentPapertradeState =
            serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        if state.version != 1 {
            return Err(unavailable());
        }
        Ok(state)
    }

    fn write_papertrade_state(&self, state: &DevelopmentPapertradeState) -> Result<()> {
        let bytes = Zeroizing::new(serde_json::to_vec(state).map_err(|_| unavailable())?);
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(unavailable());
        }
        let target = self.directory.join("papertrade-runtime-v1.json");
        let temporary = self
            .directory
            .join(format!(".papertrade-runtime-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options.open(&temporary).map_err(|_| unavailable())?;
        if file
            .write_all(&bytes)
            .and_then(|_| file.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&temporary);
            return Err(unavailable());
        }
        drop(file);
        if fs::rename(&temporary, &target).is_err()
            || fs::File::open(&self.directory)
                .and_then(|directory| directory.sync_all())
                .is_err()
        {
            let _ = fs::remove_file(&temporary);
            return Err(unavailable());
        }
        Ok(())
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
    fn papertrade_session_signer(
        &self,
        registration_id: &str,
        owner: &str,
        expires_at: u64,
    ) -> Result<(CredentialRef, String, Arc<gofin_evm::LocalEnvelopeSigner>)> {
        let operation = uuid::Uuid::parse_str(registration_id).map_err(|_| {
            GoFinError::validation("Invalid development Papertrade registration ID.")
        })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| GoFinError::validation("Development clock is invalid."))?
            .as_secs();
        if operation.hyphenated().to_string() != registration_id
            || !gofin_hyperliquid::valid_address(owner)
            || expires_at <= now
            || expires_at - now > gofin_papertrade::MAX_SESSION_SECONDS
        {
            return Err(GoFinError::validation(
                "Development Papertrade session registration is outside its supported owner or expiry bounds.",
            ));
        }
        let reference = gofin_credentials::papertrade_session_signer_reference(
            registration_id,
            owner,
            expires_at,
        )?;
        let target = self.directory.join(format!("{}.json", reference.as_str()));
        if !target.exists() {
            use std::io::Write;
            let mut private_key = Zeroizing::new(String::new());
            let signer = loop {
                let mut raw = Zeroizing::new([0u8; 32]);
                fs::File::open("/dev/urandom")
                    .and_then(|mut random| random.read_exact(raw.as_mut()))
                    .map_err(|_| unavailable())?;
                *private_key = hex::encode(raw.as_ref());
                if let Ok(signer) = gofin_evm::LocalEnvelopeSigner::from_private_key(&private_key) {
                    break signer;
                }
            };
            let _ = signer;
            let material = SecretMaterial::GeneratedPapertradeSessionSigner {
                owner_address: owner.to_ascii_lowercase(),
                chain_id: 999,
                session_manager: gofin_papertrade::SESSION_KEY_MANAGER.to_owned(),
                operation_key: registration_id.to_owned(),
                expires_at,
                private_key: private_key.to_string(),
            };
            material.validate_present()?;
            let bytes = Zeroizing::new(serde_json::to_vec(&material).map_err(|_| unavailable())?);
            let temporary = self.directory.join(format!(
                ".papertrade-session-{}-{}.tmp",
                reference.as_str(),
                uuid::Uuid::new_v4()
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let mut file = options.open(&temporary).map_err(|_| unavailable())?;
            let result = file.write_all(&bytes).and_then(|_| file.sync_all());
            drop(file);
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
                return Err(unavailable());
            }
            match fs::hard_link(&temporary, &target) {
                Ok(()) => {
                    let _ = fs::remove_file(&temporary);
                    fs::File::open(&self.directory)
                        .and_then(|directory| directory.sync_all())
                        .map_err(|_| unavailable())?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&temporary);
                }
                Err(_) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(unavailable());
                }
            }
        }
        self.material(&reference, |material| match material {
            SecretMaterial::GeneratedPapertradeSessionSigner {
                owner_address,
                chain_id: 999,
                session_manager,
                operation_key,
                expires_at: stored_expiry,
                private_key,
            } if owner_address.eq_ignore_ascii_case(owner)
                && session_manager.eq_ignore_ascii_case(gofin_papertrade::SESSION_KEY_MANAGER)
                && operation_key == registration_id
                && *stored_expiry == expires_at =>
            {
                let signer = Arc::new(gofin_evm::LocalEnvelopeSigner::from_private_key(
                    private_key,
                )?);
                Ok((
                    reference.clone(),
                    format!("{:#x}", signer.address()),
                    signer,
                ))
            }
            _ => Err(unavailable()),
        })
    }

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

    fn generated_solana_signer(
        &self,
        reference: &CredentialRef,
    ) -> Result<Arc<gofin_solana::SolanaSigner>> {
        let private_key = self.material(reference, |material| match material {
            SecretMaterial::GeneratedSolanaSigner { private_key, .. } => {
                Ok(Zeroizing::new(private_key.clone()))
            }
            _ => Err(unavailable()),
        })?;
        Ok(Arc::new(gofin_solana::SolanaSigner::parse(&private_key)?))
    }

    fn create_solana_auxiliary_signer(
        &self,
        operation_key: &str,
        owner_address: &str,
        purpose: &str,
    ) -> Result<(CredentialRef, Arc<gofin_solana::SolanaSigner>)> {
        use sha2::{Digest, Sha256};
        let operation = uuid::Uuid::parse_str(operation_key).map_err(|_| {
            GoFinError::validation("Invalid development auxiliary signer operation identity.")
        })?;
        if operation.hyphenated().to_string() != operation_key {
            return Err(GoFinError::validation(
                "Invalid development auxiliary signer operation identity.",
            ));
        }
        if !matches!(
            purpose,
            "meteora_position_nft_mint"
                | "meteora_temporary_wsol"
                | "papertrade_cctp_message_sent_event"
        ) {
            return Err(GoFinError::validation(
                "Unsupported development auxiliary signer purpose.",
            ));
        }
        let digest = Sha256::digest(
            format!("fin:dev:solana:auxiliary:{operation_key}:{owner_address}:{purpose}")
                .as_bytes(),
        );
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&digest[..16]);
        id_bytes[6] = (id_bytes[6] & 0x0f) | 0x40;
        id_bytes[8] = (id_bytes[8] & 0x3f) | 0x80;
        let reference =
            CredentialRef::new(&uuid::Uuid::from_bytes(id_bytes).hyphenated().to_string())?;
        cleanup_dev_aux_staging(&self.directory, &reference);
        let target = self.directory.join(format!("{}.json", reference.as_str()));
        if !target.exists() {
            use ed25519_dalek::SigningKey;
            let mut seed = Zeroizing::new([0u8; 32]);
            fs::File::open("/dev/urandom")
                .and_then(|mut random| random.read_exact(seed.as_mut()))
                .map_err(|_| unavailable())?;
            let signing_key = SigningKey::from_bytes(&seed);
            let mut keypair = Zeroizing::new([0u8; 64]);
            keypair[..32].copy_from_slice(seed.as_ref());
            keypair[32..].copy_from_slice(&signing_key.verifying_key().to_bytes());
            let private_key = Zeroizing::new(
                serde_json::to_string(&keypair.as_ref()).map_err(|_| unavailable())?,
            );
            let material = SecretMaterial::GeneratedSolanaSigner {
                owner_address: owner_address.to_owned(),
                purpose: purpose.to_owned(),
                operation_key: operation.hyphenated().to_string(),
                private_key: private_key.to_string(),
            };
            material.validate_present()?;
            let bytes = Zeroizing::new(serde_json::to_vec(&material).map_err(|_| unavailable())?);
            let temporary = self.directory.join(format!(
                ".aux-{}-{}.tmp",
                reference.as_str(),
                uuid::Uuid::new_v4()
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            }
            let mut file = options.open(&temporary).map_err(|_| unavailable())?;
            use std::io::Write;
            let write_result = file.write_all(&bytes).and_then(|_| file.sync_all());
            drop(file);
            if write_result.is_err() {
                let _ = fs::remove_file(&temporary);
                return Err(unavailable());
            }
            match fs::hard_link(&temporary, &target) {
                Ok(()) => {
                    let _ = fs::remove_file(&temporary);
                    fs::File::open(&self.directory)
                        .and_then(|directory| directory.sync_all())
                        .map_err(|_| unavailable())?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&temporary);
                }
                Err(_) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(unavailable());
                }
            }
        }
        let material = self.material(&reference, |material| match material {
            SecretMaterial::GeneratedSolanaSigner {
                owner_address: stored_owner,
                purpose: stored_purpose,
                operation_key: stored_operation,
                private_key,
            } if stored_owner == owner_address
                && stored_purpose == purpose
                && stored_operation == operation_key =>
            {
                Ok(Zeroizing::new(private_key.clone()))
            }
            _ => Err(unavailable()),
        })?;
        Ok((
            reference,
            Arc::new(gofin_solana::SolanaSigner::parse(&material)?),
        ))
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

    fn papertrade_wallet_signer(
        &self,
        reference: &CredentialRef,
        owner: &str,
    ) -> Result<Arc<gofin_evm::LocalEnvelopeSigner>> {
        if !gofin_hyperliquid::valid_address(owner) {
            return Err(GoFinError::validation(
                "Invalid development Papertrade owner.",
            ));
        }
        self.material(reference, |material| match material {
            SecretMaterial::EvmPrivateKey { private_key } => {
                let signer = Arc::new(gofin_evm::LocalEnvelopeSigner::from_private_key(private_key)?);
                if !format!("{:#x}", signer.address()).eq_ignore_ascii_case(owner) {
                    return Err(GoFinError::new(
                        "SIGNER_ADDRESS_MISMATCH",
                        "signing",
                        "The development wallet key does not derive the configured Papertrade owner.",
                        "Correct the disposable wallet mapping or re-import its credential.",
                    ));
                }
                Ok(signer)
            }
            _ => Err(GoFinError::new(
                "CREDENTIAL_TYPE_MISMATCH",
                "credentials",
                "Development credential is not an EVM wallet key.",
                "Import the disposable EVM key mapped to this Papertrade owner.",
            )),
        })
    }

    fn papertrade_sessions(
        &self,
        owner: &str,
    ) -> Result<Vec<gofin_credentials::PapertradeSessionRegistration>> {
        if !gofin_hyperliquid::valid_address(owner) {
            return Err(GoFinError::validation(
                "Invalid development Papertrade owner.",
            ));
        }
        let mut sessions = self.with_papertrade_state(|state| Ok(state.sessions.clone()))?;
        sessions.retain(|session| session.owner_address.eq_ignore_ascii_case(owner));
        sessions.sort_by(|left, right| left.registration_id.cmp(&right.registration_id));
        Ok(sessions)
    }

    fn papertrade_runtime_record(&self, key: &str) -> Result<Option<serde_json::Value>> {
        validate_dev_runtime_key(key)?;
        self.with_papertrade_state(|state| Ok(state.runtime_records.get(key).cloned()))
    }

    fn put_papertrade_runtime_record(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        validate_dev_runtime_key(key)?;
        let encoded = serde_json::to_vec(value).map_err(|_| unavailable())?;
        if encoded.len() > 1024 * 1024 {
            return Err(unavailable());
        }
        self.with_papertrade_state(|state| {
            if let Some(existing) = state.runtime_records.get(key) {
                if existing == value {
                    return Ok(());
                }
                return Err(GoFinError::new(
                    "PAPERTRADE_IDENTITY_CONFLICT",
                    "credentials",
                    "A development Papertrade runtime identity is already bound to different content.",
                    "Resume the original development operation instead of reusing its identity.",
                ));
            }
            state.runtime_records.insert(key.to_owned(), value.clone());
            Ok(())
        })
    }

    fn activate_papertrade_session(
        &self,
        registration_id: &str,
        owner: &str,
        expiry: u64,
        registered_at: u64,
    ) -> Result<()> {
        validate_dev_session_identity(registration_id, owner)?;
        if expiry == 0 || registered_at == 0 || registered_at > expiry {
            return Err(GoFinError::validation(
                "Invalid development Papertrade session times.",
            ));
        }
        let (credential_ref, _, _) =
            self.papertrade_session_signer(registration_id, owner, expiry)?;
        self.with_papertrade_state(|state| {
            if let Some(existing) = state.sessions.iter().find(|item| item.registration_id == registration_id) {
                if !existing.owner_address.eq_ignore_ascii_case(owner)
                    || existing.expires_at != expiry
                    || existing.credential_ref != credential_ref
                    || existing.revoked_at.is_some()
                {
                    return Err(GoFinError::new(
                        "PAPERTRADE_SESSION_ID_CONFLICT",
                        "credentials",
                        "The development Papertrade registration identity is already bound or revoked.",
                        "Recover the original development registration instead of reusing it.",
                    ));
                }
                return Ok(());
            }
            state.sessions.push(gofin_credentials::PapertradeSessionRegistration {
                owner_address: owner.to_ascii_lowercase(),
                registration_id: registration_id.to_owned(),
                expires_at: expiry,
                credential_ref,
                registered_at,
                revoked_at: None,
            });
            Ok(())
        })
    }

    fn deactivate_papertrade_session(
        &self,
        registration_id: &str,
        owner: &str,
        revoked_at: u64,
    ) -> Result<()> {
        validate_dev_session_identity(registration_id, owner)?;
        if revoked_at == 0 {
            return Err(GoFinError::validation(
                "Invalid development Papertrade revocation time.",
            ));
        }
        self.with_papertrade_state(|state| {
            let session = state
                .sessions
                .iter_mut()
                .find(|item| item.registration_id == registration_id)
                .ok_or_else(|| {
                    GoFinError::new(
                        "PAPERTRADE_SESSION_NOT_REGISTERED",
                        "credentials",
                        "No development Papertrade registration exists for this identity.",
                        "Recover the original development registration before revoking it.",
                    )
                })?;
            if !session.owner_address.eq_ignore_ascii_case(owner) {
                return Err(GoFinError::new(
                    "PAPERTRADE_SESSION_OWNER_MISMATCH",
                    "credentials",
                    "The development Papertrade registration belongs to another owner.",
                    "Use the owner recorded for the original development registration.",
                ));
            }
            session.revoked_at.get_or_insert(revoked_at);
            Ok(())
        })
    }
}

fn validate_dev_session_identity(registration_id: &str, owner: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(registration_id)
        .map_err(|_| GoFinError::validation("Invalid development Papertrade registration ID."))?;
    if parsed.hyphenated().to_string() != registration_id
        || !gofin_hyperliquid::valid_address(owner)
    {
        return Err(GoFinError::validation(
            "Invalid development Papertrade session identity.",
        ));
    }
    Ok(())
}

fn validate_dev_runtime_key(key: &str) -> Result<()> {
    if key.is_empty()
        // Funding leg keys preserve the full engine operation identity in
        // hex; 512 bytes covers the generated named-plan operation format.
        || key.len() > 512
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
    {
        return Err(GoFinError::validation(
            "Invalid development Papertrade runtime key.",
        ));
    }
    Ok(())
}

fn cleanup_dev_aux_staging(directory: &Path, reference: &CredentialRef) {
    let prefix = format!(".aux-{}-", reference.as_str());
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
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

pub fn remove_material(directory: &Path, reference: &CredentialRef) -> Result<()> {
    // Reuse the private-file checks before unlinking an environment-owned key.
    DevelopmentCredentialProvider::new(directory.to_path_buf())?.material(reference, |_| Ok(()))?;
    fs::remove_file(directory.join(format!("{}.json", reference.as_str())))
        .map_err(|_| unavailable())?;
    fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| unavailable())
}

fn unavailable() -> GoFinError {
    GoFinError::new(
        "DEV_CREDENTIAL_UNAVAILABLE",
        "credentials",
        "Development credentials could not be safely accessed.",
        "Re-import the dedicated development credentials file.",
    )
}

#[cfg(test)]
mod papertrade_session_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn development_provider() -> (tempfile::TempDir, DevelopmentCredentialProvider) {
        let directory = tempfile::tempdir().unwrap();
        create_store(directory.path()).unwrap();
        let provider = DevelopmentCredentialProvider::new(directory.path().to_path_buf()).unwrap();
        (directory, provider)
    }

    #[test]
    fn generated_session_key_is_persistent_and_bound_to_registration_owner_and_expiry() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let provider = DevelopmentCredentialProvider::new(directory.path().to_path_buf()).unwrap();
        let registration = "5d122760-1964-4f2c-8db8-209b452beaca";
        let owner = "0x0000000000000000000000000000000000000001";
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let (reference, address, _) = provider
            .papertrade_session_signer(registration, owner, expiry)
            .unwrap();
        let (recovered, same_address, _) = provider
            .papertrade_session_signer(registration, owner, expiry)
            .unwrap();
        assert_eq!(reference, recovered);
        assert_eq!(address, same_address);
        assert!(
            provider
                .papertrade_session_signer(
                    registration,
                    "0x0000000000000000000000000000000000000002",
                    expiry,
                )
                .is_ok()
        );
        assert!(
            provider
                .papertrade_session_signer(registration, owner, expiry + 1)
                .is_ok()
        );
        let stored = provider
            .material(&reference, |material| match material {
                SecretMaterial::GeneratedPapertradeSessionSigner { private_key, .. } => {
                    Ok(Zeroizing::new(private_key.clone()))
                }
                _ => Err(unavailable()),
            })
            .unwrap();
        assert_eq!(stored.len(), 64);
    }

    #[test]
    fn wallet_signer_checks_owner_and_runtime_records_are_immutable_and_durable() {
        let (directory, provider) = development_provider();
        let private_key = "01".repeat(32);
        let reference = write_material(
            directory.path(),
            &SecretMaterial::EvmPrivateKey { private_key },
        )
        .unwrap();
        let owner = format!(
            "{:#x}",
            gofin_evm::LocalEnvelopeSigner::from_private_key(&"01".repeat(32))
                .unwrap()
                .address()
        );
        assert!(
            provider
                .papertrade_wallet_signer(&reference, &owner)
                .is_ok()
        );
        assert!(matches!(
            provider.papertrade_wallet_signer(
                &reference,
                "0x0000000000000000000000000000000000000002"
            ),
            Err(error) if error.code == "SIGNER_ADDRESS_MISMATCH"
        ));

        let record = serde_json::json!({"version": 1, "state": "prepared"});
        provider
            .put_papertrade_runtime_record("papertrade-test-operation", &record)
            .unwrap();
        provider
            .put_papertrade_runtime_record("papertrade-test-operation", &record)
            .unwrap();
        assert!(matches!(
            provider.put_papertrade_runtime_record(
                "papertrade-test-operation",
                &serde_json::json!({"version": 1, "state": "different"})
            ),
            Err(error) if error.code == "PAPERTRADE_IDENTITY_CONFLICT"
        ));
        let reopened = DevelopmentCredentialProvider::new(directory.path().to_path_buf()).unwrap();
        assert_eq!(
            reopened
                .papertrade_runtime_record("papertrade-test-operation")
                .unwrap(),
            Some(record)
        );
        let metadata = fs::metadata(directory.path().join("papertrade-runtime-v1.json")).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
    }

    #[test]
    fn generated_named_plan_funding_leg_key_is_bounded_and_durable() {
        let (directory, provider) = development_provider();
        let operation_id = "plan-execution:4ef633d3-5e2e-4cf2-9ea3-1ef193f976fb:ceff6b4d-f0cb-4cec-825c-9cc2b40d47a8:deposit:e7667423-8322-41cf-95b6-df758fecb963";
        let leg_id = format!("papertrade-funding:{operation_id}:0");
        let key = format!(
            "papertrade-funding-leg:{}:dispatch_attempted",
            hex::encode(leg_id.as_bytes())
        );
        assert!(key.len() > 256);
        assert!(key.len() <= 512);
        let record = serde_json::json!({"version":1,"attempted":true});
        provider
            .put_papertrade_runtime_record(&key, &record)
            .unwrap();

        let reopened = DevelopmentCredentialProvider::new(directory.path().to_path_buf()).unwrap();
        assert_eq!(
            reopened.papertrade_runtime_record(&key).unwrap(),
            Some(record)
        );
    }

    #[test]
    fn development_session_registry_activates_and_revokes_without_reactivation() {
        let (directory, provider) = development_provider();
        let registration = "5d122760-1964-4f2c-8db8-209b452beaca";
        let owner = "0x0000000000000000000000000000000000000001";
        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        provider
            .activate_papertrade_session(registration, owner, expiry, expiry - 30)
            .unwrap();
        let active = provider.papertrade_sessions(owner).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].registration_id, registration);
        assert_eq!(active[0].revoked_at, None);
        provider
            .deactivate_papertrade_session(registration, owner, expiry - 10)
            .unwrap();
        assert_eq!(
            provider.papertrade_sessions(owner).unwrap()[0].revoked_at,
            Some(expiry - 10)
        );
        assert!(
            provider
                .activate_papertrade_session(registration, owner, expiry, expiry - 30)
                .is_err()
        );
        assert!(
            provider
                .deactivate_papertrade_session(
                    registration,
                    "0x0000000000000000000000000000000000000002",
                    expiry - 9
                )
                .is_err()
        );
        let reopened = DevelopmentCredentialProvider::new(directory.path().to_path_buf()).unwrap();
        assert_eq!(
            reopened.papertrade_sessions(owner).unwrap()[0].revoked_at,
            Some(expiry - 10)
        );
    }
}
