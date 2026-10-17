//! Closed EIP-712 signing contract for Papertrade.
//!
//! This is intentionally not a generic typed-data or `sign_hash` facility.
//! The primary types, domains, verifier contracts, and field encodings are
//! fixed here; broker code can only map reviewed adapter intents into one of
//! these variants.
use crate::{Address, B256, GoFinError, Result, U256, address};
use alloy::primitives::keccak256;
use std::{fmt, str::FromStr};

const CHAIN_ID: u64 = 999;
const EXCHANGE: &str = "0x6cd5661646289fb6e65ea5c032310fded797d0a2";
const PAPER_STAKING: &str = "0xaad6c7b0cc3014fc80ffedae0ed7ce5967b0f016";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
const MAX_INTENT_SECONDS: u64 = 60 * 60;
const MAX_SESSION_SECONDS: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DomainTarget {
    Exchange,
    PaperStaking,
    Cancellation,
}

/// Exact supported Papertrade EIP-712 primary messages. Numeric fields that
/// are uint256 are retained as canonical decimal strings so JSON and hashing
/// use exactly the same atoms without floating point conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PapertradeTypedIntent {
    RegisterSessionKey {
        user: String,
        key: String,
        expiry: u64,
        fee_address: String,
        nonce: String,
        deadline: u64,
    },
    OpenPosition {
        user: String,
        instrument_id: u32,
        is_long: bool,
        size: u128,
        leverage: u32,
        use_debt: bool,
        nonce: String,
        deadline: u64,
    },
    Close {
        user: String,
        position_ids: Vec<String>,
        nonce: String,
        deadline: u64,
    },
    WithdrawToCore {
        user: String,
        amount: String,
        nonce: String,
        deadline: u64,
    },
    Stake {
        user: String,
        amount: String,
        nonce: String,
        deadline: u64,
    },
    Unstake {
        user: String,
        amount: String,
        nonce: String,
        deadline: u64,
    },
    Claim {
        user: String,
        nonce: String,
        deadline: u64,
    },
    CancelIntent {
        user: String,
        intent_id: String,
        deadline: String,
    },
}

/// Signature fields are returned only to the protected broker dispatch path.
/// Keep Debug redacted because callers may log unexpected values.
pub struct SignedPapertradeIntent {
    pub intent: PapertradeTypedIntent,
    pub digest: String,
    pub signer: String,
    pub intent_id: String,
    pub v: String,
    pub r: String,
    pub s: String,
}

impl fmt::Debug for SignedPapertradeIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedPapertradeIntent")
            .field("intent", &self.intent)
            .field("signer", &self.signer)
            .field("intent_id", &self.intent_id)
            .field("signature", &"<redacted>")
            .finish()
    }
}

impl PapertradeTypedIntent {
    pub fn user(&self) -> &str {
        match self {
            Self::RegisterSessionKey { user, .. }
            | Self::OpenPosition { user, .. }
            | Self::Close { user, .. }
            | Self::WithdrawToCore { user, .. }
            | Self::Stake { user, .. }
            | Self::Unstake { user, .. }
            | Self::Claim { user, .. }
            | Self::CancelIntent { user, .. } => user,
        }
    }

