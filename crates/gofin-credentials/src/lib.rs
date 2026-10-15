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
    JupiterApiKey {
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
            Self::JupiterApiKey { api_key } => {
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
    /// Internal recovery inventory; never exposed through the client protocol.
    pub fn references(&self) -> Result<Vec<CredentialRef>> {
        gofin_authorization::validate_installation()?;
        let mut references = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|_| {
            error(
                "CREDENTIAL_STORE_UNAVAILABLE",
                "The protected credential inventory is unavailable.",
            )
        })? {
            let entry = entry.map_err(|_| {
                error(
                    "CREDENTIAL_STORE_UNAVAILABLE",
                    "The credential inventory could not be inspected.",
                )
            })?;
            let name = entry.file_name();
            let raw = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .ok_or_else(|| {
                    error(
                        "UNSAFE_CREDENTIAL_STORE",
                        "The protected store contains an unexpected file.",
                    )
                })?;
            let reference = CredentialRef::new(raw)?;
            gofin_authorization::validate_metadata(&entry.path(), true)?;
            references.push(reference);
        }
        references.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(references)
    }
    fn path_for(&self, reference: &CredentialRef) -> PathBuf {
        self.dir.join(format!("{}.json", reference.as_str()))
    }
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
