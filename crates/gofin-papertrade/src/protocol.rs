use async_trait::async_trait;
use gofin_domain::{AssetId, ConnectionMetadata, GoFinError, ResourceId, Result};
use num_traits::Num;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CHAIN_ID: u64 = 999;
pub const EXCHANGE: &str = "0x6cd5661646289fb6e65ea5c032310fded797d0a2";
pub const EXCHANGE_IMPLEMENTATION: &str = "0x0f3febfc94876b0c895f7acecdc8f5c863b549ee";
pub const EXCHANGE_CODE_HASH: &str =
    "0x9d20287df886415035d53344a0dc8a1044949308cc3283ee0fe7ea5f3ea28d92";
pub const BATCH_EXECUTOR: &str = "0x233ff29b239abc1d7748605da9b205c3e008626e";
pub const BATCH_EXECUTOR_IMPLEMENTATION: &str = "0xe0bc9cc7b067060aafccbaa3e250700c9cdd7262";
pub const BATCH_EXECUTOR_CODE_HASH: &str =
    "0xe926c659aca61daaaf9e68267817be4dae5152f11320a6ba970549dfdd15f62a";
pub const SESSION_KEY_MANAGER: &str = "0x8326eb8896e358717f10502ec73ce8d28e37b108";
pub const SESSION_KEY_MANAGER_IMPLEMENTATION: &str = "0x8a5069fe303e8f81e91825436e956d595032e558";
pub const SESSION_KEY_MANAGER_CODE_HASH: &str =
    "0x337787e630c4fe48e2cea45e1dbd7cb12be2254d158042ee1cd446e706d5c484";
pub const PAPER_TOKEN: &str = "0xe40f17915daa230324030003a197cdaef2261c0e";
pub const PAPER_STAKING: &str = "0xaad6c7b0cc3014fc80ffedae0ed7ce5967b0f016";
pub const PAPER_STAKING_IMPLEMENTATION: &str = "0xe6e3519455170913c5c3bfe541dd918a2d0cbb15";
pub const PAPER_STAKING_CODE_HASH: &str =
    "0x54573c497e5109e353cf5330343323915e0c0f26e1b410fd3cb2cd8d1460f154";
pub const DEPOSIT_PROXY_FACTORY: &str = "0x1629b7d46bef6e13ff754174fc9f32dea309b38f";
pub const PAPER_DECIMALS: u8 = 18;
pub const USDC_DECIMALS: u8 = 6;
pub const MAX_SESSION_SECONDS: u64 = 30 * 24 * 60 * 60;
pub const MAX_INTENT_SECONDS: u64 = 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadRequest {
    TradingState,
    AccountState {
        owner: String,
    },
    Positions {
        owner: String,
    },
    History {
        owner: String,
        since_ms: Option<u64>,
    },
    ProtocolSummary,
    StakingState {
        owner: String,
    },
}

/// Only protocol operations supported by GoFin. There is no raw endpoint,
/// calldata, action JSON, private key, or caller-selected typed-data domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Intent {
    Open {
        owner: String,
        instrument_id: u32,
        is_long: bool,
        #[serde(with = "decimal_u128")]
        #[schemars(with = "String")]
        margin_wad: u128,
        leverage: u32,
        use_debt: bool,
        nonce: String,
        deadline: u64,
    },
    Close {
        owner: String,
        position_ids: Vec<String>,
        nonce: String,
        deadline: u64,
    },
    Cancel {
        owner: String,
        intent_id: String,
        deadline: String,
    },
    RegisterSession {
        owner: String,
        /// Durable logical identity used by the broker to generate or recover
        /// one protected key across cancellation and process restart.
        registration_id: String,
        expiry: u64,
        nonce: String,
        deadline: u64,
    },
    RevokeSession {
        owner: String,
        registration_id: String,
    },
    Stake {
        owner: String,
        #[serde(with = "decimal_u128")]
        #[schemars(with = "String")]
        amount: u128,
        nonce: String,
        deadline: u64,
    },
    Unstake {
        owner: String,
        #[serde(with = "decimal_u128")]
        #[schemars(with = "String")]
        amount: u128,
        nonce: String,
        deadline: u64,
    },
    Claim {
        owner: String,
        nonce: String,
        deadline: u64,
    },
}