    pub fn primary_type(&self) -> &'static str {
        match self {
            Self::RegisterSessionKey { .. } => "RegisterSessionKey",
            Self::OpenPosition { .. } => "OpenPosition",
            Self::Close { .. } => "Close",
            Self::WithdrawToCore { .. } => "WithdrawToCore",
            Self::Stake { .. } => "Stake",
            Self::Unstake { .. } => "Unstake",
            Self::Claim { .. } => "Claim",
            Self::CancelIntent { .. } => "CancelIntent",
        }
    }

    fn target(&self) -> DomainTarget {
        match self {
            Self::Stake { .. } | Self::Unstake { .. } | Self::Claim { .. } => {
                DomainTarget::PaperStaking
            }
            Self::CancelIntent { .. } => DomainTarget::Cancellation,
            _ => DomainTarget::Exchange,
        }
    }

    fn deadline(&self) -> Result<u64> {
        let deadline = match self {
            Self::RegisterSessionKey { deadline, .. }
            | Self::OpenPosition { deadline, .. }
            | Self::Close { deadline, .. }
            | Self::WithdrawToCore { deadline, .. }
            | Self::Stake { deadline, .. }
            | Self::Unstake { deadline, .. }
            | Self::Claim { deadline, .. } => *deadline,
            Self::CancelIntent { deadline, .. } => uint256(deadline)?
                .try_into()
                .map_err(|_| invalid("Cancellation deadline exceeds uint64."))?,
        };
        Ok(deadline)
    }

    /// Compute the only supported EIP-712 digest for this intent. `now` comes
    /// from the broker's clock. It prevents expired or unbounded payloads from
    /// reaching a protected signer.
    pub fn digest(&self, now: u64) -> Result<B256> {
        let user = nonzero_address(self.user())?;
        let deadline = self.deadline()?;
        if deadline <= now || deadline - now > MAX_INTENT_SECONDS {
            return Err(invalid(
                "Papertrade intent deadline must be within one hour.",
            ));
        }
        let struct_hash = match self {
            Self::RegisterSessionKey {
                key,
                expiry,
                fee_address,
                nonce,
                ..
            } => {
                let key = nonzero_address(key)?;
                let fee_address = address(fee_address)?;
                if fee_address.to_string().to_ascii_lowercase() != ZERO_ADDRESS
                    || *expiry <= now
                    || *expiry - now > MAX_SESSION_SECONDS
                {
                    return Err(invalid(
                        "Papertrade session registration requires the fixed zero fee address and expiry within 30 days.",
                    ));
                }
                let mut encoded = type_prefix(
                    "RegisterSessionKey(address user,address key,uint64 expiry,address feeAddress,uint256 nonce,uint64 deadline)",
                );
                push_address(&mut encoded, user);
                push_address(&mut encoded, key);
                push_u64(&mut encoded, *expiry);
                push_address(&mut encoded, fee_address);
                push_u256(&mut encoded, uint256(nonce)?);
                push_u64(&mut encoded, expiry_deadline(self)?);
                keccak256(encoded)
            }
            Self::OpenPosition {
                instrument_id,
                is_long,
                size,
                leverage,
                use_debt,
                nonce,
                deadline,
                ..
            } => {
                if !matches!(*instrument_id, 0 | 1) || *size == 0 || *leverage == 0 || *use_debt {
                    return Err(invalid(
                        "Papertrade opens require BTC or ETH, positive exact size and leverage, and cash collateral only.",
                    ));
                }
                let mut encoded = type_prefix(
                    "OpenPosition(address user,uint32 instrumentId,bool isLong,uint128 size,uint32 leverage,bool useDebt,uint256 nonce,uint64 deadline)",
                );
                push_address(&mut encoded, user);
                push_u64(&mut encoded, u64::from(*instrument_id));
                push_u64(&mut encoded, u64::from(*is_long));
                push_u128(&mut encoded, *size);
                push_u64(&mut encoded, u64::from(*leverage));
                push_u64(&mut encoded, u64::from(*use_debt));
                push_u256(&mut encoded, uint256(nonce)?);
                push_u64(&mut encoded, *deadline);
                keccak256(encoded)
            }
            Self::Close {
                position_ids,
                nonce,
                deadline,
                ..
            } => {
                if position_ids.is_empty() || position_ids.len() > 12 {
                    return Err(invalid(
                        "Papertrade close requires 1 to 12 exact position IDs.",
                    ));
                }
                let ids = position_ids
                    .iter()
                    .map(|id| uint256(id))
                    .collect::<Result<Vec<_>>>()?;
                if ids.iter().any(U256::is_zero)
                    || ids
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        != ids.len()
                {
                    return Err(invalid(
                        "Papertrade close position IDs must be nonzero and unique.",
                    ));
                }
                let mut array = Vec::with_capacity(ids.len() * 32);
                for id in ids {
                    push_u256(&mut array, id);
                }
                let ids_hash = keccak256(array);
                let mut encoded = type_prefix(
                    "Close(address user,uint256[] positionIds,uint256 nonce,uint64 deadline)",
                );
                push_address(&mut encoded, user);
                encoded.extend_from_slice(ids_hash.as_slice());
                push_u256(&mut encoded, uint256(nonce)?);
                push_u64(&mut encoded, *deadline);
                keccak256(encoded)
            }
            Self::WithdrawToCore {
                amount,
                nonce,
                deadline,
                ..
            } => {
                let amount = positive_uint256(amount)?;
                let mut encoded = type_prefix(
                    "WithdrawToCore(address user,uint256 amount,uint256 nonce,uint64 deadline)",
                );
                push_address(&mut encoded, user);
                push_u256(&mut encoded, amount);
                push_u256(&mut encoded, uint256(nonce)?);
                push_u64(&mut encoded, *deadline);
                keccak256(encoded)
            }
            Self::Stake {
                amount,
                nonce,
                deadline,
                ..
            } => {
                let amount = positive_uint256(amount)?;
                let mut encoded =
                    type_prefix("Stake(address user,uint256 amount,uint256 nonce,uint64 deadline)");
                push_address(&mut encoded, user);
                push_u256(&mut encoded, amount);
                push_u256(&mut encoded, uint256(nonce)?);
                push_u64(&mut encoded, *deadline);
                keccak256(encoded)
            }
            Self::Unstake {
                amount,
                nonce,
                deadline,
                ..
            } => {
                let amount = positive_uint256(amount)?;
                let mut encoded = type_prefix(
                    "Unstake(address user,uint256 amount,uint256 nonce,uint64 deadline)",
                );
                push_address(&mut encoded, user);
                push_u256(&mut encoded, amount);
                push_u256(&mut encoded, uint256(nonce)?);
                push_u64(&mut encoded, *deadline);
                keccak256(encoded)
            }
            Self::Claim {
                nonce, deadline, ..
            } => {
                let mut encoded = type_prefix("Claim(address user,uint256 nonce,uint64 deadline)");
                push_address(&mut encoded, user);
                push_u256(&mut encoded, uint256(nonce)?);
                push_u64(&mut encoded, *deadline);
                keccak256(encoded)
            }
            Self::CancelIntent {
                intent_id,
                deadline,
                ..
            } => {
                let id = B256::from_str(intent_id)
                    .map_err(|_| invalid("Cancellation intent ID must be bytes32."))?;
                if id.is_zero() {
                    return Err(invalid("Cancellation intent ID must be nonzero."));
                }
                let mut encoded =
                    type_prefix("CancelIntent(address user,bytes32 intentId,uint256 deadline)");
                push_address(&mut encoded, user);
                encoded.extend_from_slice(id.as_slice());
                push_u256(&mut encoded, uint256(deadline)?);
                keccak256(encoded)
            }
        };
        Ok(eip712_digest(domain_separator(self.target())?, struct_hash))
    }
}

