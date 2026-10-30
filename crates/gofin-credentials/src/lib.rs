//! Protected in-process storage. The client protocol has no secret-export method.
//! Based on a reviewed Ogi Spark/GLM draft; buffers and partial writes fail closed.
use gofin_domain::{GoFinError, Result};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const CREDENTIAL_DIR: &str = "/Library/GoFin/private/credentials";
const MAX_BYTES: usize = 32_768;
fn error(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "credentials",
        message,
        "Run protected connection enrollment in your own human terminal.",
    )
}

fn valid_evm_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_secp256k1_private_key(value: &str) -> bool {
    const ORDER: [u8; 32] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36,
        0x41, 0x41,
    ];
    let raw = value.strip_prefix("0x").unwrap_or(value);
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let Ok(bytes) = hex::decode(raw).map(Zeroizing::new) else {
        return false;
    };
    bytes.iter().any(|byte| *byte != 0) && bytes.as_slice() < ORDER.as_slice()
}

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretMaterial {
    KrakenApi {
        api_key: String,
        secret: String,
    },
    KrakenFuturesApi {
        api_key: String,
        secret: String,
    },
    BinanceHmac {
        api_key: String,
        secret: String,
    },
    BinanceRsa {
        api_key: String,
        private_key: String,
    },
    BinanceEd25519 {
        api_key: String,
        private_key: String,
    },
    #[serde(alias = "ethereum_private_key")]
    EvmPrivateKey {
        private_key: String,
    },
    SolanaPrivateKey {
        private_key: String,
    },
    /// Broker-created Solana auxiliary signer bound to one owner, purpose and
    /// durable logical operation. The private key never enters public data.
    GeneratedSolanaSigner {
        owner_address: String,
        purpose: String,
        operation_key: String,
        private_key: String,
    },
    /// Broker-created signer for one Papertrade session registration. Its
    /// identity is fixed to the owner, chain, session manager, expiry, and
    /// durable logical operation. Revocation never deletes this credential;
    /// the retained key supports recovery of uncertain registration/revoke
    /// outcomes without granting new authority.
    GeneratedPapertradeSessionSigner {
        owner_address: String,
        chain_id: u64,
        session_manager: String,
        operation_key: String,
        expires_at: u64,
        private_key: String,
    },
    JupiterApiKey {
        api_key: String,
    },
    RelayApiKey {
        api_key: String,
    },
    CoinGeckoApiKey {
        api_key: String,
        plan: String,
    },
    CoinbaseCdpEd25519 {
        api_key: String,
        private_key_base64: String,
    },
    CoinbaseCdpEcdsa {
        api_key: String,
        private_key_pem: String,
    },
    CoinbaseExchangeHmac {
        api_key: String,
        secret: String,
        passphrase: String,
    },
    CoinbaseInternationalHmac {
        api_key: String,
        secret: String,
        passphrase: String,
    },
}
impl fmt::Debug for SecretMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretMaterial(<redacted>)")
    }
}
impl SecretMaterial {
    pub fn validate_present(&self) -> Result<()> {
        let valid = match self {
            Self::KrakenApi { api_key, secret } | Self::KrakenFuturesApi { api_key, secret } => {
                use base64::Engine;
                !api_key.is_empty()
                    && api_key.len() <= 256
                    && api_key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b))
                    && base64::engine::general_purpose::STANDARD
                        .decode(secret)
                        .is_ok_and(|key| !key.is_empty() && key.len() <= 256)
            }
            Self::BinanceHmac { api_key, secret } => !api_key.is_empty() && !secret.is_empty(),
            Self::BinanceRsa {
                api_key,
                private_key,
            } => !api_key.is_empty() && private_key.contains("PRIVATE KEY"),
            Self::BinanceEd25519 {
                api_key,
                private_key,
            } => {
                use ed25519_dalek::pkcs8::DecodePrivateKey;
                !api_key.is_empty()
                    && ed25519_dalek::SigningKey::from_pkcs8_pem(private_key).is_ok()
            }
            Self::EvmPrivateKey { private_key } => {
                let raw = private_key.strip_prefix("0x").unwrap_or(private_key);
                raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit())
            }
            // The broker performs canonical base58/JSON64 parsing with the
            // reviewed Solana signer before saving a connection. Keep this
            // storage layer independent of the runtime/signing crate.
            Self::SolanaPrivateKey { private_key } => !private_key.trim().is_empty(),
            Self::GeneratedSolanaSigner {
                owner_address,
                purpose,
                operation_key,
                private_key,
            } => {
                !private_key.trim().is_empty()
                    && !owner_address.trim().is_empty()
                    && owner_address.len() <= 64
                    && matches!(
                        purpose.as_str(),
                        "meteora_position_nft_mint" | "meteora_temporary_wsol"
                    )
                    && uuid::Uuid::parse_str(operation_key)
                        .is_ok_and(|id| id.hyphenated().to_string() == *operation_key)
            }
            Self::GeneratedPapertradeSessionSigner {
                owner_address,
                chain_id,
                session_manager,
                operation_key,
                expires_at,
                private_key,
            } => {
                *chain_id == 999
                    && valid_evm_address(owner_address)
                    && session_manager
                        .eq_ignore_ascii_case("0x8326eb8896e358717f10502ec73ce8d28e37b108")
                    && uuid::Uuid::parse_str(operation_key)
                        .is_ok_and(|id| id.hyphenated().to_string() == *operation_key)
                    && *expires_at > 0
                    && valid_secp256k1_private_key(private_key)
            }
            Self::JupiterApiKey { api_key } | Self::RelayApiKey { api_key } => {
                !api_key.is_empty()
                    && api_key.len() <= 1024
                    && api_key.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
            }
            Self::CoinGeckoApiKey { api_key, plan } => {
                matches!(plan.as_str(), "demo" | "pro")
                    && !api_key.is_empty()
                    && api_key.len() <= 256
                    && api_key.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
                    })
            }
            Self::CoinbaseCdpEd25519 {
                api_key,
                private_key_base64,
            } => {
                use base64::Engine;
                use ed25519_dalek::SigningKey;
                let decoded = base64::engine::general_purpose::STANDARD.decode(private_key_base64);
                !api_key.is_empty()
                    && api_key.len() <= 256
                    && decoded.is_ok_and(|bytes| {
                        if bytes.len() != 64 {
                            return false;
                        }
                        let Ok(seed) = <[u8; 32]>::try_from(&bytes[..32]) else {
                            return false;
                        };
                        SigningKey::from_bytes(&seed).verifying_key().as_bytes() == &bytes[32..]
                    })
            }
            Self::CoinbaseCdpEcdsa {
                api_key,
                private_key_pem,
            } => {
                use p256::pkcs8::DecodePrivateKey;
                !api_key.is_empty()
                    && api_key.len() <= 256
                    && (p256::ecdsa::SigningKey::from_pkcs8_pem(private_key_pem).is_ok()
                        || p256::SecretKey::from_sec1_pem(private_key_pem).is_ok())
            }
            Self::CoinbaseExchangeHmac {
                api_key,
                secret,
                passphrase,
            }
            | Self::CoinbaseInternationalHmac {
                api_key,
                secret,
                passphrase,
            } => {
                use base64::Engine;
                !api_key.is_empty()
                    && api_key.len() <= 256
                    && !passphrase.is_empty()
                    && passphrase.len() <= 512
                    && base64::engine::general_purpose::STANDARD
                        .decode(secret)
                        .is_ok_and(|bytes| !bytes.is_empty() && bytes.len() <= 256)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(error(
                "INVALID_CREDENTIAL",
                "The required credential format is invalid.",
            ))
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct CredentialRef(String);
impl CredentialRef {
    pub fn new(value: &str) -> Result<Self> {
        let parsed = uuid::Uuid::parse_str(value).map_err(|_| {
            error(
                "INVALID_CREDENTIAL_REFERENCE",
                "The credential handle is invalid.",
            )
        })?;
        if parsed.hyphenated().to_string() != value {
            return Err(error(
                "INVALID_CREDENTIAL_REFERENCE",
                "The credential handle is not canonical.",
            ));
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for CredentialRef {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Self::new(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

fn auxiliary_signer_reference(
    operation_key: &str,
    owner_address: &str,
    purpose: &str,
) -> Result<CredentialRef> {
    if owner_address.trim().is_empty()
        || owner_address.len() > 64
        || !matches!(
            purpose,
            "meteora_position_nft_mint"
                | "meteora_temporary_wsol"
                | "papertrade_cctp_message_sent_event"
        )
    {
        return Err(error(
            "INVALID_CREDENTIAL_REFERENCE",
            "The internal Solana signer owner or purpose is invalid.",
        ));
    }
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"fin-solana-auxiliary-signer-v1\0");
    digest.update(operation_key.as_bytes());
    digest.update([0]);
    digest.update(owner_address.as_bytes());
    digest.update([0]);
    digest.update(purpose.as_bytes());
    let digest = digest.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // The opaque handle is not a secret and is never accepted as key material.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    CredentialRef::new(&uuid::Uuid::from_bytes(bytes).hyphenated().to_string())
}

pub fn papertrade_session_signer_reference(
    operation_key: &str,
    owner_address: &str,
    expires_at: u64,
) -> Result<CredentialRef> {
    let parsed_operation = uuid::Uuid::parse_str(operation_key).map_err(|_| {
        error(
            "INVALID_CREDENTIAL_REFERENCE",
            "The Papertrade registration identity is invalid.",
        )
    })?;
    if parsed_operation.hyphenated().to_string() != operation_key
        || !valid_evm_address(owner_address)
        || expires_at == 0
    {
        return Err(error(
            "INVALID_CREDENTIAL_REFERENCE",
            "The Papertrade session signer owner or expiry is invalid.",
        ));
    }
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"gofin-papertrade-session-signer-v1\0");
    digest.update(operation_key.as_bytes());
    digest.update([0]);
    digest.update(owner_address.to_ascii_lowercase().as_bytes());
    digest.update([0]);
    digest.update(999u64.to_be_bytes());
    digest.update([0]);
    digest.update(b"0x8326eb8896e358717f10502ec73ce8d28e37b108");
    digest.update([0]);
    digest.update(expires_at.to_be_bytes());
    let digest = digest.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    CredentialRef::new(&uuid::Uuid::from_bytes(bytes).hyphenated().to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PapertradeSessionIndex {
    version: u32,
    records: Vec<PapertradeSessionIndexRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PapertradeSessionIndexRecord {
    owner_address: String,
    registration_id: String,
    expires_at: u64,
    credential_ref: CredentialRef,
    registered_at: u64,
    revoked_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PapertradeSessionRegistration {
    pub owner_address: String,
    pub registration_id: String,
    pub expires_at: u64,
    pub credential_ref: CredentialRef,
    pub registered_at: u64,
    pub revoked_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PapertradeRuntimeIndex {
    version: u32,
    records: std::collections::BTreeMap<String, serde_json::Value>,
}

pub struct TrustedCredentialStore {
    dir: PathBuf,
}
impl TrustedCredentialStore {
    pub fn open() -> Result<Self> {
        gofin_authorization::validate_installation()?;
        let dir = PathBuf::from(CREDENTIAL_DIR);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            if !dir.try_exists().map_err(|_| {
                error(
                    "CREDENTIAL_STORE_UNAVAILABLE",
                    "The protected store is unavailable.",
                )
            })? {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&dir)
                    .map_err(|_| {
                        error(
                            "CREDENTIAL_STORE_UNAVAILABLE",
                            "The protected store could not be created.",
                        )
                    })?;
                sync_dir(dir.parent().expect("fixed parent"))?;
            }
            let meta = fs::symlink_metadata(&dir).map_err(|_| {
                error(
                    "CREDENTIAL_STORE_UNAVAILABLE",
                    "The protected store is unavailable.",
                )
            })?;
            if !meta.is_dir()
                || meta.file_type().is_symlink()
                || meta.uid() != 0
                || meta.mode() & 0o777 != 0o700
            {
                return Err(error(
                    "UNSAFE_CREDENTIAL_STORE",
                    "The credential directory is not a protected root-owned directory.",
                ));
            }
            reject_acl(&dir)?;
        }
        #[cfg(not(unix))]
        {
            return Err(error(
                "CREDENTIAL_STORE_UNAVAILABLE",
                "Protected credential storage is unavailable on this platform.",
            ));
        }
        Ok(Self { dir })
    }
    pub fn put(&self, material: &SecretMaterial) -> Result<CredentialRef> {
        gofin_authorization::validate_installation()?;
        material.validate_present()?;
        write_material(&self.dir, material)
    }

    /// Create or recover a broker-owned Solana auxiliary signer bound to a
    /// precise owner and protocol purpose. Deterministic references and an
    /// atomic no-clobber link make concurrent calls converge on one credential.
    pub fn create_or_load_solana_auxiliary_signer(
        &self,
        operation_key: &str,
        owner_address: &str,
        purpose: &str,
    ) -> Result<CredentialRef> {
        gofin_authorization::validate_installation()?;
        let parsed = uuid::Uuid::parse_str(operation_key).map_err(|_| {
            error(
                "INVALID_CREDENTIAL_REFERENCE",
                "The internal Meteora signer operation identity is invalid.",
            )
        })?;
        if parsed.hyphenated().to_string() != operation_key {
            return Err(error(
                "INVALID_CREDENTIAL_REFERENCE",
                "The internal Meteora signer operation identity is not canonical.",
            ));
        }
        if owner_address.trim().is_empty()
            || owner_address.len() > 64
            || !matches!(
                purpose,
                "meteora_position_nft_mint"
                    | "meteora_temporary_wsol"
                    | "papertrade_cctp_message_sent_event"
            )
        {
            return Err(error(
                "INVALID_CREDENTIAL_REFERENCE",
                "The internal Solana signer owner or purpose is invalid.",
            ));
        }
        let reference = auxiliary_signer_reference(operation_key, owner_address, purpose)?;
        create_or_load_auxiliary_signer_at(
            &self.dir,
            &reference,
            operation_key,
            owner_address,
            purpose,
        )?;
        self.validate_solana_auxiliary_signer_record(
            &reference,
            operation_key,
            owner_address,
            purpose,
        )?;
        Ok(reference)
    }

    /// Create or recover the protected EVM signer used for one Papertrade
    /// session registration. There is no input for caller-supplied key bytes,
    /// chain, manager, or purpose; each registration keeps its original key
    /// across retries and later revocation recovery.
    pub fn create_or_load_papertrade_session_signer(
        &self,
        operation_key: &str,
        owner_address: &str,
        expires_at: u64,
    ) -> Result<CredentialRef> {
        gofin_authorization::validate_installation()?;
        let parsed = uuid::Uuid::parse_str(operation_key).map_err(|_| {
            error(
                "INVALID_CREDENTIAL_REFERENCE",
                "The Papertrade registration identity is invalid.",
            )
        })?;
        if parsed.hyphenated().to_string() != operation_key
            || !valid_evm_address(owner_address)
            || expires_at == 0
        {
            return Err(error(
                "INVALID_CREDENTIAL_REFERENCE",
                "The Papertrade owner or registration identity is invalid.",
            ));
        }
        let reference =
            papertrade_session_signer_reference(operation_key, owner_address, expires_at)?;
        create_or_load_papertrade_session_signer_at(
            &self.dir,
            &reference,
            operation_key,
            owner_address,
            expires_at,
        )?;
        self.validate_papertrade_session_signer_record(
            &reference,
            operation_key,
            owner_address,
            expires_at,
        )?;
        Ok(reference)
    }

    /// Record activation only after the caller has verified the canonical
    /// SessionKeyManager registration receipt. Registration IDs are unique
    /// across owners and expiry revisions; the generated key remains in the
    /// credential store so revocation and uncertain-outcome recovery retain
    /// the original identity.
    pub fn mark_papertrade_session_registered(
        &self,
        registration_id: &str,
        owner_address: &str,
        expires_at: u64,
        registered_at: u64,
    ) -> Result<PapertradeSessionRegistration> {
        let credential_ref = self.create_or_load_papertrade_session_signer(
            registration_id,
            owner_address,
            expires_at,
        )?;
        let lock_ref = CredentialRef::new("11111111-1111-4111-8111-111111111111")?;
        let _lock = lock_auxiliary_reference(&self.dir, &lock_ref)?;
        let mut index = read_session_index(&self.dir)?;
        if let Some(record) = index
            .records
            .iter_mut()
            .find(|record| record.registration_id == registration_id)
        {
            if !record.owner_address.eq_ignore_ascii_case(owner_address)
                || record.expires_at != expires_at
                || record.credential_ref != credential_ref
            {
                return Err(error(
                    "PAPERTRADE_SESSION_ID_CONFLICT",
                    "The Papertrade registration ID is already bound to another owner or key.",
                ));
            }
            if record.revoked_at.is_some() {
                return Err(error(
                    "PAPERTRADE_SESSION_REVOKED",
                    "A revoked Papertrade session registration cannot be reactivated.",
                ));
            }
        } else {
            index.records.push(PapertradeSessionIndexRecord {
                owner_address: owner_address.to_ascii_lowercase(),
                registration_id: registration_id.to_owned(),
                expires_at,
                credential_ref,
                registered_at,
                revoked_at: None,
            });
        }
        write_session_index(&self.dir, &index)?;
        papertrade_session_registrations(self, index, owner_address)?
            .into_iter()
            .find(|record| record.registration_id == registration_id)
            .ok_or_else(|| {
                error(
                    "CREDENTIAL_UNAVAILABLE",
                    "Papertrade registration was not persisted.",
                )
            })
    }

    /// Retain the credential and registration identity after canonical
    /// revocation; this supports recovery and prevents accidental key reuse.
    pub fn mark_papertrade_session_revoked(
        &self,
        registration_id: &str,
        owner_address: &str,
        revoked_at: u64,
    ) -> Result<PapertradeSessionRegistration> {
        let lock_ref = CredentialRef::new("11111111-1111-4111-8111-111111111111")?;
        let _lock = lock_auxiliary_reference(&self.dir, &lock_ref)?;
        let mut index = read_session_index(&self.dir)?;
        let record = index
            .records
            .iter_mut()
            .find(|record| record.registration_id == registration_id)
            .ok_or_else(|| {
                error(
                    "PAPERTRADE_SESSION_NOT_REGISTERED",
                    "No durable Papertrade registration exists for this identity.",
                )
            })?;
        if !record.owner_address.eq_ignore_ascii_case(owner_address) {
            return Err(error(
                "PAPERTRADE_SESSION_OWNER_MISMATCH",
                "The Papertrade registration belongs to another owner.",
            ));
        }
        record.revoked_at.get_or_insert(revoked_at);
        write_session_index(&self.dir, &index)?;
        papertrade_session_registrations(self, index, owner_address)?
            .into_iter()
            .find(|record| record.registration_id == registration_id)
            .ok_or_else(|| {
                error(
                    "CREDENTIAL_UNAVAILABLE",
                    "Papertrade registration was not persisted.",
                )
            })
    }

    pub fn papertrade_sessions(
        &self,
        owner_address: &str,
    ) -> Result<Vec<PapertradeSessionRegistration>> {
        let lock_ref = CredentialRef::new("11111111-1111-4111-8111-111111111111")?;
        let _lock = lock_auxiliary_reference(&self.dir, &lock_ref)?;
        papertrade_session_registrations(self, read_session_index(&self.dir)?, owner_address)
    }

    /// Read a broker-owned immutable Papertrade plan or frozen configuration
    /// transaction. Records contain no secret key material or signatures.
    pub fn papertrade_runtime_record(&self, key: &str) -> Result<Option<serde_json::Value>> {
        validate_papertrade_runtime_key(key)?;
        let lock_ref = CredentialRef::new("11111111-1111-4111-8111-111111111111")?;
        let _lock = lock_auxiliary_reference(&self.dir, &lock_ref)?;
        Ok(read_papertrade_runtime_index(&self.dir)?
            .records
            .remove(key))
    }

    /// Persist an immutable broker-owned Papertrade plan/transaction identity.
    /// Repeated writes are allowed only when the bytes represent the same
    /// value, which makes recovery idempotent without permitting rebinding.
    pub fn put_papertrade_runtime_record(
        &self,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        validate_papertrade_runtime_key(key)?;
        let encoded = serde_json::to_vec(value).map_err(|_| {
            error(
                "CREDENTIAL_SAVE_FAILED",
                "Papertrade runtime record could not be encoded.",
            )
        })?;
        if encoded.len() > 1024 * 1024 {
            return Err(error(
                "CREDENTIAL_SAVE_FAILED",
                "Papertrade runtime record exceeds its size bound.",
            ));
        }
        let lock_ref = CredentialRef::new("11111111-1111-4111-8111-111111111111")?;
        let _lock = lock_auxiliary_reference(&self.dir, &lock_ref)?;
        let mut index = read_papertrade_runtime_index(&self.dir)?;
        if let Some(existing) = index.records.get(key) {
            if existing == value {
                return Ok(());
            }
            return Err(error(
                "PAPERTRADE_IDENTITY_CONFLICT",
                "A Papertrade durable identity is already bound to different content.",
            ));
        }
        index.records.insert(key.to_owned(), value.clone());
        write_papertrade_runtime_index(&self.dir, &index)
    }

    fn validate_papertrade_session_signer_record(
        &self,
        reference: &CredentialRef,
        operation_key: &str,
        owner_address: &str,
        expires_at: u64,
    ) -> Result<()> {
        self.with_material(reference, |material| match material {
            SecretMaterial::GeneratedPapertradeSessionSigner {
                owner_address: stored_owner,
                chain_id: 999,
                session_manager,
                operation_key: stored_operation,
                expires_at: stored_expiry,
                ..
            } if stored_operation == operation_key
                && stored_owner.eq_ignore_ascii_case(owner_address)
                && session_manager
                    .eq_ignore_ascii_case("0x8326eb8896e358717f10502ec73ce8d28e37b108")
                && *stored_expiry == expires_at =>
            {
                Ok(())
            }
            _ => Err(error(
                "CREDENTIAL_TYPE_MISMATCH",
                "The protected key is not bound to this Papertrade registration.",
            )),
        })
    }

    fn validate_solana_auxiliary_signer_record(
        &self,
        reference: &CredentialRef,
        operation_key: &str,
        owner_address: &str,
        purpose: &str,
    ) -> Result<()> {
        self.with_material(reference, |material| match material {
            SecretMaterial::GeneratedSolanaSigner {
                owner_address: stored_owner,
                purpose: stored_purpose,
                operation_key: stored,
                ..
            } if stored == operation_key && stored_owner == owner_address && stored_purpose == purpose => Ok(()),
            _ => Err(error(
                "CREDENTIAL_TYPE_MISMATCH",
                "The protected reference does not belong to this Solana owner, purpose and operation.",
            )),
        })
    }
    pub fn with_material<T>(
        &self,
        reference: &CredentialRef,
        callback: impl FnOnce(&SecretMaterial) -> Result<T>,
    ) -> Result<T> {
        gofin_authorization::validate_installation()?;
        let path = self.path_for(reference);
        gofin_authorization::validate_metadata(&path, true)?;
        reject_acl(&path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&path).map_err(|_| {
            error(
                "CREDENTIAL_UNAVAILABLE",
                "The protected credential could not be opened.",
            )
        })?;
        let metadata = file.metadata().map_err(|_| {
            error(
                "CREDENTIAL_UNAVAILABLE",
                "The protected credential could not be inspected.",
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != 0 || metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 {
                return Err(error(
                    "UNSAFE_CREDENTIAL",
                    "The protected credential has unsafe ownership, permissions, or links.",
                ));
            }
        }
        if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
            return Err(error(
                "INVALID_CREDENTIAL",
                "The protected credential is not a bounded regular file.",
            ));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                error(
                    "CREDENTIAL_UNAVAILABLE",
                    "The protected credential could not be read.",
                )
            })?;
        if bytes.len() > MAX_BYTES {
            return Err(error(
                "INVALID_CREDENTIAL",
                "The protected credential exceeded its size limit.",
            ));
        }
        let material: SecretMaterial = serde_json::from_slice(&bytes).map_err(|_| {
            error(
                "INVALID_CREDENTIAL",
                "The protected credential format is invalid.",
            )
        })?;
        material.validate_present()?;
        callback(&material)
    }
    pub fn remove(&self, reference: &CredentialRef) -> Result<()> {
        gofin_authorization::validate_installation()?;
        let path = self.path_for(reference);
        gofin_authorization::validate_metadata(&path, true)?;
        reject_acl(&path)?;
        fs::remove_file(path).map_err(|_| {
            error(
                "CREDENTIAL_REMOVE_FAILED",
                "The protected credential could not be removed.",
            )
        })?;
        sync_dir(&self.dir)
    }
    /// Connection-enrollment recovery inventory; never exposed to clients.
    /// Runtime auxiliary signers are retained independently of configuration:
    /// their durable operations can still need them after a connection changes.
    pub fn connection_references(&self) -> Result<Vec<CredentialRef>> {
        gofin_authorization::validate_installation()?;
        connection_references_at(&self.dir, 0)
    }
    fn path_for(&self, reference: &CredentialRef) -> PathBuf {
        self.dir.join(format!("{}.json", reference.as_str()))
    }
}

/// Recognize only names emitted by this store's own writers. The optional
/// reference identifies credential material; the size bound applies equally to
/// internal indexes, locks and interrupted staging files, which are never
/// candidates for connection cleanup.
fn inventory_entry(name: &str) -> Result<(Option<CredentialRef>, u64)> {
    let internal_bound = match name {
        "papertrade-sessions-v1.json" => Some(1024 * 1024),
        "papertrade-runtime-v1.json" => Some(16 * 1024 * 1024),
        _ => None,
    };
    if let Some(bound) = internal_bound {
        return Ok((None, bound));
    }
    if let Some(raw) = name.strip_suffix(".json") {
        return Ok((Some(CredentialRef::new(raw)?), MAX_BYTES as u64));
    }
    if let Some(raw) = name.strip_prefix('.').and_then(|s| s.strip_suffix(".lock")) {
        CredentialRef::new(raw)?;
        return Ok((None, 0));
    }
    if let Some(raw) = name.strip_prefix('.').and_then(|s| s.strip_suffix(".tmp"))
        && let Some((owner, nonce)) = raw.split_once('.')
    {
        CredentialRef::new(nonce)?;
        let bound = match owner {
            "papertrade-sessions" => 1024 * 1024,
            "papertrade-runtime" => 16 * 1024 * 1024,
            _ => {
                CredentialRef::new(owner)?;
                MAX_BYTES as u64
            }
        };
        return Ok((None, bound));
    }
    Err(error(
        "UNSAFE_CREDENTIAL_STORE",
        "The protected store contains an unexpected file.",
    ))
}

fn validate_inventory_metadata(metadata: &fs::Metadata, owner: u32, bound: u64) -> Result<()> {
    let mut safe =
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= bound;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        safe &=
            metadata.uid() == owner && metadata.mode() & 0o7777 == 0o600 && metadata.nlink() == 1;
    }
    let _ = owner;
    if !safe {
        return Err(error(
            "UNSAFE_CREDENTIAL_STORE",
            "The protected store contains unsafe file metadata.",
        ));
    }
    Ok(())
}

fn connection_references_at(dir: &Path, owner: u32) -> Result<Vec<CredentialRef>> {
    let unavailable = || {
        error(
            "CREDENTIAL_STORE_UNAVAILABLE",
            "The protected credential inventory is unavailable.",
        )
    };
    let mut references = Vec::new();
    for entry in fs::read_dir(dir).map_err(|_| unavailable())? {
        let entry = entry.map_err(|_| unavailable())?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            error(
                "UNSAFE_CREDENTIAL_STORE",
                "The protected store contains an unexpected file.",
            )
        })?;
        let (reference, bound) = inventory_entry(name)?;
        let path = entry.path();
        validate_inventory_metadata(
            &fs::symlink_metadata(&path).map_err(|_| unavailable())?,
            owner,
            bound,
        )?;
        reject_acl(&path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        let file = options.open(&path).map_err(|_| unavailable())?;
        validate_inventory_metadata(&file.metadata().map_err(|_| unavailable())?, owner, bound)?;
        let Some(reference) = reference else { continue };
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(bound + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() > bound as usize {
            return Err(error(
                "INVALID_CREDENTIAL",
                "The protected credential exceeded its size limit.",
            ));
        }
        let material: SecretMaterial = serde_json::from_slice(&bytes).map_err(|_| {
            error(
                "INVALID_CREDENTIAL",
                "The protected credential format is invalid.",
            )
        })?;
        material.validate_present()?;
        match material {
            SecretMaterial::GeneratedSolanaSigner { .. }
            | SecretMaterial::GeneratedPapertradeSessionSigner { .. } => {}
            _ => references.push(reference),
        }
    }
    references.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    Ok(references)
}

fn create_or_load_auxiliary_signer_at(
    dir: &Path,
    reference: &CredentialRef,
    operation_key: &str,
    owner_address: &str,
    purpose: &str,
) -> Result<()> {
    let _lock = lock_auxiliary_reference(dir, reference)?;
    cleanup_auxiliary_staging_files(dir, reference);
    if dir.join(format!("{}.json", reference.as_str())).exists() {
        return Ok(());
    }
    let material = generated_auxiliary_signer_material(operation_key, owner_address, purpose)?;
    write_material_at(dir, reference, &material)?;
    Ok(())
}

fn generated_auxiliary_signer_material(
    operation_key: &str,
    owner_address: &str,
    purpose: &str,
) -> Result<SecretMaterial> {
    let mut seed = Zeroizing::new([0u8; 32]);
    getrandom::fill(seed.as_mut()).map_err(|_| {
        error(
            "CREDENTIAL_GENERATION_FAILED",
            "A protected Solana position signer could not be generated.",
        )
    })?;
    use ed25519_dalek::SigningKey;
    let signing_key = SigningKey::from_bytes(&seed);
    let mut keypair = Zeroizing::new([0u8; 64]);
    keypair[..32].copy_from_slice(&seed[..]);
    keypair[32..].copy_from_slice(&signing_key.verifying_key().to_bytes());
    let key_json = Zeroizing::new(serde_json::to_string(keypair.as_ref()).map_err(|_| {
        error(
            "CREDENTIAL_GENERATION_FAILED",
            "A protected Solana position signer could not be encoded.",
        )
    })?);
    let material = SecretMaterial::GeneratedSolanaSigner {
        owner_address: owner_address.to_owned(),
        purpose: purpose.to_owned(),
        operation_key: operation_key.to_owned(),
        private_key: key_json.to_string(),
    };
    material.validate_present()?;
    Ok(material)
}

fn create_or_load_papertrade_session_signer_at(
    dir: &Path,
    reference: &CredentialRef,
    operation_key: &str,
    owner_address: &str,
    expires_at: u64,
) -> Result<()> {
    let _lock = lock_auxiliary_reference(dir, reference)?;
    cleanup_auxiliary_staging_files(dir, reference);
    if dir.join(format!("{}.json", reference.as_str())).exists() {
        return Ok(());
    }
    let material =
        generated_papertrade_session_signer_material(operation_key, owner_address, expires_at)?;
    write_material_at(dir, reference, &material)?;
    Ok(())
}

fn generated_papertrade_session_signer_material(
    operation_key: &str,
    owner_address: &str,
    expires_at: u64,
) -> Result<SecretMaterial> {
    let mut secret = Zeroizing::new([0u8; 32]);
    loop {
        getrandom::fill(secret.as_mut()).map_err(|_| {
            error(
                "CREDENTIAL_GENERATION_FAILED",
                "A protected Papertrade session signer could not be generated.",
            )
        })?;
        let encoded = Zeroizing::new(hex::encode(secret.as_ref()));
        if valid_secp256k1_private_key(&encoded) {
            break;
        }
    }
    let material = SecretMaterial::GeneratedPapertradeSessionSigner {
        owner_address: owner_address.to_ascii_lowercase(),
        chain_id: 999,
        session_manager: "0x8326eb8896e358717f10502ec73ce8d28e37b108".into(),
        operation_key: operation_key.into(),
        expires_at,
        private_key: hex::encode(secret.as_ref()),
    };
    material.validate_present()?;
    Ok(material)
}

fn write_material(dir: &Path, material: &SecretMaterial) -> Result<CredentialRef> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(material)
            .map_err(|_| error("INVALID_CREDENTIAL", "The credential could not be encoded."))?,
    );
    if bytes.len() > MAX_BYTES {
        return Err(error(
            "INVALID_CREDENTIAL",
            "The credential exceeded its size limit.",
        ));
    }
    let reference = CredentialRef::new(&uuid::Uuid::new_v4().hyphenated().to_string())?;
    let path = dir.join(format!("{}.json", reference.as_str()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&path).map_err(|_| {
        error(
            "CREDENTIAL_SAVE_FAILED",
            "The protected credential could not be created.",
        )
    })?;
    let save = (|| {
        file.write_all(&bytes).map_err(|_| {
            error(
                "CREDENTIAL_SAVE_FAILED",
                "The protected credential could not be written.",
            )
        })?;
        file.sync_all().map_err(|_| {
            error(
                "CREDENTIAL_SAVE_FAILED",
                "The protected credential could not be synchronized.",
            )
        })?;
        sync_dir(dir)
    })();
    if let Err(error) = save {
        drop(file);
        let _ = fs::remove_file(&path);
        let _ = sync_dir(dir);
        return Err(error);
    }
    Ok(reference)
}

/// Atomically create an immutable credential at an internally derived handle.
/// A hard link publishes the fully fsynced temporary file without replacing a
/// concurrent winner. Returns false when that winner already exists.
fn write_material_at(
    dir: &Path,
    reference: &CredentialRef,
    material: &SecretMaterial,
) -> Result<bool> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(material)
            .map_err(|_| error("INVALID_CREDENTIAL", "The credential could not be encoded."))?,
    );
    if bytes.len() > MAX_BYTES {
        return Err(error(
            "INVALID_CREDENTIAL",
            "The credential exceeded its size limit.",
        ));
    }
    let temporary = dir.join(format!(
        ".{}.{}.tmp",
        reference.as_str(),
        uuid::Uuid::new_v4()
    ));
    let target = dir.join(format!("{}.json", reference.as_str()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&temporary).map_err(|_| {
        error(
            "CREDENTIAL_SAVE_FAILED",
            "The protected credential could not be staged.",
        )
    })?;
    let save = (|| {
        file.write_all(&bytes).map_err(|_| {
            error(
                "CREDENTIAL_SAVE_FAILED",
                "The protected credential could not be written.",
            )
        })?;
        file.sync_all().map_err(|_| {
            error(
                "CREDENTIAL_SAVE_FAILED",
                "The protected credential could not be synchronized.",
            )
        })?;
        drop(file);
        match fs::hard_link(&temporary, &target) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(_) => Err(error(
                "CREDENTIAL_SAVE_FAILED",
                "The protected credential could not be published.",
            )),
        }
    })();
    let _ = fs::remove_file(&temporary);
    let published = save?;
    sync_dir(dir)?;
    Ok(published)
}

/// Remove plaintext staging files left by a crash before no-clobber publish.
/// The exact reference prefix limits cleanup to this logical auxiliary signer.
fn cleanup_auxiliary_staging_files(dir: &Path, reference: &CredentialRef) {
    let prefix = format!(".{}.", reference.as_str());
    let Ok(entries) = fs::read_dir(dir) else {
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

/// Serialize create/recover/cleanup for one deterministic auxiliary identity
/// across threads and broker processes. Staging files are removed only while
/// holding this lock, so a concurrent writer cannot lose its active plaintext
/// staging file before its no-clobber publish.
fn lock_auxiliary_reference(dir: &Path, reference: &CredentialRef) -> Result<File> {
    let path = dir.join(format!(".{}.lock", reference.as_str()));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|_| {
        error(
            "CREDENTIAL_SAVE_FAILED",
            "The protected auxiliary signer lock could not be opened.",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // flock blocks until another broker process completes the complete
        // create/load/cleanup operation for this same opaque identity.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(error(
                "CREDENTIAL_SAVE_FAILED",
                "The protected auxiliary signer lock could not be acquired.",
            ));
        }
    }
    Ok(file)
}

fn read_session_index(dir: &Path) -> Result<PapertradeSessionIndex> {
    const FILE: &str = "papertrade-sessions-v1.json";
    const MAX_INDEX_BYTES: u64 = 1024 * 1024;
    let path = dir.join(FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PapertradeSessionIndex {
                version: 1,
                records: Vec::new(),
            });
        }
        Err(_) => {
            return Err(error(
                "CREDENTIAL_UNAVAILABLE",
                "Papertrade session registry is unavailable.",
            ));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > MAX_INDEX_BYTES
        {
            return Err(error(
                "UNSAFE_CREDENTIAL",
                "Papertrade session registry permissions or size are unsafe.",
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(&path).map_err(|_| {
        error(
            "CREDENTIAL_UNAVAILABLE",
            "Papertrade session registry could not be opened.",
        )
    })?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            error(
                "CREDENTIAL_UNAVAILABLE",
                "Papertrade session registry could not be read.",
            )
        })?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return Err(error(
            "UNSAFE_CREDENTIAL",
            "Papertrade session registry exceeds its size bound.",
        ));
    }
    let index: PapertradeSessionIndex = serde_json::from_slice(&bytes).map_err(|_| {
        error(
            "CREDENTIAL_UNAVAILABLE",
            "Papertrade session registry is malformed.",
        )
    })?;
    if index.version != 1
        || index.records.iter().any(|record| {
            !valid_evm_address(&record.owner_address)
                || !matches!(uuid::Uuid::parse_str(&record.registration_id), Ok(id) if id.hyphenated().to_string() == record.registration_id)
                || record.expires_at == 0
                || record.registered_at == 0
        })
    {
        return Err(error(
            "CREDENTIAL_UNAVAILABLE",
            "Papertrade session registry version or identity is invalid.",
        ));
    }
    Ok(index)
}

fn validate_papertrade_runtime_key(key: &str) -> Result<()> {
    if key.is_empty()
        // Funding-leg identities hex-encode the engine's full stable
        // operation id. The 512-byte bound accommodates the observed
        // plan-execution UUID path; callers still preflight generated keys.
        || key.len() > 512
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:".contains(&byte))
    {
        return Err(error(
            "INVALID_CREDENTIAL_REFERENCE",
            "Papertrade runtime record key is invalid.",
        ));
    }
    Ok(())
}

fn read_papertrade_runtime_index(dir: &Path) -> Result<PapertradeRuntimeIndex> {
    const FILE: &str = "papertrade-runtime-v1.json";
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let path = dir.join(FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PapertradeRuntimeIndex {
                version: 1,
                records: std::collections::BTreeMap::new(),
            });
        }
        Err(_) => {
            return Err(error(
                "CREDENTIAL_UNAVAILABLE",
                "Papertrade runtime journal is unavailable.",
            ));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.len() > MAX_BYTES
        {
            return Err(error(
                "UNSAFE_CREDENTIAL",
                "Papertrade runtime journal permissions or size are unsafe.",
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|_| {
        error(
            "CREDENTIAL_UNAVAILABLE",
            "Papertrade runtime journal could not be opened.",
        )
    })?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            error(
                "CREDENTIAL_UNAVAILABLE",
                "Papertrade runtime journal could not be read.",
            )
        })?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(error(
            "UNSAFE_CREDENTIAL",
            "Papertrade runtime journal exceeds its size bound.",
        ));
    }
    let index: PapertradeRuntimeIndex = serde_json::from_slice(&bytes).map_err(|_| {
        error(
            "CREDENTIAL_UNAVAILABLE",
            "Papertrade runtime journal is malformed.",
        )
    })?;
    if index.version != 1
        || index.records.len() > 4096
        || index
            .records
            .keys()
            .any(|key| validate_papertrade_runtime_key(key).is_err())
    {
        return Err(error(
            "CREDENTIAL_UNAVAILABLE",
            "Papertrade runtime journal version or identity is invalid.",
        ));
    }
    Ok(index)
}

fn write_papertrade_runtime_index(dir: &Path, index: &PapertradeRuntimeIndex) -> Result<()> {
    const FILE: &str = "papertrade-runtime-v1.json";
    if index.version != 1 || index.records.len() > 4096 {
        return Err(error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade runtime journal version or record count is invalid.",
        ));
    }
    let bytes = Zeroizing::new(serde_json::to_vec(index).map_err(|_| {
        error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade runtime journal could not be encoded.",
        )
    })?);
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade runtime journal exceeds its size bound.",
        ));
    }
    let target = dir.join(FILE);
    if let Ok(metadata) = fs::symlink_metadata(&target) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != 0
                || metadata.mode() & 0o777 != 0o600
                || metadata.nlink() != 1
            {
                return Err(error(
                    "UNSAFE_CREDENTIAL",
                    "Papertrade runtime journal cannot be safely replaced.",
                ));
            }
        }
    }
    let temporary = dir.join(format!(".papertrade-runtime.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&temporary).map_err(|_| {
        error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade runtime journal could not be staged.",
        )
    })?;
    if file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .is_err()
    {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade runtime journal could not be synchronized.",
        ));
    }
    drop(file);
    fs::rename(&temporary, &target).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade runtime journal could not be published.",
        )
    })?;
    sync_dir(dir)
}