/// A signed EIP-712 message. Its variants and field types mirror the official
/// frontend's typed-data module; callers cannot choose a type, domain, or field
/// list. `nonce` and `deadline` are allocated by the protected runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "primary_type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PapertradeTypedIntent {
    RegisterSessionKey {
        user: String,
        key: String,
        expiry: u64,
        #[serde(rename = "feeAddress")]
        fee_address: String,
        nonce: String,
        deadline: u64,
    },
    OpenPosition {
        user: String,
        #[serde(rename = "instrumentId")]
        instrument_id: u32,
        #[serde(rename = "isLong")]
        is_long: bool,
        #[serde(with = "decimal_u128")]
        #[schemars(with = "String")]
        size: u128,
        leverage: u32,
        #[serde(rename = "useDebt")]
        use_debt: bool,
        nonce: String,
        deadline: u64,
    },
    Close {
        user: String,
        #[serde(rename = "positionIds")]
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
        #[serde(rename = "intentId")]
        intent_id: String,
        deadline: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PapertradeDomainTarget {
    Exchange,
    PaperStaking,
    Cancellation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Eip712Domain {
    pub name: String,
    pub version: String,
    pub chain_id: u64,
    pub verifying_contract: String,
}

impl PapertradeTypedIntent {
    pub fn domain(&self) -> Eip712Domain {
        let (name, version, contract) = match self.domain_target() {
            PapertradeDomainTarget::Exchange => ("String", "1", EXCHANGE),
            PapertradeDomainTarget::PaperStaking => ("PaperStaking", "1", PAPER_STAKING),
            PapertradeDomainTarget::Cancellation => ("PaperTrade Intent Cancel", "2", EXCHANGE),
        };
        Eip712Domain {
            name: name.into(),
            version: version.into(),
            chain_id: CHAIN_ID,
            verifying_contract: contract.into(),
        }
    }

    pub fn domain_target(&self) -> PapertradeDomainTarget {
        match self {
            Self::Stake { .. } | Self::Unstake { .. } | Self::Claim { .. } => {
                PapertradeDomainTarget::PaperStaking
            }
            Self::CancelIntent { .. } => PapertradeDomainTarget::Cancellation,
            _ => PapertradeDomainTarget::Exchange,
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

    pub fn action(&self) -> Option<&'static str> {
        match self {
            Self::RegisterSessionKey { .. } => Some("registerSessionKey"),
            Self::OpenPosition { .. } => Some("openPosition"),
            Self::Close { .. } => Some("closePositions"),
            Self::WithdrawToCore { .. } => Some("withdrawToCore"),
            Self::Stake { .. } => Some("paperStake"),
            Self::Unstake { .. } => Some("paperUnstake"),
            Self::Claim { .. } => Some("paperClaim"),
            Self::CancelIntent { .. } => None,
        }
    }

    pub fn validate(&self, now: u64) -> Result<()> {
        let user = match self {
            Self::RegisterSessionKey { user, .. }
            | Self::OpenPosition { user, .. }
            | Self::Close { user, .. }
            | Self::WithdrawToCore { user, .. }
            | Self::Stake { user, .. }
            | Self::Unstake { user, .. }
            | Self::Claim { user, .. }
            | Self::CancelIntent { user, .. } => user,
        };
        if !valid_address(user) {
            return Err(GoFinError::validation(
                "Papertrade typed intent owner is invalid.",
            ));
        }
        match self {
            Self::RegisterSessionKey {
                key,
                expiry,
                fee_address,
                nonce,
                deadline,
                ..
            } => {
                if !valid_address(key)
                    || fee_address != "0x0000000000000000000000000000000000000000"
                    || *expiry <= now
                    || *expiry - now > MAX_SESSION_SECONDS
                {
                    return Err(GoFinError::validation(
                        "Session registration requires a broker-owned key, zero fee address, and an expiry within 30 days.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::OpenPosition {
                instrument_id,
                size,
                leverage,
                use_debt,
                nonce,
                deadline,
                ..
            } => {
                if !matches!(*instrument_id, 0 | 1) || *size == 0 || *leverage == 0 || *use_debt {
                    return Err(GoFinError::validation(
                        "Papertrade opens require BTC/ETH, positive size and leverage, and useDebt=false.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::Close {
                position_ids,
                nonce,
                deadline,
                ..
            } => {
                if position_ids.is_empty()
                    || position_ids.len() > 12
                    || position_ids.iter().any(|x| !valid_uint256(x) || x == "0")
                {
                    return Err(GoFinError::validation(
                        "Papertrade close requires 1–12 exact nonzero position IDs.",
                    ));
                }
                let mut unique = position_ids.clone();
                unique.sort();
                unique.dedup();
                if unique.len() != position_ids.len() {
                    return Err(GoFinError::validation(
                        "Papertrade close position IDs must be unique.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::WithdrawToCore {
                amount,
                nonce,
                deadline,
                ..
            }
            | Self::Stake {
                amount,
                nonce,
                deadline,
                ..
            }
            | Self::Unstake {
                amount,
                nonce,
                deadline,
                ..
            } => {
                if !valid_uint256(amount) || amount == "0" {
                    return Err(GoFinError::validation(
                        "Papertrade amount must be a positive uint256 atomic quantity.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::Claim {
                nonce, deadline, ..
            } => validate_deadline(nonce, *deadline, now)?,
            Self::CancelIntent {
                intent_id,
                deadline,
                ..
            } => {
                if !valid_bytes32(intent_id)
                    || !valid_uint256(deadline)
                    || deadline == "0"
                    || num_bigint::BigUint::from_str_radix(deadline, 10)
                        .is_ok_and(|d| d <= now.into() || d > (now + MAX_INTENT_SECONDS).into())
                {
                    return Err(GoFinError::validation(
                        "Cancellation needs a 32-byte intent ID and deadline within one hour.",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The broker returns this unsigned binding after allocating a durable nonce
/// and resolving a protected wallet/session key. It contains no signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundIntent {
    pub typed: PapertradeTypedIntent,
    pub intent_id: String,
    pub signer: String,
    pub action: Option<String>,
    #[serde(default)]
    pub registration_id: Option<String>,
    /// For cancellation only, the exact original action resolved from the
    /// enrolled owner's intent snapshot and frozen with the approved target.
    #[serde(default)]
    pub cancel_target_action: Option<String>,
}
impl BoundIntent {
    pub fn matches_request(&self, request: &IntentRequest) -> bool {
        let action_ok = self.action.as_deref() == self.typed.action();
        if !valid_bytes32(&self.intent_id)
            || self.intent_id.eq_ignore_ascii_case(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            )
            || !valid_address(&self.signer)
            || !action_ok
        {
            return false;
        }
        match (&self.typed, request) {
            (
                PapertradeTypedIntent::OpenPosition {
                    user,
                    instrument_id,
                    is_long,
                    size,
                    leverage,
                    use_debt,
                    ..
                },
                IntentRequest::Open {
                    owner,
                    instrument_id: ri,
                    is_long: rl,
                    margin_wad,
                    leverage: rr,
                    ..
                },
            ) => {
                user.eq_ignore_ascii_case(owner)
                    && instrument_id == ri
                    && is_long == rl
                    && *size == *margin_wad
                    && leverage == rr
                    && !*use_debt
            }
            (
                PapertradeTypedIntent::Close {
                    user, position_ids, ..
                },
                IntentRequest::Close {
                    owner,
                    position_ids: ids,
                },
            ) => user.eq_ignore_ascii_case(owner) && position_ids == ids,
            (
                PapertradeTypedIntent::CancelIntent {
                    user, intent_id, ..
                },
                IntentRequest::Cancel {
                    owner,
                    intent_id: id,
                },
            ) => {
                user.eq_ignore_ascii_case(owner)
                    && intent_id.eq_ignore_ascii_case(id)
                    && self
                        .cancel_target_action
                        .as_deref()
                        .is_some_and(is_cancellable_target_action)
            }
            (
                PapertradeTypedIntent::RegisterSessionKey { user, expiry, .. },
                IntentRequest::RegisterSession {
                    owner,
                    registration_id,
                    expiry: e,
                    ..
                },
            ) => {
                user.eq_ignore_ascii_case(owner)
                    && expiry == e
                    && self.registration_id.as_deref() == Some(registration_id.as_str())
            }
            (
                PapertradeTypedIntent::Stake { user, amount, .. },
                IntentRequest::Stake { owner, amount: a },
            ) => user.eq_ignore_ascii_case(owner) && amount == &a.to_string(),
            (
                PapertradeTypedIntent::Unstake { user, amount, .. },
                IntentRequest::Unstake { owner, amount: a },
            ) => user.eq_ignore_ascii_case(owner) && amount == &a.to_string(),
            (PapertradeTypedIntent::Claim { user, .. }, IntentRequest::Claim { owner }) => {
                user.eq_ignore_ascii_case(owner)
            }
            _ => false,
        }
    }
}

pub fn is_cancellable_target_action(action: &str) -> bool {
    matches!(
        action,
        "registerSessionKey"
            | "openPosition"
            | "closePositions"
            | "withdrawToCore"
            | "paperStake"
            | "paperUnstake"
            | "paperClaim"
    )
}

/// Immutable, broker-prepared EVM transaction for the owner-signed session
/// revocation. It is frozen before the protected journal write and cannot
/// address a different contract, chain, selector, account or session key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundSessionRevoke {
    pub owner: String,
    pub session_key: String,
    pub registration_id: String,
    pub submission_id: String,
    pub transaction: gofin_evm::FrozenTransaction,
}

/// Public, durable identity for a broker-held session registration. The
/// session address is deliberately omitted; the protected credential store
/// retains the complete typed message under registration ID and operation ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRegistrationBinding {
    pub owner: String,
    pub registration_id: String,
    pub intent_id: String,
    pub signer: String,
    pub expiry: u64,
    pub nonce: String,
    pub deadline: u64,
}

impl SessionRegistrationBinding {
    pub fn from_bound(registration_id: String, intent: &BoundIntent) -> Result<Self> {
        let PapertradeTypedIntent::RegisterSessionKey {
            user,
            expiry,
            nonce,
            deadline,
            ..
        } = &intent.typed
        else {
            return Err(GoFinError::validation(
                "Expected a typed Papertrade session registration.",
            ));
        };
        if registration_id.trim().is_empty()
            || !valid_bytes32(&intent.intent_id)
            || !valid_address(user)
            || !valid_address(&intent.signer)
        {
            return Err(GoFinError::validation(
                "Session registration binding identity is invalid.",
            ));
        }
        Ok(Self {
            owner: user.clone(),
            registration_id,
            intent_id: intent.intent_id.clone(),
            signer: intent.signer.clone(),
            expiry: *expiry,
            nonce: nonce.clone(),
            deadline: *deadline,
        })
    }
}

/// Public durable revocation identity. The frozen transaction calldata and
/// session address stay in the broker's protected registration journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRevocationBinding {
    pub owner: String,
    pub registration_id: String,
    pub submission_id: String,
}

/// Canonical read-only evidence that a relayed registration signature can no
/// longer be executed and has not consumed its protected nonce. The broker
/// obtains the latest canonical block and the code-pinned SessionKeyManager
/// state at that same block; this proves failure of an old uncertain
/// registration without inventing a dispatch block or replaying it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRegistrationExpiryProof {
    pub chain_id: u64,
    pub owner: String,
    pub registration_id: String,
    pub intent_id: String,
    pub nonce: String,
    pub deadline: u64,
    pub canonical_block_number: u64,
    pub canonical_block_hash: String,
    pub canonical_block_timestamp: u64,
    pub session_key_manager_proxy: String,
    pub session_key_manager_implementation: String,
    pub session_key_manager_code_hash: String,
    pub nonce_unused: bool,
    pub session_absent: bool,
}

impl SessionRegistrationExpiryProof {
    pub fn validate_against(&self, binding: &SessionRegistrationBinding) -> Result<()> {
        let bytes32 = |value: &str| {
            value.len() == 66
                && value.starts_with("0x")
                && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        if self.chain_id != CHAIN_ID
            || !self.owner.eq_ignore_ascii_case(&binding.owner)
            || self.registration_id != binding.registration_id
            || !self.intent_id.eq_ignore_ascii_case(&binding.intent_id)
            || self.nonce != binding.nonce
            || self.deadline != binding.deadline
            || self.canonical_block_number == 0
            || !bytes32(&self.canonical_block_hash)
            || self.canonical_block_timestamp <= binding.deadline
            || !self
                .session_key_manager_proxy
                .eq_ignore_ascii_case(SESSION_KEY_MANAGER)
            || !self
                .session_key_manager_implementation
                .eq_ignore_ascii_case(SESSION_KEY_MANAGER_IMPLEMENTATION)
            || !self
                .session_key_manager_code_hash
                .eq_ignore_ascii_case(SESSION_KEY_MANAGER_CODE_HASH)
            || !self.nonce_unused
            || !self.session_absent
        {
            return Err(GoFinError::validation(
                "Canonical expiry proof does not prove this exact registration nonce is unused after its deadline under the reviewed SessionKeyManager implementation.",
            ));
        }
        Ok(())
    }
}

/// Canonical proof that an uncertain owner-signed WithdrawToCore intent can
/// no longer execute. The runtime only returns this after verifying the same
/// canonical block, the reviewed Exchange and SessionKeyManager code pins,
/// unused owner nonce, and absence of the exact intent ID from bounded logs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoreWithdrawalExpiryProof {
    pub chain_id: u64,
    pub owner: String,
    pub intent_id: String,
    pub nonce: String,
    pub deadline: u64,
    pub canonical_block_number: u64,
    pub canonical_block_hash: String,
    pub canonical_block_timestamp: u64,
    pub exchange_proxy: String,
    pub exchange_implementation: String,
    pub exchange_code_hash: String,
    pub session_key_manager_proxy: String,
    pub session_key_manager_implementation: String,
    pub session_key_manager_code_hash: String,
    pub nonce_unused: bool,
    pub intent_absent: bool,
}

/// Canonical proof that an uncertain owner/session-signed trading or staking
/// intent can no longer execute. The proof is deliberately unavailable for
/// cancellation (which has no nonce) and WithdrawToCore (which has its own
/// stricter route-specific proof). All contract reads and the bounded exact
/// intent lookup are tied to the same post-deadline canonical block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PapertradeIntentExpiryProof {
    pub chain_id: u64,
    pub owner: String,
    pub intent_id: String,
    pub nonce: String,
    pub deadline: u64,
    pub canonical_block_number: u64,
    pub canonical_block_hash: String,
    pub canonical_block_timestamp: u64,
    pub target_proxy: String,
    pub target_implementation: String,
    pub target_code_hash: String,
    pub nonce_proxy: String,
    pub nonce_implementation: String,
    pub nonce_code_hash: String,
    pub batch_executor_proxy: String,
    pub batch_executor_implementation: String,
    pub batch_executor_code_hash: String,
    pub nonce_unused: bool,
    pub intent_absent: bool,
}

impl PapertradeIntentExpiryProof {
    pub fn validate_against(&self, bound: &BoundIntent) -> Result<()> {
        let bytes32 = |value: &str| {
            value.len() == 66
                && value.starts_with("0x")
                && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        let (
            owner,
            nonce,
            deadline,
            target,
            target_implementation,
            target_hash,
            nonce_contract,
            nonce_implementation,
            nonce_hash,
        ) = match &bound.typed {
            PapertradeTypedIntent::OpenPosition {
                user,
                nonce,
                deadline,
                ..
            }
            | PapertradeTypedIntent::Close {
                user,
                nonce,
                deadline,
                ..
            } => (
                user,
                nonce,
                deadline,
                EXCHANGE,
                EXCHANGE_IMPLEMENTATION,
                EXCHANGE_CODE_HASH,
                SESSION_KEY_MANAGER,
                SESSION_KEY_MANAGER_IMPLEMENTATION,
                SESSION_KEY_MANAGER_CODE_HASH,
            ),
            PapertradeTypedIntent::Stake {
                user,
                nonce,
                deadline,
                ..
            }
            | PapertradeTypedIntent::Unstake {
                user,
                nonce,
                deadline,
                ..
            }
            | PapertradeTypedIntent::Claim {
                user,
                nonce,
                deadline,
            } => (
                user,
                nonce,
                deadline,
                PAPER_STAKING,
                PAPER_STAKING_IMPLEMENTATION,
                PAPER_STAKING_CODE_HASH,
                PAPER_STAKING,
                PAPER_STAKING_IMPLEMENTATION,
                PAPER_STAKING_CODE_HASH,
            ),
            PapertradeTypedIntent::CancelIntent { .. }
            | PapertradeTypedIntent::WithdrawToCore { .. }
            | PapertradeTypedIntent::RegisterSessionKey { .. } => {
                return Err(GoFinError::validation(
                    "This Papertrade intent requires a different terminal-proof path.",
                ));
            }
        };
        if self.chain_id != CHAIN_ID
            || !self.owner.eq_ignore_ascii_case(owner)
            || !self.intent_id.eq_ignore_ascii_case(&bound.intent_id)
            || self.nonce != *nonce
            || self.deadline != *deadline
            || self.canonical_block_number == 0
            || !bytes32(&self.canonical_block_hash)
            || self.canonical_block_timestamp <= *deadline
            || !self.target_proxy.eq_ignore_ascii_case(target)
            || !self
                .target_implementation
                .eq_ignore_ascii_case(target_implementation)
            || !self.target_code_hash.eq_ignore_ascii_case(target_hash)
            || !self.nonce_proxy.eq_ignore_ascii_case(nonce_contract)
            || !self
                .nonce_implementation
                .eq_ignore_ascii_case(nonce_implementation)
            || !self.nonce_code_hash.eq_ignore_ascii_case(nonce_hash)
            || !self
                .batch_executor_proxy
                .eq_ignore_ascii_case(BATCH_EXECUTOR)
            || !self
                .batch_executor_implementation
                .eq_ignore_ascii_case(BATCH_EXECUTOR_IMPLEMENTATION)
            || !self
                .batch_executor_code_hash
                .eq_ignore_ascii_case(BATCH_EXECUTOR_CODE_HASH)
            || !self.nonce_unused
            || !self.intent_absent
        {
            return Err(GoFinError::validation(
                "Canonical expiry proof does not bind this exact Papertrade intent, nonce and reviewed implementation state after its deadline.",
            ));
        }
        Ok(())
    }
}

impl CoreWithdrawalExpiryProof {
    pub fn validate_against(&self, bound: &BoundIntent) -> Result<()> {
        let bytes32 = |value: &str| {
            value.len() == 66
                && value.starts_with("0x")
                && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        let PapertradeTypedIntent::WithdrawToCore {
            user,
            nonce,
            deadline,
            ..
        } = &bound.typed
        else {
            return Err(GoFinError::validation(
                "Expected a bound Papertrade WithdrawToCore intent.",
            ));
        };
        if self.chain_id != CHAIN_ID
            || !self.owner.eq_ignore_ascii_case(user)
            || !self.intent_id.eq_ignore_ascii_case(&bound.intent_id)
            || self.nonce != *nonce
            || self.deadline != *deadline
            || self.canonical_block_number == 0
            || !bytes32(&self.canonical_block_hash)
            || self.canonical_block_timestamp <= *deadline
            || !self.exchange_proxy.eq_ignore_ascii_case(EXCHANGE)
            || !self
                .exchange_implementation
                .eq_ignore_ascii_case(EXCHANGE_IMPLEMENTATION)
            || !self
                .exchange_code_hash
                .eq_ignore_ascii_case(EXCHANGE_CODE_HASH)
            || !self
                .session_key_manager_proxy
                .eq_ignore_ascii_case(SESSION_KEY_MANAGER)
            || !self
                .session_key_manager_implementation
                .eq_ignore_ascii_case(SESSION_KEY_MANAGER_IMPLEMENTATION)
            || !self
                .session_key_manager_code_hash
                .eq_ignore_ascii_case(SESSION_KEY_MANAGER_CODE_HASH)
            || !self.nonce_unused
            || !self.intent_absent
        {
            return Err(GoFinError::validation(
                "Canonical expiry proof does not prove this exact WithdrawToCore intent is unused after its deadline under reviewed protocol implementations.",
            ));
        }
        Ok(())
    }
}

impl BoundSessionRevoke {
    pub fn validate(&self) -> Result<()> {
        let data = self
            .transaction
            .calldata
            .strip_prefix("0x")
            .ok_or_else(|| GoFinError::validation("Revocation calldata must be 0x-prefixed."))?;
        let bytes = hex::decode(data)
            .map_err(|_| GoFinError::validation("Revocation calldata is malformed."))?;
        if !valid_address(&self.owner)
            || !valid_address(&self.session_key)
            || self.registration_id.trim().is_empty()
            || self.submission_id.trim().is_empty()
            || self.transaction.chain_id != CHAIN_ID
            || !self.transaction.sender.eq_ignore_ascii_case(&self.owner)
            || self.transaction.to.to_ascii_lowercase() != SESSION_KEY_MANAGER
            || !self.transaction.value.is_zero()
            || bytes.len() != 4 + 32
            || bytes[..4] != [0x84, 0xf4, 0xfc, 0x6a]
            || bytes[4..16].iter().any(|x| *x != 0)
            || bytes[16..36] != address_bytes(&self.session_key)?
        {
            return Err(GoFinError::validation(
                "Session revocation transaction is not the fixed zero-value owner-signed SessionKeyManager call.",
            ));
        }
        self.transaction.consensus()?;
        Ok(())
    }
}

fn address_bytes(value: &str) -> Result<Vec<u8>> {
    let bytes = hex::decode(value.strip_prefix("0x").unwrap_or_default())
        .map_err(|_| GoFinError::validation("Address is malformed."))?;
    if bytes.len() != 20 {
        return Err(GoFinError::validation("Address must be 20 bytes."));
    }
    Ok(bytes)
}

/// User-approved economic request. Nonces, signatures and session public keys
/// are added by broker-owned credential and nonce services.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IntentRequest {
    Open {
        owner: String,
        instrument_id: u32,
        is_long: bool,
        #[serde(with = "decimal_u128")]
        #[schemars(with = "String")]
        margin_wad: u128,
        leverage: u32,
    },
    Close {
        owner: String,
        position_ids: Vec<String>,
    },
    Cancel {
        owner: String,
        intent_id: String,
    },
    RegisterSession {
        owner: String,
        registration_id: String,
        expiry: u64,
    },
    RevokeSession {
        owner: String,
        registration_id: String,
    },
    Stake {
        owner: String,
        #[serde(with = "decimal_u128")]
        #[schemars(with = "String")]
        amount: u128,
    },
    Unstake {
        owner: String,
        #[serde(with = "decimal_u128")]
        #[schemars(with = "String")]
        amount: u128,
    },
    Claim {
        owner: String,
    },
}

/// Keep protocol atom quantities decimal and exact across durable JSON
/// bindings. The visitor also accepts old integer-form records for upgrade
/// compatibility where the JSON parser retained their full precision.
mod decimal_u128 {
    use serde::{Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        struct DecimalVisitor;
        impl<'de> de::Visitor<'de> for DecimalVisitor {
            type Value = u128;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a decimal u128 string or integer")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                self.visit_str(&value)
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(u128::from(value))
            }
            fn visit_u128<E: de::Error>(self, value: u128) -> Result<Self::Value, E> {
                Ok(value)
            }
        }
        deserializer.deserialize_any(DecimalVisitor)
    }
}

impl Intent {
    pub fn owner(&self) -> &str {
        match self {
            Self::Open { owner, .. }
            | Self::Close { owner, .. }
            | Self::Cancel { owner, .. }
            | Self::RegisterSession { owner, .. }
            | Self::RevokeSession { owner, .. }
            | Self::Stake { owner, .. }
            | Self::Unstake { owner, .. }
            | Self::Claim { owner, .. } => owner,
        }
    }

    /// Enforce the contract boundary before the broker signs. `now` must come
    /// from the broker clock, not caller input.
    pub fn validate(&self, now: u64) -> Result<()> {
        if !valid_address(self.owner()) {
            return Err(GoFinError::validation(
                "Papertrade owner address is invalid.",
            ));
        }
        match self {
            Self::Open {
                instrument_id,
                margin_wad,
                leverage,
                use_debt,
                nonce,
                deadline,
                ..
            } => {
                if !matches!(*instrument_id, 0 | 1)
                    || *margin_wad == 0
                    || *leverage == 0
                    || *use_debt
                {
                    return Err(GoFinError::validation(
                        "Papertrade opens are limited to BTC (0) or ETH (1), positive exact margin and leverage, with cash collateral only.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::Close {
                position_ids,
                nonce,
                deadline,
                ..
            } => {
                if position_ids.is_empty()
                    || position_ids.len() > 12
                    || position_ids
                        .iter()
                        .any(|id| !valid_uint256(id) || id == "0")
                {
                    return Err(GoFinError::validation(
                        "A Papertrade close requires 1 to 12 exact nonzero position IDs.",
                    ));
                }
                let mut ids = position_ids.clone();
                ids.sort();
                ids.dedup();
                if ids.len() != position_ids.len() {
                    return Err(GoFinError::validation(
                        "Papertrade close position IDs must be unique.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::Cancel {
                intent_id,
                deadline,
                ..
            } => {
                if !valid_bytes32(intent_id) || !valid_uint256(deadline) || deadline == "0" {
                    return Err(GoFinError::validation(
                        "Papertrade cancellation needs a 32-byte intent ID and uint256 deadline.",
                    ));
                }
            }
            Self::RegisterSession {
                registration_id,
                expiry,
                nonce,
                deadline,
                ..
            } => {
                if registration_id.trim().is_empty()
                    || *expiry <= now
                    || *expiry - now > MAX_SESSION_SECONDS
                {
                    return Err(GoFinError::validation(
                        "Papertrade session needs a durable registration ID and an expiry within the next 30 days.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::RevokeSession {
                registration_id, ..
            } => {
                if registration_id.trim().is_empty() {
                    return Err(GoFinError::validation(
                        "Papertrade revocation requires a registered session identity.",
                    ));
                }
            }
            Self::Stake {
                amount,
                nonce,
                deadline,
                ..
            }
            | Self::Unstake {
                amount,
                nonce,
                deadline,
                ..
            } => {
                if *amount == 0 {
                    return Err(GoFinError::validation(
                        "PAPER amount must be a positive exact integer in token atoms.",
                    ));
                }
                validate_deadline(nonce, *deadline, now)?;
            }
            Self::Claim {
                nonce, deadline, ..
            } => validate_deadline(nonce, *deadline, now)?,
        }
        Ok(())
    }
}

impl IntentRequest {
    pub fn owner(&self) -> &str {
        match self {
            Self::Open { owner, .. }
            | Self::Close { owner, .. }
            | Self::Cancel { owner, .. }
            | Self::RegisterSession { owner, .. }
            | Self::RevokeSession { owner, .. }
            | Self::Stake { owner, .. }
            | Self::Unstake { owner, .. }
            | Self::Claim { owner } => owner,
        }
    }
    pub fn validate(&self, now: u64) -> Result<()> {
        if !valid_address(self.owner()) {
            return Err(GoFinError::validation(
                "Papertrade owner address is invalid.",
            ));
        }
        match self {
            Self::Open {
                instrument_id,
                margin_wad,
                leverage,
                ..
            } if !matches!(*instrument_id, 0 | 1) || *margin_wad == 0 || *leverage == 0 => {
                return Err(GoFinError::validation(
                    "Open requires BTC/ETH, positive margin, and positive leverage.",
                ));
            }
            Self::Close { position_ids, .. } => {
                if position_ids.is_empty()
                    || position_ids.len() > 12
                    || position_ids
                        .iter()
                        .any(|id| !valid_uint256(id) || id == "0")
                {
                    return Err(GoFinError::validation(
                        "Close requires 1–12 exact nonzero position IDs.",
                    ));
                }
                let mut ids = position_ids.clone();
                ids.sort();
                ids.dedup();
                if ids.len() != position_ids.len() {
                    return Err(GoFinError::validation("Close position IDs must be unique."));
                }
            }
            Self::Cancel { intent_id, .. } if !valid_bytes32(intent_id) => {
                return Err(GoFinError::validation(
                    "Cancel requires a 32-byte intent ID.",
                ));
            }
            Self::RegisterSession {
                registration_id,
                expiry,
                ..
            } if registration_id.trim().is_empty()
                || *expiry <= now
                || *expiry - now > MAX_SESSION_SECONDS =>
            {
                return Err(GoFinError::validation(
                    "Session expiry must be within the next 30 days.",
                ));
            }
            Self::RevokeSession {
                registration_id, ..
            } if registration_id.trim().is_empty() => {
                return Err(GoFinError::validation(
                    "Session revocation requires a registration identity.",
                ));
            }
            Self::Stake { amount, .. } | Self::Unstake { amount, .. } if *amount == 0 => {
                return Err(GoFinError::validation("PAPER amount must be positive."));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub intent_id: String,
    pub action: String,
    pub status: String,
    #[serde(default)]
    pub transaction_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "status", deny_unknown_fields)]
pub enum Reconciliation {
    Pending {
        intent_id: String,
    },
    Settled {
        intent_id: String,
        transaction_hash: String,
        canonical_event: String,
        effects: Value,
    },
    Rejected {
        intent_id: String,
        reason: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancelIntentState {
    Cancelled,
    Succeeded,
    Failed,
    Expired,
    Unknown,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancelStatusSource {
    CancelEndpoint,
    WalletSse,
}

/// A protected-runtime normalized observation of the exact target intent's
/// terminal outcome. Endpoint responses and owner-scoped SSE patches have
/// different freshness fields, so both are retained.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelStatusProof {
    pub owner: String,
    pub target_intent_id: String,
    pub action: String,
    pub state: CancelIntentState,
    pub source: CancelStatusSource,
    #[serde(default)]
    pub account_revision: Option<u64>,
    #[serde(default)]
    pub indexed_through_block: Option<u64>,
}

/// Funding orchestration request. The route and asset are fixed identifiers;
/// the runtime returns evidence for the exact net credit/debit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FundingIntent {
    pub direction: FundingDirection,
    pub route: FundingRoute,
    pub owner: String,
    pub amount_usdc_atoms: u64,
    pub request_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FundingDirection {
    Deposit,
    Withdraw,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FundingRoute {
    Core,
    Hyperevm,
    Ethereum,
    Arbitrum,
    Solana,
}

/// Which boundary a settled funding receipt proves. Multi-leg withdrawals
/// first settle the exact Papertrade-to-Core credit; their external route is
/// handled by a separately authorized continuation operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FundingReceiptStage {
    /// The requested route destination has been canonically credited.
    #[default]
    FinalDestination,
    /// Papertrade cash has reached the owner's Hyperliquid Core spot balance.
    CoreCredit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FundingReceipt {
    /// Whether this receipt proves the final route or only its Core-credit
    /// first stage. Older persisted receipts are final-destination receipts.
    #[serde(default)]
    pub stage: FundingReceiptStage,
    pub status: String,
    /// Legacy net-credited amount for compatibility with existing callers.
    pub amount_usdc_atoms: u64,
    /// Exact amount debited from the funding source, including any additional
    /// source-side USDC fee, in source USDC atoms.
    pub source_debit_usdc_atoms: u64,
    /// Exact amount credited at the destination, after protocol deductions.
    pub destination_credit_usdc_atoms: u64,
    pub actual_source_fee_usdc_atoms: u64,
    pub actual_cctp_fee_usdc_atoms: u64,
    /// USDC retained at the route source (for example the documented Core
    /// forwarding reserve) rather than charged as a fee or credited to the
    /// final destination.
    #[serde(default)]
    pub retained_source_credit_usdc_atoms: u64,
    /// Exact native rent retained by a source-owned protocol account, when
    /// the funding route creates one (for example Circle's Solana MessageSent
    /// account). This is refundable account funding, not network fee.
    #[serde(default)]
    pub retained_source_native_atoms: Option<String>,
    #[serde(default)]
    pub retained_source_native_asset: Option<String>,
    pub activation_fee_usdc_atoms: u64,
    /// Actual native fee in the route's native atom unit, when a native fee
    /// was paid and proven from canonical execution evidence.
    pub actual_native_fee_atoms: Option<String>,
    pub actual_native_fee_asset: Option<String>,
    pub source_identity: String,
    pub destination_identity: String,
    pub evidence_id: String,
}

/// Public reference to a private broker-persisted funding plan. Calldata,
/// signatures, route quotes and per-leg requests remain in broker storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FundingBinding {
    pub version: u32,
    pub operation_id: String,
    pub intent: FundingIntent,
    /// Exact protected funding source and destination account identities.
    /// These come from the broker's enrolled accounts (including a distinct
    /// Solana signer when applicable), not from public operation input.
    pub source_identity: String,
    pub destination_identity: String,
    /// Immutable cost and settlement bounds selected by the broker's route
    /// policy. These bounds are public approval material; the per-leg plan
    /// and signed payloads remain private to the broker.
    pub bounds: FundingFinancialBounds,
    /// The one reviewed EVM envelope, when this route has an EVM leg. It is
    /// part of the authorized binding because nested dispatch validates this
    /// exact unsigned transaction before signing.
    #[serde(default)]
    pub transaction: Option<gofin_evm::FrozenTransaction>,
    /// Exact reviewed Solana message identity for a source burn. The raw
    /// signed transaction remains private to the broker runtime; this stable
    /// summary binds its signer, fee limit, recent blockhash and message hash.
    #[serde(default)]
    pub solana_transaction: Option<SolanaFundingTransactionBinding>,
    /// Exact source event-account address for rent-bearing Solana CCTP burns.
    #[serde(default)]
    pub solana_message_event_account: Option<String>,
    /// The one exact owner-signed Papertrade intent for Core withdrawals,
    /// when the route first withdraws Papertrade cash into HyperCore.
    #[serde(default)]
    pub papertrade_intent: Option<BoundIntent>,
}

/// Public identity for the onward leg of a multi-stage Papertrade withdrawal.
/// Stage one is the separate `papertrade.withdraw` operation that proves the
/// exact Papertrade-to-Core credit. This binding carries only that immutable
/// parent reference and the approved stage-two bounds; per-leg payloads stay
/// in the broker's protected funding journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FundingContinuationBinding {
    pub version: u32,
    /// Logical operation ID for this stage-two execution.
    pub operation_id: String,
    /// Immutable operation ID of the Papertrade withdrawal (stage one).
    pub route_id: String,
    pub parent_submission: gofin_domain::Submission,
    pub plan_fingerprint: String,
    pub stage1_proof_fingerprint: String,
    pub intent: FundingIntent,
    /// HyperCore source and exact enrolled external destination.
    pub source_identity: String,
    pub destination_identity: String,
    pub bounds: FundingFinancialBounds,
}

impl FundingContinuationBinding {
    pub fn validate(&self) -> Result<()> {
        let digest = |value: &str| {
            value.len() == 66
                && value.starts_with("0x")
                && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        if self.version != 1
            || self.operation_id.trim().is_empty()
            || self.route_id.trim().is_empty()
            || self.operation_id == self.route_id
            || self.intent.direction != FundingDirection::Withdraw
            || self.intent.route == FundingRoute::Core
            || self.intent.owner.trim().is_empty()
            || self.intent.amount_usdc_atoms < 10_000_000
            || self.parent_submission.kind != "papertrade_funding"
            || self.parent_submission.reference != self.route_id
            || self
                .parent_submission
                .data
                .get("operation_id")
                .and_then(Value::as_str)
                != Some(self.route_id.as_str())
            || !digest(&self.plan_fingerprint)
            || !digest(&self.stage1_proof_fingerprint)
        {
            return Err(GoFinError::validation(
                "Papertrade funding continuation binding is incomplete or does not match its stage-one withdrawal.",
            ));
        }
        validate_funding(&self.intent)?;
        // Stage two binds the exact source action amount rather than the
        // original Papertrade withdrawal amount. HyperCore CCTP leaves its
        // protocol-defined 0.1 USDC reserve in Core; that reserve was credited
        // in stage one and is not part of this stage's source debit.
        let stage_two_source = match &self.intent.route {
            FundingRoute::Core => {
                return Err(GoFinError::validation(
                    "A Core-only withdrawal has no onward funding continuation.",
                ));
            }
            FundingRoute::Hyperevm => self
                .intent
                .amount_usdc_atoms
                .checked_sub(100_000)
                .ok_or_else(|| {
                    GoFinError::validation(
                        "HyperEVM withdrawal is below its retained Core reserve.",
                    )
                })?,
            FundingRoute::Ethereum | FundingRoute::Arbitrum | FundingRoute::Solana => self
                .intent
                .amount_usdc_atoms
                .checked_sub(100_000)
                .ok_or_else(|| {
                    GoFinError::validation("CCTP withdrawal is below its retained Core reserve.")
                })?,
        };
        let stage_two_intent = FundingIntent {
            amount_usdc_atoms: stage_two_source,
            ..self.intent.clone()
        };
        // `maximum_source_fee_usdc_atoms` on the public continuation binding
        // bounds a separately charged Core fee. The exact principal is
        // already reduced by the route's retained 0.1 USDC reserve, so that
        // separate fee cap must not be subtracted from the destination floor
        // a second time during this structural check.
        let mut stage_two_bounds = self.bounds.clone();
        stage_two_bounds.maximum_source_fee_usdc_atoms = 0;
        validate_funding_bounds(&stage_two_intent, &stage_two_bounds)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SolanaFundingTransactionBinding {
    pub message_sha256: String,
    pub signer: String,
    pub fee_payer: String,
    pub recent_blockhash: String,
    pub last_valid_block_height: Option<u64>,
    pub maximum_network_fee_lamports: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FundingFinancialBounds {
    /// Exact source USDC amount charged, in the source network's native atoms.
    pub source_usdc_atoms: u64,
    /// Additional bounded source-side USDC fee. The action amount remains
    /// exact; this cap allows routes such as native Core spot sends that
    /// charge the enrolled sender separately.
    pub maximum_source_fee_usdc_atoms: u64,
    /// Minimum USDC atoms required at the destination after protocol fees.
    pub destination_min_usdc_atoms: u64,
    /// Maximum CCTP fee charged in USDC atoms.
    pub maximum_cctp_fee_usdc_atoms: u64,
    /// Maximum first-use proxy activation cost withheld from the USDC
    /// destination credit. It is included in the exact source amount.
    pub maximum_activation_fee_atoms: u64,
    /// Exact canonical native-asset identity for network gas, when nonzero.
    pub native_fee_asset: Option<String>,
    /// Finite maximum native gas atoms; absent is allowed only for routes with
    /// no separate source-network gas charge.
    pub maximum_native_fee_atoms: Option<String>,
    /// Maximum native balance retained as refundable source-account funding.
    /// It is bounded and accounted separately from actual network fees.
    #[serde(default)]
    pub maximum_refundable_native_funding_atoms: Option<String>,
    /// Total native source debit ceiling covering actual network fee plus
    /// refundable account funding. Present only when both are authorized.
    #[serde(default)]
    pub maximum_native_source_debit_atoms: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "status", deny_unknown_fields)]
pub enum FundingProgressStatus {
    Pending,
    Settled,
    Rejected,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FundingProgress {
    pub status: FundingProgressStatus,
    #[serde(default)]
    pub receipt: Option<FundingReceipt>,
    pub evidence: Value,
}

/// Broker-resolved identity for invalidating the Hyperliquid Core nonce of an
/// unresolved Papertrade Core-deposit attempt. It deliberately omits the
/// nonce, owner-selected transaction fields, and signed payload; those remain
/// in the protected funding plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoreDepositNonceRecoveryBinding {
    pub version: u32,
    pub child_operation_id: String,
    pub parent_operation_id: String,
    pub parent_submission: gofin_domain::Submission,
    pub owner: String,
    pub route_id: String,
    pub plan_fingerprint: String,
}

impl CoreDepositNonceRecoveryBinding {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.child_operation_id.trim().is_empty()
            || self.parent_operation_id.trim().is_empty()
            || self.child_operation_id == self.parent_operation_id
            || !valid_address(&self.owner)
            || self.route_id.trim().is_empty()
            || self.plan_fingerprint.len() != 66
            || !self.plan_fingerprint.starts_with("0x")
            || !self.plan_fingerprint[2..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.parent_submission.kind.trim().is_empty()
            || self.parent_submission.reference.trim().is_empty()
        {
            return Err(GoFinError::validation(
                "Core nonce recovery binding is incomplete or malformed.",
            ));
        }
        Ok(())
    }
}

/// Exact acknowledgement proof linking the protected Noop to its recovery
/// child and unresolved parent. The nonce itself remains broker-private.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoreDepositNonceRecoveryProof {
    pub version: u32,
    /// Logical child operation identity (`execution:<request_id>`), which is
    /// known before the engine creates the durable child record.
    pub child_operation_id: String,
    /// Actual durable OperationRecord UUID, resolved through the parent's
    /// recovery-child link after the child is created.
    pub child_record_id: String,
    pub child_submission: gofin_domain::Submission,
    pub parent_operation_id: String,
    pub parent_submission: gofin_domain::Submission,
    pub owner: String,
    pub route_id: String,
    pub plan_fingerprint: String,
    pub evidence_id: String,
}

impl CoreDepositNonceRecoveryProof {
    pub fn validate_against(&self, binding: &CoreDepositNonceRecoveryBinding) -> Result<()> {
        if self.version != 1
            || self.child_operation_id != binding.child_operation_id
            || self.child_record_id.trim().is_empty()
            || self.parent_operation_id != binding.parent_operation_id
            || self.parent_submission != binding.parent_submission
            || !self.owner.eq_ignore_ascii_case(&binding.owner)
            || self.route_id != binding.route_id
            || !self
                .plan_fingerprint
                .eq_ignore_ascii_case(&binding.plan_fingerprint)
            || self.child_submission.kind.trim().is_empty()
            || self.child_submission.reference.trim().is_empty()
            || self.evidence_id.trim().is_empty()
        {
            return Err(GoFinError::validation(
                "Core nonce recovery proof does not match the immutable parent and child identities.",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoreDepositNonceRecoveryStatus {
    Pending,
    Unknown,
    Rejected,
    Settled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoreDepositNonceRecoveryProgress {
    pub status: CoreDepositNonceRecoveryStatus,
    #[serde(default)]
    pub proof: Option<CoreDepositNonceRecoveryProof>,
    pub evidence: Value,
}

/// A protected account mapping. Credentials are deliberately absent: wallet
/// and session keys are resolved only by the broker runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PapertradeAccount {
    pub name: String,
    pub owner: String,
    pub signing_enabled: bool,
}

impl PapertradeAccount {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || !valid_address(&self.owner) {
            return Err(GoFinError::validation(
                "Papertrade account needs a name and valid owner address.",
            ));
        }
        Ok(())
    }
    pub fn trading_cash_resource(&self) -> ResourceId {
        ResourceId(format!(
            "papertrade:mainnet:{}:trading_cash",
            self.owner.to_ascii_lowercase()
        ))
    }
    pub fn metadata(&self) -> ConnectionMetadata {
        ConnectionMetadata {
            name: self.name.clone(),
            provider: "papertrade".into(),
            resource: self.trading_cash_resource(),
            role: Some("trading_account".into()),
            parent: None,
            network: Some("ethereum:999".into()),
            address: Some(self.owner.to_ascii_lowercase()),
            credential_ref: None,
            signing_enabled: self.signing_enabled,
            details: serde_json::json!({
                "owner": self.owner.to_ascii_lowercase(),
                "chain_id": CHAIN_ID,
                "exchange": EXCHANGE,
                "markets": ["BTC", "ETH"],
                "collateral": "USDC cash only"
            }),
        }
    }
}

pub fn paper_asset() -> AssetId {
    AssetId {
        network: "ethereum:999".into(),
        reference: PAPER_TOKEN.into(),
        decimals: PAPER_DECIMALS,
    }
}

/// Papertrade's USDC is an internal HyperCore cash ledger. It is not the
/// owner's wallet USDC and uses a distinct resource despite sharing the token.
pub fn trading_usdc_asset() -> AssetId {
    AssetId {
        network: "ethereum:999".into(),
        reference: "papertrade:internal-usdc".into(),
        decimals: 18,
    }
}

pub fn hypercore_usdc_asset() -> AssetId {
    AssetId {
        network: "hyperliquid:mainnet".into(),
        reference: "USDC".into(),
        decimals: USDC_DECIMALS,
    }
}

#[async_trait]
pub trait PapertradeRuntime: Send + Sync {
    fn attach_execution_state(
        &self,
        _state: std::sync::Arc<dyn gofin_domain::execution_state::ExecutionState>,
    ) -> Result<()> {
        Ok(())
    }
    async fn read(&self, request: ReadRequest) -> Result<Value>;
    async fn valuation(&self, request: &IntentRequest) -> Result<Option<gofin_domain::Valuation>>;
    /// Price the owner-paid native gas bound for the immutable direct session
    /// revocation transaction. Relayed registration has no owner fee and does
    /// not use this method.
    async fn session_revocation_valuation(
        &self,
        _revocation: &BoundSessionRevoke,
    ) -> Result<Option<gofin_domain::Valuation>> {
        Ok(None)
    }
    async fn prepare_intent(
        &self,
        request: IntentRequest,
        operation_id: &str,
    ) -> Result<BoundIntent>;
    /// Refresh mutable market, account, position, fee and nonce evidence while
    /// preserving the exact bound request. Implementations must not reallocate
    /// its nonce, expiry, intent ID, or session key.
    async fn revalidate_intent(
        &self,
        request: &IntentRequest,
        bound: Option<&BoundIntent>,
    ) -> Result<()>;
    async fn prepare_session_registration(
        &self,
        owner: &str,
        registration_id: &str,
        expiry: u64,
        operation_id: &str,
    ) -> Result<BoundIntent>;
    async fn prepare_session_revocation(
        &self,
        owner: &str,
        registration_id: &str,
        operation_id: &str,
    ) -> Result<BoundSessionRevoke>;
    async fn revalidate_session_configuration(
        &self,
        binding: &SessionConfigurationBinding,
    ) -> Result<()>;
    /// Called only after the adapter has journaled the exact `BoundIntent` ID.
    /// All signatures stay broker-side and never enter a public response.
    async fn dispatch(&self, intent: &BoundIntent) -> Result<Receipt>;
    async fn dispatch_session_registration(
        &self,
        binding: &SessionRegistrationBinding,
    ) -> Result<Receipt>;
    /// Resolve the protected full typed binding by stable registration ID for
    /// canonical receipt verification. It must never return private key bytes.
    async fn session_registration_intent(
        &self,
        binding: &SessionRegistrationBinding,
    ) -> Result<Option<BoundIntent>>;
    /// Activate protected credentials only after the adapter validates the
    /// exact canonical registration event and typed intent.
    async fn activate_session_registration(
        &self,
        binding: &SessionRegistrationBinding,
        proof: &crate::VerifiedPapertradeCall,
    ) -> Result<()>;
    async fn dispatch_session_revocation(
        &self,
        binding: &SessionRevocationBinding,
        journal: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Receipt>;
    /// Resolve the frozen exact transaction from the broker-protected journal.
    async fn session_revocation_transaction(
        &self,
        binding: &SessionRevocationBinding,
    ) -> Result<Option<BoundSessionRevoke>>;
    /// Revoke protected credentials only after exact canonical transaction and
    /// SessionKeyRevoked event verification.
    async fn deactivate_session_registration(
        &self,
        binding: &SessionRevocationBinding,
        proof: &crate::VerifiedSessionRevocation,
    ) -> Result<()>;
    async fn canonical_transaction(
        &self,
        transaction_hash: &str,
    ) -> Result<Option<crate::CanonicalTransaction>>;
    /// Return the dispatch block durably recorded before the relayed
    /// registration POST. A missing baseline means receipt discovery must
    /// remain unresolved rather than scanning an unbounded history.
    async fn session_registration_from_block(
        &self,
        _binding: &SessionRegistrationBinding,
    ) -> Result<Option<u64>> {
        Ok(None)
    }
    /// Prove terminal expiry of a relayed registration when the durable
    /// dispatch baseline is absent. Runtime proof is same-block and
    /// code-pinned; this method is read-only and never resubmits its signature.
    async fn session_registration_expiry_proof(
        &self,
        _binding: &SessionRegistrationBinding,
    ) -> Result<Option<SessionRegistrationExpiryProof>> {
        Ok(None)
    }
    /// Prove terminal expiry of an uncertain owner-signed Core withdrawal.
    /// The runtime must check the exact intent ID, unused nonce and reviewed
    /// implementation pins at one canonical post-deadline block.
    async fn core_withdrawal_expiry_proof(
        &self,
        _bound: &BoundIntent,
    ) -> Result<Option<CoreWithdrawalExpiryProof>> {
        Ok(None)
    }
    /// Prove terminal expiry for an uncertain Open/Close/Stake/Unstake/Claim
    /// intent. Cancellation and WithdrawToCore use their separate evidence
    /// paths; this callback is read-only and never resubmits the signature.
    async fn intent_expiry_proof(
        &self,
        _bound: &BoundIntent,
    ) -> Result<Option<PapertradeIntentExpiryProof>> {
        Ok(None)
    }
    /// Discover the exact submitted BatchExecutor transaction from its
    /// protocol intent ID using canonical CallExecuted logs, starting no
    /// earlier than the durable preview block. This closes the lost-response
    /// gap without inventing an HTTP history route.
    async fn canonical_intent_transaction(
        &self,
        intent_id: &str,
        from_block: u64,
    ) -> Result<Option<crate::CanonicalTransaction>>;
    /// Recover exact target terminal state from the authenticated cancel
    /// endpoint response or owner-scoped wallet SSE. This is separate from
    /// BatchExecutor reconciliation because the signed cancel intent ID is
    /// not the target intent ID.
    async fn cancel_status(
        &self,
        owner: &str,
        target_intent_id: &str,
        from_block: u64,
    ) -> Result<Option<CancelStatusProof>>;
    /// Locate a canonical keeper receipt for one owner-owned position. Unlike
    /// a user close, this transaction is not bound to a BatchExecutor intent.
    async fn liquidation_transaction(
        &self,
        owner: &str,
        position_id: &str,
        since_block: Option<u64>,
    ) -> Result<Option<crate::CanonicalTransaction>>;
    async fn funding_valuation(
        &self,
        intent: &FundingIntent,
    ) -> Result<Option<gofin_domain::Valuation>>;
    /// Value only the separately authorized stage-two onward route. The
    /// initial Papertrade withdrawal is valued independently as its Core
    /// credit stage and must not reserve these fee bounds prematurely.
    async fn funding_continuation_valuation(
        &self,
        binding: &FundingContinuationBinding,
    ) -> Result<Option<gofin_domain::Valuation>> {
        self.funding_valuation(&binding.intent).await
    }
    /// Construct and durably store the exact route plan and all leg identities.
    async fn prepare_funding(
        &self,
        intent: FundingIntent,
        operation_id: &str,
    ) -> Result<FundingBinding>;
    /// Dispatch only from the exact protected persisted plan referenced here.
    async fn dispatch_funding(
        &self,
        binding: &FundingBinding,
        outer_submission: &gofin_domain::Submission,
        validated: &gofin_domain::RevalidatedOperation,
        journal: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<FundingProgress>;
    /// Read/reconcile the protected plan without ever issuing a new write.
    async fn reconcile_funding(&self, binding: &FundingBinding) -> Result<FundingProgress>;
    /// Bind the onward leg of a previously settled Papertrade withdrawal.
    /// The caller supplies only the immutable route ID; owner, route, stage-one
    /// proof and onward bounds are resolved from the protected broker journal.
    async fn prepare_funding_continuation(
        &self,
        _route_id: &str,
        _operation_id: &str,
    ) -> Result<FundingContinuationBinding> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_CONTINUATION_UNAVAILABLE",
            "preflight",
            "This runtime cannot prepare a protected funding continuation.",
            "Keep the Papertrade withdrawal at its verified Core stage until its protected runtime supports onward settlement.",
        ))
    }
    async fn revalidate_funding_continuation(
        &self,
        _binding: &FundingContinuationBinding,
    ) -> Result<()> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_CONTINUATION_UNAVAILABLE",
            "revalidation",
            "This runtime cannot revalidate a protected funding continuation.",
            "Keep the Papertrade withdrawal at its verified Core stage until its protected runtime supports onward settlement.",
        ))
    }
    async fn dispatch_funding_continuation(
        &self,
        _binding: &FundingContinuationBinding,
        _outer_submission: &gofin_domain::Submission,
        _validated: &gofin_domain::RevalidatedOperation,
        _journal: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<FundingProgress> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_CONTINUATION_UNAVAILABLE",
            "dispatch",
            "This runtime cannot dispatch a protected funding continuation.",
            "Do not replay or infer completion of the onward route from the first-stage Core credit.",
        ))
    }
    async fn reconcile_funding_continuation(
        &self,
        _binding: &FundingContinuationBinding,
    ) -> Result<FundingProgress> {
        Err(GoFinError::new(
            "PAPERTRADE_FUNDING_CONTINUATION_UNAVAILABLE",
            "reconciliation",
            "This runtime cannot reconcile a protected funding continuation.",
            "Keep the onward route unresolved and reconcile it through its protected runtime.",
        ))
    }
    /// Resolve only the protected identity of a pending Core deposit's
    /// Hyperliquid nonce. Public callers provide the parent operation ID, not
    /// an owner or nonce; the runtime validates the parent route and exact
    /// source attempt before freezing a child binding.
    async fn prepare_funding_nonce_recovery(
        &self,
        _parent_operation_id: &str,
        _child_operation_id: &str,
    ) -> Result<CoreDepositNonceRecoveryBinding> {
        Err(GoFinError::new(
            "PAPERTRADE_NONCE_RECOVERY_UNAVAILABLE",
            "preflight",
            "This runtime cannot prepare a protected Core-deposit nonce recovery.",
            "Reconcile the original Papertrade funding submission through its protected runtime.",
        ))
    }
    async fn revalidate_funding_nonce_recovery(
        &self,
        _binding: &CoreDepositNonceRecoveryBinding,
    ) -> Result<()> {
        Err(GoFinError::new(
            "PAPERTRADE_NONCE_RECOVERY_UNAVAILABLE",
            "revalidation",
            "This runtime cannot revalidate a protected Core-deposit nonce recovery.",
            "Keep the original submission unresolved and use its protected runtime.",
        ))
    }
    async fn dispatch_funding_nonce_recovery(
        &self,
        _binding: &CoreDepositNonceRecoveryBinding,
        _outer_submission: &gofin_domain::Submission,
        _validated: &gofin_domain::RevalidatedOperation,
        _journal: std::sync::Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<CoreDepositNonceRecoveryProgress> {
        Err(GoFinError::new(
            "PAPERTRADE_NONCE_RECOVERY_UNAVAILABLE",
            "dispatch",
            "This runtime cannot dispatch a protected Core-deposit nonce recovery.",
            "Keep the original submission unresolved and use its protected runtime.",
        ))
    }
    async fn reconcile_funding_nonce_recovery(
        &self,
        _binding: &CoreDepositNonceRecoveryBinding,
        _child_submission: Option<&gofin_domain::Submission>,
    ) -> Result<CoreDepositNonceRecoveryProgress> {
        Err(GoFinError::new(
            "PAPERTRADE_NONCE_RECOVERY_UNAVAILABLE",
            "reconciliation",
            "This runtime cannot reconcile a protected Core-deposit nonce recovery.",
            "Keep the original submission unresolved and inspect its durable child record.",
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum SessionConfigurationBinding {
    Register {
        registration: SessionRegistrationBinding,
    },
    Revoke {
        revocation: SessionRevocationBinding,
    },
}

pub fn validate_funding(intent: &FundingIntent) -> Result<()> {
    if !valid_address(&intent.owner)
        || intent.amount_usdc_atoms < 10_000_000
        || intent.request_id.trim().is_empty()
    {
        return Err(GoFinError::validation(
            "Papertrade funding requires an exact owner, at least 10 USDC and a request ID.",
        ));
    }
    Ok(())
}

pub fn validate_funding_bounds(
    intent: &FundingIntent,
    bounds: &FundingFinancialBounds,
) -> Result<()> {
    if bounds.source_usdc_atoms != intent.amount_usdc_atoms
        || bounds
            .source_usdc_atoms
            .checked_add(bounds.maximum_source_fee_usdc_atoms)
            .is_none()
        || (matches!(intent.direction, FundingDirection::Deposit)
            && bounds.destination_min_usdc_atoms < 10_000_000)
        || bounds.destination_min_usdc_atoms > intent.amount_usdc_atoms
        || bounds.maximum_cctp_fee_usdc_atoms > intent.amount_usdc_atoms
        || bounds.maximum_activation_fee_atoms > intent.amount_usdc_atoms
        || bounds.destination_min_usdc_atoms
            != intent
                .amount_usdc_atoms
                .saturating_sub(if intent.direction == FundingDirection::Withdraw {
                    bounds.maximum_source_fee_usdc_atoms
                } else {
                    0
                })
                .saturating_sub(bounds.maximum_cctp_fee_usdc_atoms)
                .saturating_sub(bounds.maximum_activation_fee_atoms)
    {
        return Err(GoFinError::validation(
            "Papertrade funding bounds do not cover the exact source amount and a valid destination minimum.",
        ));
    }
    match (&bounds.native_fee_asset, &bounds.maximum_native_fee_atoms) {
        (None, None) => {}
        (Some(asset), Some(atoms))
            if !asset.trim().is_empty()
                && !atoms.is_empty()
                && atoms.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let parsed = atoms.parse::<u128>().map_err(|_| {
                GoFinError::validation(
                    "Papertrade native fee cap exceeds the supported exact range.",
                )
            })?;
            if parsed == 0 {
                return Err(GoFinError::validation(
                    "Papertrade native fee asset requires a positive finite cap.",
                ));
            }
        }
        _ => {
            return Err(GoFinError::validation(
                "Papertrade native fee asset and finite cap must be provided together.",
            ));
        }
    }
    match (
        bounds.maximum_refundable_native_funding_atoms.as_deref(),
        bounds.maximum_native_source_debit_atoms.as_deref(),
        bounds.native_fee_asset.as_deref(),
        bounds.maximum_native_fee_atoms.as_deref(),
    ) {
        (None, None, _, _) => {}
        (Some(refundable), Some(total), Some(_), Some(fee)) => {
            let refundable = refundable.parse::<u128>().map_err(|_| {
                GoFinError::validation("Papertrade refundable native funding cap is malformed.")
            })?;
            let total = total.parse::<u128>().map_err(|_| {
                GoFinError::validation("Papertrade native source debit cap is malformed.")
            })?;
            let fee = fee
                .parse::<u128>()
                .map_err(|_| GoFinError::validation("Papertrade native fee cap is malformed."))?;
            if refundable == 0
                || fee == 0
                || total == 0
                || fee.checked_add(refundable).is_none_or(|sum| sum > total)
                || intent.route != FundingRoute::Solana
                || intent.direction != FundingDirection::Deposit
            {
                return Err(GoFinError::validation(
                    "Papertrade native fee and refundable funding exceed their combined source debit bound.",
                ));
            }
        }
        _ => {
            return Err(GoFinError::validation(
                "Refundable native funding requires matching fee, asset and total debit bounds.",
            ));
        }
    }
    Ok(())
}

pub fn wad_from_decimal(value: &str, decimals: u8) -> Result<u128> {
    if decimals > 18 || value.is_empty() || value.starts_with('-') {
        return Err(GoFinError::validation(
            "Amount must be a nonnegative decimal with supported precision.",
        ));
    }
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let frac = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || frac.len() > decimals as usize
    {
        return Err(GoFinError::validation(
            "Amount has invalid decimal syntax or exceeds asset precision.",
        ));
    }
    let scale = 10u128
        .checked_pow(decimals as u32)
        .ok_or_else(|| GoFinError::validation("Amount precision overflow."))?;
    let whole = whole
        .parse::<u128>()
        .map_err(|_| GoFinError::validation("Amount is too large."))?;
    let fraction = if frac.is_empty() {
        0
    } else {
        frac.parse::<u128>()
            .map_err(|_| GoFinError::validation("Amount is invalid."))?
    };
    let fractional_scale = 10u128
        .checked_pow((decimals as usize - frac.len()) as u32)
        .ok_or_else(|| GoFinError::validation("Amount precision overflow."))?;
    whole
        .checked_mul(scale)
        .and_then(|x| x.checked_add(fraction.checked_mul(fractional_scale)?))
        .ok_or_else(|| GoFinError::validation("Amount is too large."))
}

/// Exact integer fee/PnL arithmetic using floor division as Solidity does.
pub fn mul_div_floor(a: u128, b: u128, denominator: u128) -> Result<u128> {
    if denominator == 0 {
        return Err(GoFinError::validation("Cannot divide by zero."));
    }
    use num_bigint::BigUint;
    use num_traits::ToPrimitive;
    ((BigUint::from(a) * BigUint::from(b)) / BigUint::from(denominator))
        .to_u128()
        .ok_or_else(|| GoFinError::validation("Exact protocol calculation overflow."))
}

fn validate_deadline(nonce: &str, deadline: u64, now: u64) -> Result<()> {
    if !valid_uint256(nonce)
        || nonce == "0"
        || deadline <= now
        || deadline - now > MAX_INTENT_SECONDS
    {
        return Err(GoFinError::validation(
            "Papertrade intent needs a nonzero nonce and a deadline within one hour.",
        ));
    }
    Ok(())
}
fn valid_uint256(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 78
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return false;
    }
    use num_traits::Num;
    num_bigint::BigUint::from_str_radix(value, 10).is_ok_and(|n| n.bits() <= 256)
}
fn valid_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_bytes32(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    const OWNER: &str = "0x1234567890123456789012345678901234567890";
    #[test]
    fn intent_request_bindings_keep_u128_quantities_exact_in_json() {
        let request = IntentRequest::Stake {
            owner: OWNER.into(),
            amount: 1_000_000_000_000_000_000_000,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["amount"], "1000000000000000000000");
        assert_eq!(
            serde_json::from_value::<IntentRequest>(encoded).unwrap(),
            request
        );
        // Read previously persisted integer-form bindings where possible.
        let legacy = serde_json::json!({"kind":"stake","owner":OWNER,"amount":7});
        assert!(matches!(
            serde_json::from_value::<IntentRequest>(legacy).unwrap(),
            IntentRequest::Stake { amount: 7, .. }
        ));
    }
    #[test]
    fn bound_open_intents_roundtrip_large_margin_as_decimal_strings() {
        for margin in [
            10_000_000_000_000_000_000u128,
            20_000_000_000_000_000_000u128,
        ] {
            let bound = BoundIntent {
                action: Some("openPosition".into()),
                typed: PapertradeTypedIntent::OpenPosition {
                    user: OWNER.into(),
                    instrument_id: 0,
                    is_long: true,
                    size: margin,
                    leverage: 100,
                    use_debt: false,
                    nonce: "9".into(),
                    deadline: 1_800_000_000,
                },
                intent_id: format!("0x{}", "11".repeat(32)),
                signer: OWNER.into(),
                registration_id: None,
                cancel_target_action: None,
            };
            let encoded = serde_json::to_value(&bound).unwrap();
            assert_eq!(encoded["typed"]["size"], margin.to_string());
            assert_eq!(
                serde_json::from_value::<BoundIntent>(encoded).unwrap(),
                bound
            );
        }
    }
    #[test]
    fn funding_bounds_bind_exact_gross_and_maximum_fee() {
        let intent = FundingIntent {
            direction: FundingDirection::Deposit,
            route: FundingRoute::Ethereum,
            owner: OWNER.into(),
            amount_usdc_atoms: 11_000_000,
            request_id: "op-1".into(),
        };
        let bounds = FundingFinancialBounds {
            source_usdc_atoms: 11_000_000,
            maximum_source_fee_usdc_atoms: 0,
            destination_min_usdc_atoms: 10_000_000,
            maximum_cctp_fee_usdc_atoms: 1_000_000,
            maximum_activation_fee_atoms: 0,
            native_fee_asset: Some("ethereum:1:native".into()),
            maximum_native_fee_atoms: Some("1000000000000000".into()),
            maximum_refundable_native_funding_atoms: None,
            maximum_native_source_debit_atoms: None,
        };
        assert!(validate_funding_bounds(&intent, &bounds).is_ok());
        let mut broader = bounds.clone();
        broader.destination_min_usdc_atoms = 9_999_999;
        assert!(validate_funding_bounds(&intent, &broader).is_err());
        let mut mismatched_activation = bounds.clone();
        mismatched_activation.maximum_activation_fee_atoms = 1;
        assert!(validate_funding_bounds(&intent, &mismatched_activation).is_err());
        let mut activation_bound = bounds.clone();
        activation_bound.maximum_activation_fee_atoms = 1_000_000;
        activation_bound.destination_min_usdc_atoms = 9_000_000;
        assert!(validate_funding_bounds(&intent, &activation_bound).is_err());
        let mut source_fee_bound = bounds.clone();
        source_fee_bound.maximum_source_fee_usdc_atoms = 1_000_000;
        assert!(validate_funding_bounds(&intent, &source_fee_bound).is_ok());
        source_fee_bound.source_usdc_atoms = u64::MAX;
        assert!(validate_funding_bounds(&intent, &source_fee_bound).is_err());
        let mut mismatched_source = bounds;
        mismatched_source.source_usdc_atoms -= 1;
        assert!(validate_funding_bounds(&intent, &mismatched_source).is_err());

        let withdrawal = FundingIntent {
            direction: FundingDirection::Withdraw,
            route: FundingRoute::Arbitrum,
            owner: OWNER.into(),
            amount_usdc_atoms: 10_000_000,
            request_id: "withdraw-1".into(),
        };
        let withdrawal_bounds = FundingFinancialBounds {
            source_usdc_atoms: 10_000_000,
            maximum_source_fee_usdc_atoms: 0,
            destination_min_usdc_atoms: 9_000_000,
            maximum_cctp_fee_usdc_atoms: 1_000_000,
            maximum_activation_fee_atoms: 0,
            native_fee_asset: None,
            maximum_native_fee_atoms: None,
            maximum_refundable_native_funding_atoms: None,
            maximum_native_source_debit_atoms: None,
        };
        assert!(validate_funding(&withdrawal).is_ok());
        assert!(validate_funding_bounds(&withdrawal, &withdrawal_bounds).is_ok());
        let hyperevm_withdrawal = FundingIntent {
            route: FundingRoute::Hyperevm,
            ..withdrawal.clone()
        };
        let hyperevm_withdrawal_bounds = FundingFinancialBounds {
            source_usdc_atoms: 10_000_000,
            maximum_source_fee_usdc_atoms: 100_000,
            destination_min_usdc_atoms: 9_900_000,
            maximum_cctp_fee_usdc_atoms: 0,
            maximum_activation_fee_atoms: 0,
            native_fee_asset: Some("hyperliquid:mainnet:HYPE".into()),
            maximum_native_fee_atoms: Some("100000".into()),
            maximum_refundable_native_funding_atoms: None,
            maximum_native_source_debit_atoms: None,
        };
        assert!(validate_funding_bounds(&hyperevm_withdrawal, &hyperevm_withdrawal_bounds).is_ok());

        let solana = FundingIntent {
            direction: FundingDirection::Deposit,
            route: FundingRoute::Solana,
            owner: OWNER.into(),
            amount_usdc_atoms: 20_000_000,
            request_id: "solana-1".into(),
        };
        let solana_bounds = FundingFinancialBounds {
            source_usdc_atoms: 20_000_000,
            maximum_source_fee_usdc_atoms: 0,
            destination_min_usdc_atoms: 19_000_000,
            maximum_cctp_fee_usdc_atoms: 1_000_000,
            maximum_activation_fee_atoms: 0,
            native_fee_asset: Some("solana:mainnet-beta:native".into()),
            maximum_native_fee_atoms: Some("500000".into()),
            maximum_refundable_native_funding_atoms: Some("4200000".into()),
            maximum_native_source_debit_atoms: Some("5000000".into()),
        };
        assert!(validate_funding_bounds(&solana, &solana_bounds).is_ok());
        let mut too_small_total = solana_bounds.clone();
        too_small_total.maximum_native_source_debit_atoms = Some("4699999".into());
        assert!(validate_funding_bounds(&solana, &too_small_total).is_err());
        let mut rent_on_evm = solana_bounds;
        rent_on_evm.native_fee_asset = Some("ethereum:1:native".into());
        rent_on_evm.maximum_refundable_native_funding_atoms = None;
        rent_on_evm.maximum_native_source_debit_atoms = None;
        assert!(validate_funding_bounds(&solana, &rent_on_evm).is_ok());
    }
    #[test]
    fn exact_decimal_scaling_never_uses_float() {
        assert_eq!(wad_from_decimal("0.000000000000000001", 18).unwrap(), 1);
        assert_eq!(wad_from_decimal("12.34", 6).unwrap(), 12_340_000);
        assert!(wad_from_decimal("0.0000001", 6).is_err());
        assert!(wad_from_decimal("1e3", 18).is_err());
    }
    #[test]
    fn intents_enforce_cash_only_and_bounded_expiry() {
        let open = Intent::Open {
            owner: OWNER.into(),
            instrument_id: 1,
            is_long: true,
            margin_wad: 1,
            leverage: 10,
            use_debt: false,
            nonce: "1".into(),
            deadline: 3_600,
        };
        assert!(open.validate(0).is_ok());
        let debt = Intent::Open {
            owner: OWNER.into(),
            instrument_id: 1,
            is_long: true,
            margin_wad: 1,
            leverage: 10,
            use_debt: true,
            nonce: "1".into(),
            deadline: 3_600,
        };
        assert!(debt.validate(0).is_err());
        let long = Intent::Claim {
            owner: OWNER.into(),
            nonce: "1".into(),
            deadline: 3_601,
        };
        assert!(long.validate(0).is_err());
    }
    #[test]
    fn protocol_math_uses_full_width_intermediate() {
        assert_eq!(mul_div_floor(u128::MAX, 10, 10).unwrap(), u128::MAX);
        assert!(mul_div_floor(1, 1, 0).is_err());
    }

    #[test]
    fn principal_asset_identity_and_account_cash_are_separate() {
        let paper = paper_asset();
        assert_eq!(paper.network, "ethereum:999");
        assert_eq!(paper.reference, PAPER_TOKEN);
        assert_eq!(paper.decimals, 18);
        let account = PapertradeAccount {
            name: "test".into(),
            owner: OWNER.into(),
            signing_enabled: true,
        };
        account.validate().unwrap();
        assert!(account.trading_cash_resource().0.contains("trading_cash"));
        assert_eq!(trading_usdc_asset().network, "ethereum:999");
        assert_eq!(trading_usdc_asset().decimals, 18);
        assert_eq!(hypercore_usdc_asset().decimals, 6);
        assert_ne!(paper.reference, trading_usdc_asset().reference);
    }

    #[test]
    fn session_identity_is_broker_owned_and_expiry_is_capped() {
        let registration = Intent::RegisterSession {
            owner: OWNER.into(),
            registration_id: "registration-1".into(),
            expiry: MAX_SESSION_SECONDS,
            nonce: "1".into(),
            deadline: 3_600,
        };
        assert!(registration.validate(0).is_ok());
        let too_long = Intent::RegisterSession {
            owner: OWNER.into(),
            registration_id: "registration-1".into(),
            expiry: MAX_SESSION_SECONDS + 1,
            nonce: "1".into(),
            deadline: 3_600,
        };
        assert!(too_long.validate(0).is_err());
    }

    #[test]
    fn public_session_registration_binding_omits_generated_key() {
        let key = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let bound = BoundIntent {
            typed: PapertradeTypedIntent::RegisterSessionKey {
                user: OWNER.into(),
                key: key.into(),
                expiry: 10_000,
                fee_address: "0x0000000000000000000000000000000000000000".into(),
                nonce: "7".into(),
                deadline: 1_000,
            },
            intent_id: format!("0x{}", "11".repeat(32)),
            signer: OWNER.into(),
            action: Some("registerSessionKey".into()),
            registration_id: Some("registration-1".into()),
            cancel_target_action: None,
        };
        let public =
            SessionRegistrationBinding::from_bound("registration-1".into(), &bound).unwrap();
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(!encoded.contains(key));
        assert!(encoded.contains("registration-1"));
    }

    #[test]
    fn session_registration_expiry_proof_requires_exact_pinned_post_deadline_state() {
        let bound = BoundIntent {
            typed: PapertradeTypedIntent::RegisterSessionKey {
                user: OWNER.into(),
                key: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                expiry: 10_000,
                fee_address: "0x0000000000000000000000000000000000000000".into(),
                nonce: "7".into(),
                deadline: 1_000,
            },
            intent_id: format!("0x{}", "11".repeat(32)),
            signer: OWNER.into(),
            action: Some("registerSessionKey".into()),
            registration_id: Some("registration-1".into()),
            cancel_target_action: None,
        };
        let registration =
            SessionRegistrationBinding::from_bound("registration-1".into(), &bound).unwrap();
        let proof = SessionRegistrationExpiryProof {
            chain_id: CHAIN_ID,
            owner: OWNER.into(),
            registration_id: "registration-1".into(),
            intent_id: bound.intent_id.clone(),
            nonce: "7".into(),
            deadline: 1_000,
            canonical_block_number: 123,
            canonical_block_hash: format!("0x{}", "22".repeat(32)),
            canonical_block_timestamp: 1_001,
            session_key_manager_proxy: SESSION_KEY_MANAGER.into(),
            session_key_manager_implementation: SESSION_KEY_MANAGER_IMPLEMENTATION.into(),
            session_key_manager_code_hash: SESSION_KEY_MANAGER_CODE_HASH.into(),
            nonce_unused: true,
            session_absent: true,
        };
        assert!(proof.validate_against(&registration).is_ok());

        let mut before_deadline = proof.clone();
        before_deadline.canonical_block_timestamp = 1_000;
        assert!(before_deadline.validate_against(&registration).is_err());
        let mut wrong_code = proof.clone();
        wrong_code.session_key_manager_code_hash = format!("0x{}", "33".repeat(32));
        assert!(wrong_code.validate_against(&registration).is_err());
        let mut consumed_nonce = proof;
        consumed_nonce.nonce_unused = false;
        assert!(consumed_nonce.validate_against(&registration).is_err());
    }
}