pub fn intent_id(digest: B256, signer: Address) -> B256 {
    let prefix = keccak256("papertrade:intent-id:v2");
    let mut encoded = Vec::with_capacity(84);
    encoded.extend_from_slice(prefix.as_slice());
    encoded.extend_from_slice(digest.as_slice());
    encoded.extend_from_slice(signer.as_slice());
    keccak256(encoded)
}

fn expiry_deadline(intent: &PapertradeTypedIntent) -> Result<u64> {
    intent.deadline()
}

fn domain_separator(target: DomainTarget) -> Result<B256> {
    let (name, version, contract) = match target {
        DomainTarget::Exchange => ("String", "1", EXCHANGE),
        DomainTarget::PaperStaking => ("PaperStaking", "1", PAPER_STAKING),
        DomainTarget::Cancellation => ("PaperTrade Intent Cancel", "2", EXCHANGE),
    };
    let type_hash = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut encoded = Vec::with_capacity(160);
    encoded.extend_from_slice(type_hash.as_slice());
    encoded.extend_from_slice(keccak256(name).as_slice());
    encoded.extend_from_slice(keccak256(version).as_slice());
    push_u64(&mut encoded, CHAIN_ID);
    push_address(&mut encoded, address(contract)?);
    Ok(keccak256(encoded))
}

fn eip712_digest(domain: B256, structure: B256) -> B256 {
    let mut encoded = [0u8; 66];
    encoded[0] = 0x19;
    encoded[1] = 0x01;
    encoded[2..34].copy_from_slice(domain.as_slice());
    encoded[34..].copy_from_slice(structure.as_slice());
    keccak256(encoded)
}

fn type_prefix(primary_type: &str) -> Vec<u8> {
    let hash = keccak256(primary_type);
    let mut encoded = Vec::with_capacity(32 * 8);
    encoded.extend_from_slice(hash.as_slice());
    encoded
}

fn push_u64(encoded: &mut Vec<u8>, value: u64) {
    encoded.extend_from_slice(&U256::from(value).to_be_bytes::<32>());
}
fn push_u128(encoded: &mut Vec<u8>, value: u128) {
    encoded.extend_from_slice(&U256::from(value).to_be_bytes::<32>());
}
fn push_address(encoded: &mut Vec<u8>, value: Address) {
    encoded.extend_from_slice(&[0u8; 12]);
    encoded.extend_from_slice(value.as_slice());
}
fn push_u256(encoded: &mut Vec<u8>, value: U256) {
    encoded.extend_from_slice(&value.to_be_bytes::<32>());
}
fn uint256(value: &str) -> Result<U256> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(
            "Papertrade uint256 fields must be canonical decimal strings.",
        ));
    }
    U256::from_str(value).map_err(|_| invalid("Papertrade uint256 field overflows."))
}
fn positive_uint256(value: &str) -> Result<U256> {
    let value = uint256(value)?;
    if value.is_zero() {
        return Err(invalid("Papertrade amount must be positive."));
    }
    Ok(value)
}
fn nonzero_address(value: &str) -> Result<Address> {
    let parsed = address(value)?;
    if parsed.is_zero() {
        return Err(invalid(
            "Papertrade owner and session key addresses must be nonzero.",
        ));
    }
    Ok(parsed)
}
fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "INVALID_PAPERTRADE_TYPED_INTENT",
        "signing",
        message,
        "Prepare a fresh bounded Papertrade operation using the exact configured account and protocol.",
    )
}