fn write_session_index(dir: &Path, index: &PapertradeSessionIndex) -> Result<()> {
    const FILE: &str = "papertrade-sessions-v1.json";
    if index.version != 1 {
        return Err(error(
            "CREDENTIAL_UNAVAILABLE",
            "Unsupported Papertrade session registry version.",
        ));
    }
    let bytes = Zeroizing::new(serde_json::to_vec(index).map_err(|_| {
        error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade session registry could not be encoded.",
        )
    })?);
    if bytes.len() > 1024 * 1024 {
        return Err(error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade session registry exceeds its size bound.",
        ));
    }
    let target = dir.join(FILE);
    if let Ok(metadata) = fs::symlink_metadata(&target) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != 0
                || metadata.mode() & 0o777 != 0o600
                || metadata.nlink() != 1
            {
                return Err(error(
                    "UNSAFE_CREDENTIAL",
                    "Papertrade session registry cannot be safely replaced.",
                ));
            }
        }
    }
    let temporary = dir.join(format!(".papertrade-sessions.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&temporary).map_err(|_| {
        error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade session registry could not be staged.",
        )
    })?;
    if file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .is_err()
    {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade session registry could not be synchronized.",
        ));
    }
    drop(file);
    fs::rename(&temporary, &target).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        error(
            "CREDENTIAL_SAVE_FAILED",
            "Papertrade session registry could not be published.",
        )
    })?;
    sync_dir(dir)
}

