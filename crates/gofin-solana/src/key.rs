use std::fmt;

use ed25519_dalek::{Signer, SigningKey};
use serde::de::{DeserializeSeed, SeqAccess, Visitor};
use zeroize::{Zeroize, Zeroizing};

use crate::failure;
use gofin_domain::{GoFinError, Result};

/// Internal signing boundary consumed by the frozen transaction runtime.
pub(crate) trait MessageSigner: Send + Sync {
    fn sign_message(&self, message: &[u8]) -> Result<[u8; 64]>;
}

/// A validated Ed25519 Solana keypair. Formatting never reveals key material.
pub struct SolanaSigner {
    key: SigningKey,
    address: String,
}

impl fmt::Debug for SolanaSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SolanaSigner")
            .field("address", &self.address)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl SolanaSigner {
    /// Parse a conventional 64-byte base58 keypair or JSON array of 64 bytes.
    /// The public half must match the public key derived from the seed.
    pub fn parse(input: &str) -> Result<Self> {
        // Keep malformed enrollment input bounded before either decoder scans
        // it. A JSON keypair needs fewer than 300 bytes without whitespace;
        // allow a modest amount of formatting while rejecting giant payloads.
        if input.len() > 1024 {
            return Err(invalid_key());
        }
        let bytes = if input.trim_start().starts_with('[') {
            parse_json_key(input)?
        } else {
            decode_base58_key(input)?
        };
        Self::from_keypair(bytes)
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    fn from_keypair(mut bytes: Zeroizing<[u8; 64]>) -> Result<Self> {
        let mut seed = Zeroizing::new([0u8; 32]);
        seed.copy_from_slice(&bytes[..32]);
        let key = SigningKey::from_bytes(&seed);
        let derived = key.verifying_key().to_bytes();
        let matches = derived.as_slice() == &bytes[32..];
        bytes.zeroize();
        seed.zeroize();
        if !matches {
            return Err(invalid_key());
        }
        use ed25519_dalek::VerifyingKey;
        let address = bs58::encode(VerifyingKey::from(&key).to_bytes()).into_string();
        Ok(Self { key, address })
    }
}

impl MessageSigner for SolanaSigner {
    fn sign_message(&self, message: &[u8]) -> Result<[u8; 64]> {
        let signature = self.key.sign(message);
        Ok(signature.to_bytes())
    }
}

fn invalid_key() -> GoFinError {
    failure(
        "SOLANA_KEY_INVALID",
        "credentials",
        "The Solana keypair is invalid or its public half does not match.",
        "Provide a standard 64-byte Solana keypair through protected credential enrollment.",
    )
}

fn decode_base58_key(input: &str) -> Result<Zeroizing<[u8; 64]>> {
    let input = input.trim();
    if input.len() > 88 {
        return Err(invalid_key());
    }
    let mut output = Zeroizing::new([0u8; 64]);
    let written = bs58::decode(input)
        .onto(output.as_mut())
        .map_err(|_| invalid_key())?;
    if written != 64 {
        return Err(invalid_key());
    }
    Ok(output)
}

fn parse_json_key(input: &str) -> Result<Zeroizing<[u8; 64]>> {
    let mut de = serde_json::Deserializer::from_str(input);
    let parsed = KeyBytesSeed
        .deserialize(&mut de)
        .map_err(|_| invalid_key())?;
    de.end().map_err(|_| invalid_key())?;
    if parsed.0.len() != 64 {
        return Err(invalid_key());
    }
    let mut output = Zeroizing::new([0u8; 64]);
    output.copy_from_slice(&parsed.0);
    Ok(output)
}

struct KeyBytesSeed;
struct KeyBytes(Zeroizing<Vec<u8>>);

impl<'de> DeserializeSeed<'de> for KeyBytesSeed {
    type Value = KeyBytes;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(KeyArrayVisitor)
    }
}

struct KeyArrayVisitor;
impl<'de> Visitor<'de> for KeyArrayVisitor {
    type Value = KeyBytes;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of 64 key bytes")
    }
    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(64));
        while bytes.len() < 64 {
            match seq.next_element::<u64>()? {
                Some(byte) if byte <= u8::MAX as u64 => bytes.push(byte as u8),
                Some(_) => return Err(serde::de::Error::custom("invalid key byte")),
                None => return Ok(KeyBytes(bytes)),
            }
        }
        if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom("invalid key length"));
        }
        Ok(KeyBytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn fixture() -> Zeroizing<[u8; 64]> {
        let seed = [7u8; 32];
        let key = SigningKey::from_bytes(&seed);
        let mut pair = Zeroizing::new([0u8; 64]);
        pair[..32].copy_from_slice(&seed);
        pair[32..].copy_from_slice(&key.verifying_key().to_bytes());
        pair
    }

    #[test]
    fn accepts_base58_and_json_and_redacts_debug() {
        let pair = fixture();
        let encoded = bs58::encode(pair.as_ref()).into_string();
        let signer = SolanaSigner::parse(&encoded).unwrap();
        let json = serde_json::to_string(&pair.as_ref()).unwrap();
        let from_json = SolanaSigner::parse(&json).unwrap();
        assert_eq!(signer.address(), from_json.address());
        assert!(SolanaSigner::parse(&format!("{json} false")).is_err());
        assert!(SolanaSigner::parse(&format!("[{0}]", vec!["7"; 65].join(","))).is_err());
        assert!(SolanaSigner::parse(&format!("[{}1]", " ".repeat(1024))).is_err());
        assert!(format!("{signer:?}").contains("REDACTED"));
        assert!(!format!("{signer:?}").contains(&encoded));
    }

    #[test]
    fn rejects_wrong_length_and_mismatched_public_half_without_echoing_input() {
        assert!(
            SolanaSigner::parse("not a key")
                .unwrap_err()
                .to_string()
                .find("not a key")
                .is_none()
        );
        assert!(SolanaSigner::parse("[1,2,3]").is_err());
        let mut pair = fixture();
        pair[63] ^= 1;
        assert!(SolanaSigner::parse(&bs58::encode(pair.as_ref()).into_string()).is_err());
    }
}