fn papertrade_session_registrations(
    store: &TrustedCredentialStore,
    index: PapertradeSessionIndex,
    owner_address: &str,
) -> Result<Vec<PapertradeSessionRegistration>> {
    index
        .records
        .into_iter()
        .filter(|record| record.owner_address.eq_ignore_ascii_case(owner_address))
        .map(|record| {
            store.with_material(&record.credential_ref, |material| match material {
                SecretMaterial::GeneratedPapertradeSessionSigner {
                    owner_address,
                    operation_key,
                    expires_at,
                    chain_id: 999,
                    session_manager,
                    private_key,
                } if owner_address.eq_ignore_ascii_case(&record.owner_address)
                    && *operation_key == record.registration_id
                    && *expires_at == record.expires_at
                    && session_manager
                        .eq_ignore_ascii_case("0x8326eb8896e358717f10502ec73ce8d28e37b108") =>
                {
                    if !valid_secp256k1_private_key(private_key) {
                        return Err(error(
                            "CREDENTIAL_TYPE_MISMATCH",
                            "Stored Papertrade session key is invalid.",
                        ));
                    }
                    Ok(())
                }
                _ => Err(error(
                    "CREDENTIAL_TYPE_MISMATCH",
                    "Stored Papertrade session credential does not match its registered owner and operation.",
                )),
            })?;
            Ok(PapertradeSessionRegistration {
                owner_address: record.owner_address,
                registration_id: record.registration_id,
                expires_at: record.expires_at,
                credential_ref: record.credential_ref,
                registered_at: record.registered_at,
                revoked_at: record.revoked_at,
            })
        })
        .collect()
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|f| f.sync_all()).map_err(|_| {
        error(
            "CREDENTIAL_STORE_SYNC_FAILED",
            "The protected store could not be synchronized.",
        )
    })
}
fn reject_acl(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/bin/ls")
            .arg("-lde")
            .arg(path)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .output()
            .map_err(|_| {
                error(
                    "UNSAFE_CREDENTIAL_STORE",
                    "Credential ACL protections could not be verified.",
                )
            })?;
        if !output.status.success()
            || String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.trim_start()
                    .split_once(':')
                    .is_some_and(|(number, _)| {
                        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
                    })
            })
        {
            return Err(error(
                "UNSAFE_CREDENTIAL_STORE",
                "Extended credential ACLs are not permitted.",
            ));
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    fn inventory_owner() -> u32 {
        // Tests only use temporary synthetic stores owned by this process.
        unsafe { libc::geteuid() }
    }

    #[cfg(unix)]
    fn inventory_fixture(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn connection_inventory_preserves_mixed_runtime_files_and_auxiliary_signers() {
        let dir = tempfile::tempdir().unwrap();
        let connection = write_material(
            dir.path(),
            &SecretMaterial::BinanceHmac {
                api_key: "synthetic-key".into(),
                secret: "synthetic-secret".into(),
            },
        )
        .unwrap();
        let operation = "550e8400-e29b-41d4-a716-446655440000";
        let solana =
            auxiliary_signer_reference(operation, "SyntheticOwner", "meteora_position_nft_mint")
                .unwrap();
        create_or_load_auxiliary_signer_at(
            dir.path(),
            &solana,
            operation,
            "SyntheticOwner",
            "meteora_position_nft_mint",
        )
        .unwrap();
        let owner = "0x1234567890123456789012345678901234567890";
        let papertrade =
            papertrade_session_signer_reference(operation, owner, 1_800_000_000).unwrap();
        create_or_load_papertrade_session_signer_at(
            dir.path(),
            &papertrade,
            operation,
            owner,
            1_800_000_000,
        )
        .unwrap();
        // Auxiliary retention follows validated material, not UUID version or
        // deterministic-name assumptions.
        write_material(
            dir.path(),
            &generated_auxiliary_signer_material(
                operation,
                "SyntheticOwner",
                "meteora_temporary_wsol",
            )
            .unwrap(),
        )
        .unwrap();
        write_session_index(
            dir.path(),
            &PapertradeSessionIndex {
                version: 1,
                records: Vec::new(),
            },
        )
        .unwrap();
        write_papertrade_runtime_index(
            dir.path(),
            &PapertradeRuntimeIndex {
                version: 1,
                records: Default::default(),
            },
        )
        .unwrap();
        let registry_lock = CredentialRef::new("11111111-1111-4111-8111-111111111111").unwrap();
        drop(lock_auxiliary_reference(dir.path(), &registry_lock).unwrap());
        for name in [
            format!(".{}.{}.tmp", solana.as_str(), uuid::Uuid::new_v4()),
            format!(".papertrade-sessions.{}.tmp", uuid::Uuid::new_v4()),
            format!(".papertrade-runtime.{}.tmp", uuid::Uuid::new_v4()),
        ] {
            inventory_fixture(&dir.path().join(name), b"interrupted synthetic staging");
        }
        let snapshot = || -> std::collections::BTreeMap<_, _> {
            fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect()
        };
        let before = snapshot();
        let candidates = connection_references_at(dir.path(), inventory_owner()).unwrap();
        assert_eq!(candidates, vec![connection.clone()]);
        assert_eq!(
            snapshot(),
            before,
            "inventory must never modify runtime recovery data"
        );
        // Model enrollment's cleanup of an inactive connection. No runtime
        // signer or bookkeeping file may appear in its deletion candidates.
        for candidate in candidates {
            fs::remove_file(dir.path().join(format!("{}.json", candidate.as_str()))).unwrap();
        }
        let mut retained = before;
        retained.remove(&std::ffi::OsString::from(format!(
            "{}.json",
            connection.as_str()
        )));
        assert_eq!(snapshot(), retained);
        create_or_load_auxiliary_signer_at(
            dir.path(),
            &solana,
            operation,
            "SyntheticOwner",
            "meteora_position_nft_mint",
        )
        .unwrap();
        create_or_load_papertrade_session_signer_at(
            dir.path(),
            &papertrade,
            operation,
            owner,
            1_800_000_000,
        )
        .unwrap();
        for reference in [solana, papertrade] {
            let name = std::ffi::OsString::from(format!("{}.json", reference.as_str()));
            assert_eq!(
                fs::read(dir.path().join(&name)).unwrap(),
                retained[&name],
                "operation recovery must reuse the original signer"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn connection_inventory_rejects_unknown_and_noncanonical_names() {
        for name in [
            "unexpected",
            ".DS_Store",
            "unknown.json",
            "papertrade-sessions-v2.json",
            ".anything.lock",
            ".anything.tmp",
            ".papertrade-runtime.not-a-uuid.tmp",
            ".550e8400-e29b-41d4-a716-446655440000.crashed.tmp",
            ".550E8400-E29B-41D4-A716-446655440000.lock",
        ] {
            let dir = tempfile::tempdir().unwrap();
            inventory_fixture(&dir.path().join(name), b"");
            assert!(
                connection_references_at(dir.path(), inventory_owner()).is_err(),
                "accepted {name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn connection_inventory_rejects_unsafe_internal_file_metadata() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for name in [
            ".550e8400-e29b-41d4-a716-446655440000.lock",
            "papertrade-runtime-v1.json",
            ".papertrade-sessions.550e8400-e29b-41d4-a716-446655440000.tmp",
        ] {
            for kind in [
                "permissions",
                "owner",
                "symlink",
                "hardlink",
                "directory",
                "oversized",
            ] {
                let dir = tempfile::tempdir().unwrap();
                let outside = tempfile::tempdir().unwrap();
                let path = dir.path().join(name);
                match kind {
                    "directory" => fs::create_dir(&path).unwrap(),
                    "symlink" => symlink(outside.path().join("absent"), &path).unwrap(),
                    "hardlink" => {
                        let target = outside.path().join("synthetic");
                        inventory_fixture(&target, b"");
                        fs::hard_link(target, &path).unwrap();
                    }
                    _ => {
                        inventory_fixture(&path, b"");
                        if kind == "permissions" {
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
                        } else if kind == "oversized" {
                            let (_, bound) = inventory_entry(name).unwrap();
                            OpenOptions::new()
                                .write(true)
                                .open(&path)
                                .unwrap()
                                .set_len(bound + 1)
                                .unwrap();
                        }
                    }
                }
                let owner = inventory_owner() + u32::from(kind == "owner");
                assert!(
                    connection_references_at(dir.path(), owner).is_err(),
                    "accepted {kind} for {name}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn connection_inventory_rejects_invalid_or_unbounded_credential_material() {
        for bytes in [
            b"not-json".to_vec(),
            br#"{"type":"binance_hmac","api_key":"","secret":""}"#.to_vec(),
            br#"{"type":"generated_solana_signer","owner_address":"owner","purpose":"unknown","operation_key":"550e8400-e29b-41d4-a716-446655440000","private_key":"synthetic"}"#.to_vec(),
            vec![b' '; MAX_BYTES + 1],
        ] {
            let dir = tempfile::tempdir().unwrap();
            inventory_fixture(&dir.path().join("550e8400-e29b-41d4-a716-446655440000.json"), &bytes);
            assert!(connection_references_at(dir.path(), inventory_owner()).is_err());
        }
    }

    #[test]
    fn papertrade_runtime_keys_allow_bounded_generated_funding_leg_identity() {
        let operation_id = "plan-execution:4ef633d3-5e2e-4cf2-9ea3-1ef193f976fb:ceff6b4d-f0cb-4cec-825c-9cc2b40d47a8:deposit:e7667423-8322-41cf-95b6-df758fecb963";
        let leg_id = format!("papertrade-funding:{operation_id}:0");
        let key = format!(
            "papertrade-funding-leg:{}:dispatch_attempted",
            hex::encode(leg_id.as_bytes())
        );
        assert!(key.len() > 256);
        assert!(key.len() <= 512);
        assert!(validate_papertrade_runtime_key(&key).is_ok());
        assert!(validate_papertrade_runtime_key(&format!("{}x", "a".repeat(512))).is_err());
    }

    #[test]
    fn auxiliary_signer_identity_is_stable_and_scoped_to_owner_and_purpose() {
        let operation = "550e8400-e29b-41d4-a716-446655440000";
        let position = auxiliary_signer_reference(
            operation,
            "Owner111111111111111111111111111111111",
            "meteora_position_nft_mint",
        )
        .unwrap();
        assert_eq!(
            position,
            auxiliary_signer_reference(
                operation,
                "Owner111111111111111111111111111111111",
                "meteora_position_nft_mint",
            )
            .unwrap()
        );
        let wsol = auxiliary_signer_reference(
            operation,
            "Owner111111111111111111111111111111111",
            "meteora_temporary_wsol",
        )
        .unwrap();
        let other_owner = auxiliary_signer_reference(
            operation,
            "Owner222222222222222222222222222222222",
            "meteora_position_nft_mint",
        )
        .unwrap();
        assert_ne!(position, wsol);
        assert_ne!(position, other_owner);
        assert!(auxiliary_signer_reference(operation, "owner", "other").is_err());
        let papertrade = auxiliary_signer_reference(
            operation,
            "Owner111111111111111111111111111111111",
            "papertrade_cctp_message_sent_event",
        )
        .unwrap();
        assert_ne!(position, papertrade);
        assert!(
            auxiliary_signer_reference(
                operation,
                "Owner111111111111111111111111111111111",
                "papertrade_arbitrary_signer",
            )
            .is_err()
        );
    }

    #[test]
    fn auxiliary_signer_creation_cleans_only_its_stale_plaintext_staging_files() {
        let dir = tempfile::tempdir().unwrap();
        let reference = CredentialRef::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let stale = dir.path().join(format!(
            ".{}.550e8400-e29b-41d4-a716-446655440001.tmp",
            reference.as_str()
        ));
        let unrelated = dir
            .path()
            .join(".550e8400-e29b-41d4-a716-446655440002.550e8400-e29b-41d4-a716-446655440003.tmp");
        fs::write(&stale, b"synthetic secret staging data").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();
        cleanup_auxiliary_staging_files(dir.path(), &reference);
        assert!(!stale.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn auxiliary_signer_cleanup_waits_for_active_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        let reference = CredentialRef::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let staging = dir
            .path()
            .join(format!(".{}.active.tmp", reference.as_str()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer_dir = dir.path().to_path_buf();
        let writer_reference = reference.clone();
        let writer = std::thread::spawn(move || {
            let _lock = lock_auxiliary_reference(&writer_dir, &writer_reference).unwrap();
            fs::write(
                writer_dir.join(format!(".{}.active.tmp", writer_reference.as_str())),
                b"active",
            )
            .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();

        let cleanup_dir = dir.path().to_path_buf();
        let cleanup_reference = reference.clone();
        let (cleanup_tx, cleanup_rx) = std::sync::mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            let _lock = lock_auxiliary_reference(&cleanup_dir, &cleanup_reference).unwrap();
            cleanup_auxiliary_staging_files(&cleanup_dir, &cleanup_reference);
            cleanup_tx.send(()).unwrap();
        });
        assert!(
            cleanup_rx
                .recv_timeout(std::time::Duration::from_millis(25))
                .is_err()
        );
        assert!(staging.exists());
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        cleanup_rx.recv().unwrap();
        cleanup.join().unwrap();
        assert!(!staging.exists());
    }

    #[test]
    fn auxiliary_signer_key_material_round_trips_and_concurrent_creators_converge() {
        let dir = tempfile::tempdir().unwrap();
        let operation = "550e8400-e29b-41d4-a716-446655440000";
        let owner = "Owner111111111111111111111111111111111";
        let purpose = "meteora_position_nft_mint";
        let reference = auxiliary_signer_reference(operation, owner, purpose).unwrap();
        // Model a crash before no-clobber publication; the next serialized
        // create removes only this identity's plaintext staging material.
        let stale = dir
            .path()
            .join(format!(".{}.crashed.tmp", reference.as_str()));
        fs::write(&stale, b"synthetic incomplete key material").unwrap();
        // Simulate process death after the generated secret was durably
        // written and fsynced, but before the no-clobber hard-link publish.
        let staged_material =
            generated_auxiliary_signer_material(operation, owner, purpose).unwrap();
        let staged = dir
            .path()
            .join(format!(".{}.after-fsync.tmp", reference.as_str()));
        let staged_bytes = serde_json::to_vec(&staged_material).unwrap();
        let mut staged_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .unwrap();
        use std::io::Write;
        staged_file.write_all(&staged_bytes).unwrap();
        staged_file.sync_all().unwrap();
        drop(staged_file);

        let mut threads = Vec::new();
        for _ in 0..8 {
            let path = dir.path().to_path_buf();
            let reference = reference.clone();
            threads.push(std::thread::spawn(move || {
                create_or_load_auxiliary_signer_at(&path, &reference, operation, owner, purpose)
                    .unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(!stale.exists());
        assert!(!staged.exists());

        let path = dir.path().join(format!("{}.json", reference.as_str()));
        let first: SecretMaterial = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        first.validate_present().unwrap();
        let first_private_key = match &first {
            SecretMaterial::GeneratedSolanaSigner {
                owner_address,
                purpose,
                operation_key,
                private_key,
            } => {
                assert_eq!(owner_address, owner);
                assert_eq!(purpose, "meteora_position_nft_mint");
                assert_eq!(operation_key, operation);
                private_key.clone()
            }
            _ => panic!("persisted auxiliary signer has the wrong credential type"),
        };

        create_or_load_auxiliary_signer_at(dir.path(), &reference, operation, owner, purpose)
            .unwrap();
        let recovered: SecretMaterial = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let recovered_private_key = match &recovered {
            SecretMaterial::GeneratedSolanaSigner { private_key, .. } => private_key.clone(),
            _ => panic!("recovered auxiliary signer has the wrong credential type"),
        };
        assert_eq!(first_private_key, recovered_private_key);
    }

    #[test]
    fn papertrade_session_key_is_generated_and_bound_to_exact_registration() {
        let dir = tempfile::tempdir().unwrap();
        let operation = "550e8400-e29b-41d4-a716-446655440000";
        let owner = "0x1234567890123456789012345678901234567890";
        let expiry = 1_800_000_000;
        let reference = papertrade_session_signer_reference(operation, owner, expiry).unwrap();
        let other_owner = papertrade_session_signer_reference(
            operation,
            "0x1234567890123456789012345678901234567891",
            expiry,
        )
        .unwrap();
        let other_expiry =
            papertrade_session_signer_reference(operation, owner, expiry + 1).unwrap();
        assert_ne!(reference, other_owner);
        assert_ne!(reference, other_expiry);

        create_or_load_papertrade_session_signer_at(
            dir.path(),
            &reference,
            operation,
            owner,
            expiry,
        )
        .unwrap();
        let path = dir.path().join(format!("{}.json", reference.as_str()));
        let first: SecretMaterial = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        first.validate_present().unwrap();
        let first_key = match &first {
            SecretMaterial::GeneratedPapertradeSessionSigner {
                owner_address,
                chain_id,
                session_manager,
                operation_key,
                expires_at,
                private_key,
            } => {
                assert_eq!(owner_address, &owner.to_ascii_lowercase());
                assert_eq!(*chain_id, 999);
                assert_eq!(
                    session_manager,
                    "0x8326eb8896e358717f10502ec73ce8d28e37b108"
                );
                assert_eq!(operation_key, operation);
                assert_eq!(*expires_at, expiry);
                private_key.clone()
            }
            _ => panic!("persisted session key has the wrong credential type"),
        };
        create_or_load_papertrade_session_signer_at(
            dir.path(),
            &reference,
            operation,
            owner,
            expiry,
        )
        .unwrap();
        let recovered: SecretMaterial = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let recovered_key = match &recovered {
            SecretMaterial::GeneratedPapertradeSessionSigner { private_key, .. } => {
                private_key.clone()
            }
            _ => panic!("recovered session key has the wrong credential type"),
        };
        assert_eq!(first_key, recovered_key);
        assert!(papertrade_session_signer_reference("not-a-uuid", owner, expiry).is_err());
        assert!(generated_papertrade_session_signer_material(operation, "owner", expiry).is_err());
    }

    #[test]
    fn kraken_spot_and_futures_credentials_are_typed_and_validate_base64_secrets() {
        for material in [
            SecretMaterial::KrakenApi {
                api_key: "SYNTHETIC-KRAKEN-KEY".into(),
                secret: "c3ludGhldGljLW9ubHk=".into(),
            },
            SecretMaterial::KrakenFuturesApi {
                api_key: "SYNTHETIC-KRAKEN-FUTURES-KEY".into(),
                secret: "c3ludGhldGljLW9ubHk=".into(),
            },
        ] {
            material.validate_present().unwrap();
            assert_eq!(format!("{material:?}"), "SecretMaterial(<redacted>)");
            let encoded = serde_json::to_string(&material).unwrap();
            let decoded: SecretMaterial = serde_json::from_str(&encoded).unwrap();
            decoded.validate_present().unwrap();
            assert_eq!(format!("{decoded:?}"), "SecretMaterial(<redacted>)");
        }
        for material in [
            SecretMaterial::KrakenApi {
                api_key: "SYNTHETIC-KRAKEN-KEY".into(),
                secret: "not-base64".into(),
            },
            SecretMaterial::KrakenFuturesApi {
                api_key: "SYNTHETIC-KRAKEN-FUTURES-KEY".into(),
                secret: "".into(),
            },
        ] {
            let error = material.validate_present().unwrap_err();
            assert_eq!(error.code, "INVALID_CREDENTIAL");
            assert!(!error.to_string().contains("SYNTHETIC-KRAKEN"));
        }
    }

    #[test]
    fn solana_and_jupiter_material_round_trip_without_debug_secret_exposure() {
        let mut signer = SecretMaterial::SolanaPrivateKey {
            private_key: "SYNTHETIC-SOLANA-KEY-FIXTURE-NOT-VALID-FOR-BROADCAST".into(),
        };
        signer.validate_present().unwrap();
        assert_eq!(format!("{signer:?}"), "SecretMaterial(<redacted>)");
        let signer_json = serde_json::to_string(&signer).unwrap();
        assert!(signer_json.contains("solana_private_key"));
        let signer_copy: SecretMaterial = serde_json::from_str(&signer_json).unwrap();
        signer_copy.validate_present().unwrap();
        signer.zeroize();
        assert!(signer.validate_present().is_err());

        let api_key = SecretMaterial::JupiterApiKey {
            api_key: "SYNTHETIC-JUPITER-KEY-FIXTURE".into(),
        };
        api_key.validate_present().unwrap();
        assert_eq!(format!("{api_key:?}"), "SecretMaterial(<redacted>)");
        let api_json = serde_json::to_string(&api_key).unwrap();
        assert!(api_json.contains("jupiter_api_key"));
        let api_copy: SecretMaterial = serde_json::from_str(&api_json).unwrap();
        api_copy.validate_present().unwrap();
        assert!(!format!("{api_copy:?}").contains("SYNTHETIC"));
    }

    #[test]
    fn jupiter_key_header_characters_are_bounded_and_secrets_never_echo() {
        for key in ["", "has space", "bad\nheader", "ключ"] {
            let error = SecretMaterial::JupiterApiKey {
                api_key: key.into(),
            }
            .validate_present()
            .unwrap_err();
            assert_eq!(error.code, "INVALID_CREDENTIAL");
            assert!(!error.to_string().contains("SYNTHETIC"));
        }
        assert!(
            SecretMaterial::JupiterApiKey {
                api_key: "A".repeat(1025)
            }
            .validate_present()
            .is_err()
        );
        assert!(
            SecretMaterial::JupiterApiKey {
                api_key: "SYNTHETIC-KEY-123".into()
            }
            .validate_present()
            .is_ok()
        );
    }

    #[test]
    fn relay_key_is_bounded_and_redacted() {
        let material = SecretMaterial::RelayApiKey {
            api_key: "SYNTHETIC-RELAY-KEY".into(),
        };
        material.validate_present().unwrap();
        assert_eq!(format!("{material:?}"), "SecretMaterial(<redacted>)");
        let encoded = serde_json::to_string(&material).unwrap();
        assert!(encoded.contains("relay_api_key"));
        assert!(
            !format!(
                "{:?}",
                serde_json::from_str::<SecretMaterial>(&encoded).unwrap()
            )
            .contains("SYNTHETIC")
        );
        for bad in ["", "has space", "bad\nheader", "ключ"] {
            assert!(
                SecretMaterial::RelayApiKey {
                    api_key: bad.into()
                }
                .validate_present()
                .is_err()
            );
        }
        assert!(
            SecretMaterial::RelayApiKey {
                api_key: "A".repeat(1025)
            }
            .validate_present()
            .is_err()
        );
    }

    #[test]
    fn coingecko_keys_are_typed_validated_redacted_and_zeroized() {
        for plan in ["demo", "pro"] {
            let mut material = SecretMaterial::CoinGeckoApiKey {
                api_key: "SYNTHETIC-COINGECKO-API-KEY-NEVER-LIVE".into(),
                plan: plan.into(),
            };
            material.validate_present().unwrap();
            assert_eq!(format!("{material:?}"), "SecretMaterial(<redacted>)");
            let encoded = serde_json::to_string(&material).unwrap();
            let decoded: SecretMaterial = serde_json::from_str(&encoded).unwrap();
            assert!(matches!(decoded, SecretMaterial::CoinGeckoApiKey { .. }));
            decoded.validate_present().unwrap();
            material.zeroize();
            let SecretMaterial::CoinGeckoApiKey { api_key, plan } = &material else {
                panic!("typed credential")
            };
            assert!(api_key.is_empty() && plan.is_empty());
            assert!(material.validate_present().is_err());
        }
    }

    #[test]
    fn malformed_coingecko_keys_and_plans_fail_without_echoing_secrets() {
        for (key, plan) in [
            ("", "demo"),
            ("SYNTHETIC KEY", "demo"),
            ("SYNTHETIC\r\nInjected: value", "pro"),
            ("SYNTHETIC\0KEY", "pro"),
            ("SYNTHETIC-ü", "demo"),
            ("SYNTHETIC:KEY", "demo"),
            ("SYNTHETIC-KEY", "unexpected"),
            ("SYNTHETIC-KEY", "Pro"),
        ] {
            let material = SecretMaterial::CoinGeckoApiKey {
                api_key: key.into(),
                plan: plan.into(),
            };
            let error = material.validate_present().unwrap_err();
            assert_eq!(error.code, "INVALID_CREDENTIAL");
            assert!(!error.to_string().contains("SYNTHETIC"));
        }
        let material = SecretMaterial::CoinGeckoApiKey {
            api_key: "S".repeat(257),
            plan: "demo".into(),
        };
        assert!(material.validate_present().is_err());
    }

    #[test]
    fn synthetic_ed25519_pkcs8_is_typed_validated_and_redacted() {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let pem = key.to_pkcs8_pem(Default::default()).unwrap();
        let material = SecretMaterial::BinanceEd25519 {
            api_key: "SYNTHETIC-ED25519-API-KEY".into(),
            private_key: pem.to_string(),
        };
        material.validate_present().unwrap();
        let debug = format!("{material:?}");
        assert!(!debug.contains("SYNTHETIC") && !debug.contains("PRIVATE KEY"));
        let encoded = serde_json::to_string(&material).unwrap();
        let decoded: SecretMaterial = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(decoded, SecretMaterial::BinanceEd25519 { .. }));
        let invalid = SecretMaterial::BinanceEd25519 {
            api_key: "SYNTHETIC-ED25519-API-KEY".into(),
            private_key:
                "-----BEGIN PRIVATE KEY-----\nSYNTHETIC-INVALID\n-----END PRIVATE KEY-----".into(),
        };
        let error = invalid.validate_present().unwrap_err();
        assert_eq!(error.code, "INVALID_CREDENTIAL");
        assert!(!error.to_string().contains("SYNTHETIC"));
    }
    #[test]
    fn coinbase_credentials_keep_algorithm_explicit_and_zeroize() {
        use base64::Engine as _;
        use ed25519_dalek::SigningKey;
        let key = SigningKey::from_bytes(&[0x21; 32]);
        let mut exported = key.to_bytes().to_vec();
        exported.extend_from_slice(key.verifying_key().as_bytes());
        let mut material = SecretMaterial::CoinbaseCdpEd25519 {
            api_key: "organizations/org/apiKeys/synthetic".into(),
            private_key_base64: base64::engine::general_purpose::STANDARD.encode(exported),
        };
        material.validate_present().unwrap();
        assert_eq!(format!("{material:?}"), "SecretMaterial(<redacted>)");
        let saved = serde_json::to_string(&material).unwrap();
        assert!(saved.contains("coinbase_cdp_ed25519"));
        material.zeroize();
        let SecretMaterial::CoinbaseCdpEd25519 {
            api_key,
            private_key_base64,
        } = &material
        else {
            panic!("typed Coinbase key")
        };
        assert!(api_key.is_empty() && private_key_base64.is_empty());

        let invalid = SecretMaterial::CoinbaseCdpEcdsa {
            api_key: "organizations/org/apiKeys/synthetic".into(),
            private_key_pem: "NOT A PEM OR A DIFFERENT ALGORITHM".into(),
        };
        let error = invalid.validate_present().unwrap_err();
        assert_eq!(error.code, "INVALID_CREDENTIAL");
        assert!(!error.to_string().contains("NOT A PEM"));
    }
    #[test]
    fn legacy_ethereum_key_records_are_reusable_evm_keys_without_secret_export() {
        let synthetic = "01".repeat(32); // Isolated fixture only, never a live key.
        let old = serde_json::json!({"type":"ethereum_private_key","private_key":synthetic});
        let material: SecretMaterial = serde_json::from_value(old).unwrap();
        material.validate_present().unwrap();
        assert!(matches!(material, SecretMaterial::EvmPrivateKey { .. }));
        assert_eq!(
            serde_json::to_value(&material).unwrap()["type"],
            "evm_private_key"
        );
        assert_eq!(format!("{material:?}"), "SecretMaterial(<redacted>)");
    }
    #[test]
    fn debug_and_bad_input_errors_never_echo_secret_values() {
        let sentinel = "SYNTHETIC-NEVER-A-REAL-SECRET";
        let material = SecretMaterial::EvmPrivateKey {
            private_key: sentinel.into(),
        };
        assert!(!format!("{material:?}").contains(sentinel));
        assert!(
            !material
                .validate_present()
                .unwrap_err()
                .to_string()
                .contains(sentinel)
        );
    }
    #[test]
    fn references_are_opaque_canonical_non_paths() {
        for bad in [
            "../secret",
            "/tmp/key",
            "not-a-uuid",
            "550E8400-E29B-41D4-A716-446655440000",
        ] {
            assert!(CredentialRef::new(bad).is_err());
        }
        assert!(CredentialRef::new("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(serde_json::from_str::<CredentialRef>("\"../secret\"").is_err());
    }
    #[test]
    fn unknown_secret_fields_are_rejected() {
        assert!(
            serde_json::from_str::<SecretMaterial>(
                r#"{"type":"binance_hmac","api_key":"SYNTHETIC","secret":"SYNTHETIC","extra":true}"#
            )
            .is_err()
        );
    }
    #[test]
    fn synthetic_store_files_use_new_private_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let material = SecretMaterial::BinanceHmac {
            api_key: "SYNTHETIC-KEY".into(),
            secret: "SYNTHETIC-SECRET".into(),
        };
        let first = write_material(dir.path(), &material).unwrap();
        let second = write_material(dir.path(), &material).unwrap();
        assert_ne!(first, second);
        let path = dir.path().join(format!("{}.json", first.as_str()));
        let value: SecretMaterial = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(matches!(value, SecretMaterial::BinanceHmac { .. }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        }
    }
    #[test]
    fn oversize_synthetic_material_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let material = SecretMaterial::BinanceHmac {
            api_key: "SYNTHETIC".into(),
            secret: "S".repeat(MAX_BYTES),
        };
        assert!(write_material(dir.path(), &material).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
