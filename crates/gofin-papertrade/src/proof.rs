//! Canonical receipt and signed-call correlation for Papertrade's BatchExecutor.
//! Receipt bytes are supplied by a broker-owned canonical RPC observer; this
//! module independently checks the fixed contract, selector, typed signature,
//! intent identity, batch item, and action-specific protocol event.
use crate::{BATCH_EXECUTOR, EXCHANGE, PAPER_STAKING, PAPER_TOKEN, PapertradeTypedIntent};
use gofin_domain::{GoFinError, Result};
use gofin_evm::{
    alloy::primitives::{Address, Signature, U256},
    papertrade_signing,
};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

const EXECUTE_SELECTOR: &str = "251fb27d";
const CALL_EXECUTED_TOPIC: &str =
    "0x8a23ef1120f3e42e722a0c0a599a66d55ab2650ecc39716f57e625684e349cad";
const CALL_REJECTED_TOPIC: &str =
    "0x6002fcbae88fcbb8133c69c91df90bbc0b4464af94df2a56f8bb843f1cc5a193";
const CALL_SKIPPED_TOPIC: &str =
    "0xaaf3cbad9087f9956efae61847c185478efbe673d0f8591fdaf1eb97a897aa7c";
const PINNED_BATCH_IMPLEMENTATION: &str = "0xe0bc9cc7b067060aafccbaa3e250700c9cdd7262";
const PINNED_BATCH_CODE_HASH: &str =
    "0xe926c659aca61daaaf9e68267817be4dae5152f11320a6ba970549dfdd15f62a";
const POSITION_OPENED_TOPIC: &str =
    "0x88451ea1189f73b554f0bf39c09868560ed149b3d788fcc933ff66ba926827f3";
const POSITION_CLOSED_TOPIC: &str =
    "0x87902ed2451849f465fc4d36a01d7f5c67b53af12f63ee85d2814a2d582f89bf";
const LIQUIDATED_TOPIC: &str = "0x0c859bc5db091ba3b9dc10c00f8dfc78b1f0b7e2566d107253ae67c069ae5de8";
// PositionsClosed(address,uint256,uint256,uint256,uint256): owner indexed;
// word0 is cash actually credited to the internal ledger, word1 is resulting
// balance, words2/3 are fees paid to staking and development.
// Its unlabelled layout is accepted only under the pinned Exchange code below.
const EXCHANGE_CASH_CREDITED_TOPIC: &str =
    "0xa32e4800a59f03b131f5fea90951bdf00d8d505ad98f48ece29d8620a0bf09b0";
const ERC20_TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const PAPER_TOKENOMICS: &str = "0x75174549dbbd44497d31a47e2e2d7406408d63d9";
const PAPER_LIQUIDATION_MINT_TOPIC: &str =
    "0x3050fba19ada85812db26b240c13c72cabec711cbf18c7c6b3554d511e2951ee";
const PAPER_CLOSE_MINT_TOPIC: &str =
    "0x620c26bf4591919d64df736a6afdfe2981d15eb343c50644b49456c6facb3ab2";
const PINNED_EXCHANGE_IMPLEMENTATION: &str = "0x0f3febfc94876b0c895f7acecdc8f5c863b549ee";
const PINNED_EXCHANGE_CODE_HASH: &str =
    "0x9d20287df886415035d53344a0dc8a1044949308cc3283ee0fe7ea5f3ea28d92";
const PINNED_STAKING_IMPLEMENTATION: &str = "0xe6e3519455170913c5c3bfe541dd918a2d0cbb15";
const PINNED_STAKING_CODE_HASH: &str =
    "0x54573c497e5109e353cf5330343323915e0c0f26e1b410fd3cb2cd8d1460f154";
const STAKED_TOPIC: &str = "0x9e71bc8eea02a63969f509818f2dafb9254532904319f9dbda79b67bd34a5f3d";
const UNSTAKED_TOPIC: &str = "0x0f5bb82176feb1b5e747e28471aa92156a04d9f3ab9f45f28e2d704232b93f75";
const CLAIMED_TOPIC: &str = "0xd8138f8a3f377c5259ca548e70e4c2de94f129f5a11036a15b69513cba2b426a";
const CORE_WITHDRAWAL_TOPIC: &str =
    "0x95d1a93901f24eeaa294588802f66fa008f9f3741060c9f79e5dab4709de5e75";
const SWEPT_TOPIC: &str = "0xc36b5179cb9c303b200074996eab2b3473eac370fdd7eba3bec636fe35109696";
const SESSION_REGISTERED_TOPIC: &str =
    "0x3cec0566535d67d6c92668517af0bf8a66cdc91d622dbb830fcc08e1bcf5d1df";
const SESSION_REVOKED_TOPIC: &str =
    "0x744157ccffbd293a2e8644928cd7d23d650f869b88f72d7bfea8041b76ca6bec";
const SESSION_KEY_MANAGER: &str = crate::SESSION_KEY_MANAGER;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    pub log_index: String,
}

/// A point-in-time transaction plus receipt fetched by the protected broker's
/// RPC observer. `canonical` is true only after block-hash recheck against the
/// canonical chain; callers cannot set this from public operation input.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalTransaction {
    pub transaction_hash: String,
    /// Sender returned by eth_getTransactionByHash and checked against the
    /// frozen transaction for owner-paid configuration writes.
    #[serde(default)]
    pub transaction_from: Option<String>,
    pub transaction_to: String,
    pub transaction_input: String,
    pub block_hash: String,
    pub block_number: u64,
    pub canonical: bool,
    pub success: bool,
    /// Canonical receipt gas fields, normalized to unsigned decimal strings.
    #[serde(default)]
    pub gas_used: Option<String>,
    #[serde(default)]
    pub effective_gas_price: Option<String>,
    pub logs: Vec<CanonicalLog>,
    /// EIP-1967 implementation identity and deployed runtime code hash read
    /// at this transaction's canonical block. Required for close cash-event
    /// decoding because that event's four-word layout is code-specific.
    #[serde(default)]
    pub exchange_implementation: Option<String>,
    #[serde(default)]
    pub exchange_code_hash: Option<String>,
    /// EIP-1967 PaperStaking implementation identity at the receipt block.
    /// Required to prove the deployed zero-reward return path, which emits no
    /// Claimed event.
    #[serde(default)]
    pub staking_implementation: Option<String>,
    #[serde(default)]
    pub staking_code_hash: Option<String>,
    /// BatchExecutor implementation identity for reviewed rejection/skip
    /// event semantics.
    #[serde(default)]
    pub batch_implementation: Option<String>,
    #[serde(default)]
    pub batch_code_hash: Option<String>,
}
impl std::fmt::Debug for CanonicalTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanonicalTransaction")
            .field("transaction_hash", &self.transaction_hash)
            .field("transaction_from", &self.transaction_from)
            .field("transaction_to", &self.transaction_to)
            .field("transaction_input", &"<redacted>")
            .field("block_hash", &self.block_hash)
            .field("block_number", &self.block_number)
            .field("canonical", &self.canonical)
            .field("success", &self.success)
            .field("gas_used", &self.gas_used)
            .field("effective_gas_price", &self.effective_gas_price)
            .field("logs", &format!("<{} redacted logs>", self.logs.len()))
            .field("exchange_implementation", &self.exchange_implementation)
            .field("exchange_code_hash", &self.exchange_code_hash)
            .field("staking_implementation", &self.staking_implementation)
            .field("staking_code_hash", &self.staking_code_hash)
            .field("batch_implementation", &self.batch_implementation)
            .field("batch_code_hash", &self.batch_code_hash)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ProtocolSettlementEvent {
    PositionOpened {
        position_id: String,
        owner: String,
        instrument_id: u32,
        is_long: bool,
        use_debt: bool,
        entry_price: String,
        margin: String,
        leverage: u32,
    },
    PositionClosed {
        position_id: String,
        owner: String,
        raw_data: String,
    },
    Liquidated {
        position_id: String,
        owner: String,
        raw_data: String,
    },
    PaperMinted {
        owner: String,
        position_id: Option<String>,
        liquidation: bool,
        amount: String,
    },
    PositionsClosedCashSettlement {
        owner: String,
        amount_wad: String,
        resulting_balance_wad: String,
    },
    Staked {
        owner: String,
        amount: String,
    },
    Unstaked {
        owner: String,
        amount: String,
    },
    Claimed {
        owner: String,
        amount: String,
    },
    CoreWithdrawal {
        owner: String,
        removed_wad: String,
        received_usdc_atoms: String,
    },
    Swept {
        owner: String,
        amount_wad: String,
    },
    SessionKeyRegistered {
        owner: String,
        key: String,
        expiry: String,
    },
    SessionKeyRevoked {
        owner: String,
        key: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedPapertradeCall {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub batch_id: String,
    pub batch_item_index: u64,
    pub intent_id: String,
    pub action: String,
    pub owner: String,
    pub target: String,
    pub selector: String,
    /// ABI-decoded CallExecuted return bytes. Used only for protocol methods
    /// whose documented zero result intentionally omits an event.
    pub call_return_data: Option<String>,
    pub event_ids: Vec<String>,
    pub events: Vec<ProtocolSettlementEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedSessionRevocation {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub owner: String,
    pub key: String,
    pub event_id: String,
    pub actual_gas_fee_atoms: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedSessionRevocationFailure {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub owner: String,
    pub key: String,
    pub actual_gas_fee_atoms: String,
}

/// Canonical keeper-liquidation evidence, independent of a user close intent.
/// `paper_minted` is correlated to the exact position id and immediate token
/// Transfer inside that position's receipt-log segment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedLiquidation {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub owner: String,
    pub position_id: String,
    pub liquidation_raw_data: String,
    pub paper_minted: String,
    pub event_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum RejectedCallKind {
    Rejected { reason_hex: String },
    Skipped { group_key: String },
    Reverted { return_data: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedRejectedCall {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub batch_id: String,
    pub batch_item_index: u64,
    pub intent_id: String,
    pub kind: RejectedCallKind,
    pub event_id: String,
}

/// Verify an externally executed liquidation for a previously opened exact
/// position. Keeper liquidations are direct Exchange calls and therefore do
/// not carry a user BatchExecutor intent id.
pub fn verify_liquidation_receipt(
    tx: &CanonicalTransaction,
    expected_owner: &str,
    expected_position_id: &str,
) -> Result<VerifiedLiquidation> {
    b256(&tx.transaction_hash)?;
    b256(&tx.block_hash)?;
    let owner = canon_address(address(expected_owner)?);
    let position_id = U256::from_str(expected_position_id)
        .map_err(|_| invalid("Liquidation position id is malformed."))?
        .to_string();
    if !tx.canonical
        || !tx.success
        || tx.block_number == 0
        || canon_address(address(&tx.transaction_to)?) != EXCHANGE
        || tx.exchange_implementation.as_deref() != Some(PINNED_EXCHANGE_IMPLEMENTATION)
        || tx.exchange_code_hash.as_deref() != Some(PINNED_EXCHANGE_CODE_HASH)
    {
        return Err(invalid(
            "Liquidation is not a successful canonical direct Exchange call under the reviewed implementation pin.",
        ));
    }
    let mut previous_log_index = None;
    for log in &tx.logs {
        let index = U256::from_str(&log.log_index)
            .or_else(|_| U256::from_str_radix(log.log_index.trim_start_matches("0x"), 16))
            .map_err(|_| invalid("Receipt log index is malformed."))?;
        if previous_log_index.is_some_and(|previous| index <= previous) {
            return Err(invalid(
                "Receipt logs are not in strictly increasing canonical order.",
            ));
        }
        previous_log_index = Some(index);
    }
    let target_marker = tx
        .logs
        .iter()
        .position(|log| {
            log.address.eq_ignore_ascii_case(EXCHANGE)
                && log
                    .topics
                    .first()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(LIQUIDATED_TOPIC))
                && log.topics.get(1).is_some_and(|topic| {
                    U256::from_be_bytes(b256(topic).unwrap_or([0; 32])).to_string() == position_id
                })
                && log.topics.get(2).is_some_and(|topic| {
                    word_address_topic(topic).is_ok_and(|found| canon_address(found) == owner)
                })
        })
        .ok_or_else(|| {
            invalid("Canonical receipt has no liquidation event for the exact owner and position.")
        })?;
    let segment_start = tx.logs[..target_marker]
        .iter()
        .rposition(|log| {
            log.address.eq_ignore_ascii_case(EXCHANGE)
                && log
                    .topics
                    .first()
                    .is_some_and(|topic| topic.eq_ignore_ascii_case(LIQUIDATED_TOPIC))
        })
        .map_or(0, |index| index + 1);
    let intent = PapertradeTypedIntent::Close {
        user: owner.clone(),
        position_ids: vec![position_id.clone()],
        nonce: "1".to_owned(),
        deadline: 1,
    };
    let (_, events) = action_events(&intent, &tx.logs[segment_start..=target_marker])?;
    let mut liquidations = events.iter().filter_map(|event| match event {
        ProtocolSettlementEvent::Liquidated {
            position_id: id,
            owner: event_owner,
            raw_data,
        } if id == &position_id && event_owner.eq_ignore_ascii_case(&owner) => Some(raw_data),
        _ => None,
    });
    let liquidation_raw_data = liquidations
        .next()
        .cloned()
        .ok_or_else(|| invalid("The exact liquidation event did not decode."))?;
    if liquidations.next().is_some() {
        return Err(invalid(
            "Canonical receipt contains duplicate liquidation events for the exact position in one segment.",
        ));
    }
    let mut mints = events.iter().filter_map(|event| match event {
        ProtocolSettlementEvent::PaperMinted {
            owner: event_owner,
            position_id: Some(id),
            liquidation: true,
            amount,
        } if id == &position_id && event_owner.eq_ignore_ascii_case(&owner) => Some(amount),
        _ => None,
    });
    let paper_minted = mints
        .next()
        .cloned()
        .ok_or_else(|| invalid("The exact position has no correlated liquidation PAPER mint."))?;
    if mints.next().is_some() {
        return Err(invalid(
            "Canonical receipt contains duplicate liquidation mints for the exact position in one segment.",
        ));
    }
    let event_ids = tx.logs[segment_start..=target_marker]
        .iter()
        .filter(|log| {
            log.topics.first().is_some_and(|topic| {
                topic.eq_ignore_ascii_case(LIQUIDATED_TOPIC)
                    || topic.eq_ignore_ascii_case(PAPER_LIQUIDATION_MINT_TOPIC)
                    || topic.eq_ignore_ascii_case(ERC20_TRANSFER_TOPIC)
            })
        })
        .map(|log| log.log_index.clone())
        .collect();
    Ok(VerifiedLiquidation {
        transaction_hash: tx.transaction_hash.to_ascii_lowercase(),
        block_hash: tx.block_hash.to_ascii_lowercase(),
        block_number: tx.block_number,
        owner,
        position_id,
        liquidation_raw_data,
        paper_minted,
        event_ids,
    })
}

fn invalid(message: &str) -> GoFinError {
    GoFinError::new(
        "PAPERTRADE_RECEIPT_INVALID",
        "reconciliation",
        message,
        "Keep the operation unresolved and inspect its original canonical transaction.",
    )
}
fn hex_bytes(value: &str) -> Result<Vec<u8>> {
    let value = value
        .strip_prefix("0x")
        .ok_or_else(|| invalid("Receipt data is not 0x-prefixed hex."))?;
    hex::decode(value).map_err(|_| invalid("Receipt data contains malformed hex."))
}
fn call_executed_return(data: &[u8]) -> Result<Vec<u8>> {
    if !bool_word(data, 0)? || U256::from_be_slice(&word(data, 32)?) != U256::from(64) {
        return Err(invalid(
            "CallExecuted has an invalid success/return-data ABI layout.",
        ));
    }
    let length: usize = U256::from_be_slice(&word(data, 64)?)
        .try_into()
        .map_err(|_| invalid("CallExecuted return data length overflows."))?;
    let padded_len = length
        .checked_add(31)
        .map(|value| value / 32 * 32)
        .ok_or_else(|| invalid("CallExecuted return data length overflows."))?;
    let expected_len = 96usize
        .checked_add(padded_len)
        .ok_or_else(|| invalid("CallExecuted return data length overflows."))?;
    if data.len() != expected_len {
        return Err(invalid(
            "CallExecuted return data is truncated or has trailing bytes.",
        ));
    }
    Ok(data[96..96 + length].to_vec())
}
fn word(bytes: &[u8], offset: usize) -> Result<[u8; 32]> {
    bytes
        .get(offset..offset + 32)
        .ok_or_else(|| invalid("EVM calldata or receipt data is truncated."))?
        .try_into()
        .map_err(|_| invalid("EVM word has an invalid width."))
}
fn word_u256(bytes: &[u8], offset: usize) -> Result<U256> {
    Ok(U256::from_be_bytes(word(bytes, offset)?))
}
fn word_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    word_u256(bytes, offset)?
        .try_into()
        .map_err(|_| invalid("ABI integer exceeds its declared width."))
}
fn word_address(bytes: &[u8], offset: usize) -> Result<Address> {
    let w = word(bytes, offset)?;
    if w[..12].iter().any(|x| *x != 0) {
        return Err(invalid("ABI address has nonzero high padding."));
    }
    Ok(Address::from_slice(&w[12..]))
}
fn address(value: &str) -> Result<Address> {
    Address::from_str(value).map_err(|_| invalid("Papertrade address is malformed."))
}
fn canon_address(value: Address) -> String {
    format!("{value:#x}").to_ascii_lowercase()
}
fn b256(value: &str) -> Result<[u8; 32]> {
    let bytes = hex_bytes(value)?;
    bytes
        .try_into()
        .map_err(|_| invalid("Expected an exact bytes32 value."))
}
fn selector(call: &[u8]) -> Result<String> {
    if call.len() < 4 {
        return Err(invalid("Batch item calldata has no selector."));
    }
    Ok(hex::encode(&call[..4]))
}
fn decimal_word(bytes: &[u8], offset: usize) -> Result<String> {
    Ok(word_u256(bytes, offset)?.to_string())
}
fn bool_word(bytes: &[u8], offset: usize) -> Result<bool> {
    match word_u64(bytes, offset)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid("ABI bool is not zero or one.")),
    }
}
fn dynamic_array(bytes: &[u8], offset_word: usize, max: usize) -> Result<Vec<U256>> {
    let rel: usize = word_u256(bytes, offset_word)?
        .try_into()
        .map_err(|_| invalid("ABI array offset overflows."))?;
    let start = rel;
    let len: usize = word_u256(bytes, start)?
        .try_into()
        .map_err(|_| invalid("ABI array length overflows."))?;
    if len == 0 || len > max {
        return Err(invalid(
            "ABI position array length is outside the supported range.",
        ));
    }
    (0..len)
        .map(|i| word_u256(bytes, start + 32 + i * 32))
        .collect()
}
fn bytes_dynamic(bytes: &[u8], base: usize, offset_word: usize) -> Result<Vec<u8>> {
    let rel: usize = word_u256(bytes, base + offset_word)?
        .try_into()
        .map_err(|_| invalid("ABI dynamic offset overflows."))?;
    let start = base
        .checked_add(rel)
        .ok_or_else(|| invalid("ABI dynamic offset overflows."))?;
    let len: usize = word_u256(bytes, start)?
        .try_into()
        .map_err(|_| invalid("ABI dynamic byte length overflows."))?;
    bytes
        .get(start + 32..start + 32 + len)
        .map(|s| s.to_vec())
        .ok_or_else(|| invalid("ABI dynamic bytes are truncated."))
}

#[derive(Debug)]
struct BatchItem {
    target: Address,
    data: Vec<u8>,
    item_id: [u8; 32],
}
fn decode_batch(input: &str) -> Result<Vec<BatchItem>> {
    let bytes = hex_bytes(input)?;
    if bytes.get(..4) != Some(&hex::decode(EXECUTE_SELECTOR).expect("fixed selector")[..]) {
        return Err(invalid("Transaction is not BatchExecutor.execute."));
    }
    let array_rel: usize = word_u256(&bytes, 4)?
        .try_into()
        .map_err(|_| invalid("Batch array offset overflows."))?;
    let array = 4usize
        .checked_add(array_rel)
        .ok_or_else(|| invalid("Batch array offset overflows."))?;
    let len: usize = word_u256(&bytes, array)?
        .try_into()
        .map_err(|_| invalid("Batch array length overflows."))?;
    if len == 0 || len > 64 {
        return Err(invalid("Batch contains an unsupported number of calls."));
    }
    let offsets_start = array + 32;
    let mut calls = Vec::with_capacity(len);
    for index in 0..len {
        let rel: usize = word_u256(&bytes, offsets_start + index * 32)?
            .try_into()
            .map_err(|_| invalid("Batch tuple offset overflows."))?;
        let base = offsets_start
            .checked_add(rel)
            .ok_or_else(|| invalid("Batch tuple offset overflows."))?;
        let target = word_address(&bytes, base)?;
        let calldata = bytes_dynamic(&bytes, base, 32)?;
        let item_id = word(&bytes, base + 128)?;
        calls.push(BatchItem {
            target,
            data: calldata,
            item_id,
        });
    }
    Ok(calls)
}

fn intent_evm(intent: &PapertradeTypedIntent) -> Result<papertrade_signing::PapertradeTypedIntent> {
    use papertrade_signing::PapertradeTypedIntent as E;
    Ok(match intent {
        PapertradeTypedIntent::RegisterSessionKey {
            user,
            key,
            expiry,
            fee_address,
            nonce,
            deadline,
        } => E::RegisterSessionKey {
            user: user.clone(),
            key: key.clone(),
            expiry: *expiry,
            fee_address: fee_address.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::OpenPosition {
            user,
            instrument_id,
            is_long,
            size,
            leverage,
            use_debt,
            nonce,
            deadline,
        } => E::OpenPosition {
            user: user.clone(),
            instrument_id: *instrument_id,
            is_long: *is_long,
            size: *size,
            leverage: *leverage,
            use_debt: *use_debt,
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Close {
            user,
            position_ids,
            nonce,
            deadline,
        } => E::Close {
            user: user.clone(),
            position_ids: position_ids.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::WithdrawToCore {
            user,
            amount,
            nonce,
            deadline,
        } => E::WithdrawToCore {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Stake {
            user,
            amount,
            nonce,
            deadline,
        } => E::Stake {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Unstake {
            user,
            amount,
            nonce,
            deadline,
        } => E::Unstake {
            user: user.clone(),
            amount: amount.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::Claim {
            user,
            nonce,
            deadline,
        } => E::Claim {
            user: user.clone(),
            nonce: nonce.clone(),
            deadline: *deadline,
        },
        PapertradeTypedIntent::CancelIntent {
            user,
            intent_id,
            deadline,
        } => E::CancelIntent {
            user: user.clone(),
            intent_id: intent_id.clone(),
            deadline: deadline.clone(),
        },
    })
}

fn recover_intent(intent: &PapertradeTypedIntent, args: &[u8], fields: usize) -> Result<String> {
    let (v, r, s) = (
        word_u64(args, fields * 32)?,
        word(args, (fields + 1) * 32)?,
        word(args, (fields + 2) * 32)?,
    );
    let parity = match v {
        27 => false,
        28 => true,
        _ => return Err(invalid("Papertrade signature v is not canonical 27 or 28.")),
    };
    let evm_intent = intent_evm(intent)?;
    let deadline = match intent {
        PapertradeTypedIntent::CancelIntent { deadline, .. } => deadline
            .parse::<u64>()
            .map_err(|_| invalid("Cancellation deadline exceeds uint64."))?,
        _ => match intent {
            PapertradeTypedIntent::RegisterSessionKey { deadline, .. }
            | PapertradeTypedIntent::OpenPosition { deadline, .. }
            | PapertradeTypedIntent::Close { deadline, .. }
            | PapertradeTypedIntent::WithdrawToCore { deadline, .. }
            | PapertradeTypedIntent::Stake { deadline, .. }
            | PapertradeTypedIntent::Unstake { deadline, .. }
            | PapertradeTypedIntent::Claim { deadline, .. } => *deadline,
            PapertradeTypedIntent::CancelIntent { .. } => unreachable!(),
        },
    };
    let now = match intent {
        PapertradeTypedIntent::RegisterSessionKey { expiry, .. } => {
            deadline.saturating_sub(1).min(expiry.saturating_sub(1))
        }
        _ => deadline.saturating_sub(1),
    };
    let digest = evm_intent
        .digest(now)
        .map_err(|_| invalid("Could not reproduce the fixed Papertrade typed-data digest."))?;
    let signature = Signature::new(U256::from_be_bytes(r), U256::from_be_bytes(s), parity);
    let signer = signature
        .recover_address_from_prehash(&digest)
        .map_err(|_| invalid("Papertrade signature does not recover."))?;
    let calculated = papertrade_signing::intent_id(digest, signer);
    Ok(format!("{calculated:#x}").to_ascii_lowercase())
}

fn verify_signed_call(
    intent: &PapertradeTypedIntent,
    target: Address,
    call: &[u8],
    expected_id: &str,
) -> Result<String> {
    let selector = selector(call)?;
    let args = call
        .get(4..)
        .ok_or_else(|| invalid("Papertrade function calldata is truncated."))?;
    let owner = address(match intent {
        PapertradeTypedIntent::RegisterSessionKey { user, .. }
        | PapertradeTypedIntent::OpenPosition { user, .. }
        | PapertradeTypedIntent::Close { user, .. }
        | PapertradeTypedIntent::WithdrawToCore { user, .. }
        | PapertradeTypedIntent::Stake { user, .. }
        | PapertradeTypedIntent::Unstake { user, .. }
        | PapertradeTypedIntent::Claim { user, .. }
        | PapertradeTypedIntent::CancelIntent { user, .. } => user,
    })?;
    let (expected_target, expected_selector, signature_index) = match intent {
        PapertradeTypedIntent::RegisterSessionKey { .. } => (SESSION_KEY_MANAGER, "5f76a1d6", 6),
        PapertradeTypedIntent::OpenPosition { .. } => (EXCHANGE, "ec6783f0", 8),
        PapertradeTypedIntent::Close { .. } => (EXCHANGE, "2792e706", 4),
        PapertradeTypedIntent::WithdrawToCore { .. } => (EXCHANGE, "8d09f23f", 4),
        PapertradeTypedIntent::Stake { .. } => (PAPER_STAKING, "83115825", 4),
        PapertradeTypedIntent::Unstake { .. } => (PAPER_STAKING, "fa2871b5", 4),
        PapertradeTypedIntent::Claim { .. } => (PAPER_STAKING, "9c96d4a1", 3),
        _ => {
            return Err(invalid(
                "This Papertrade typed operation has no verified by-signature selector yet.",
            ));
        }
    };
    if canon_address(target) != expected_target
        || selector != expected_selector
        || word_address(args, 0)? != owner
    {
        return Err(invalid(
            "Batch call target, selector, or owner differs from the bound typed intent.",
        ));
    }
    match intent {
        PapertradeTypedIntent::RegisterSessionKey {
            key,
            expiry,
            fee_address,
            nonce,
            deadline,
            ..
        } => {
            if word_address(args, 32)? != address(key)?
                || word_u64(args, 64)? != *expiry
                || word_address(args, 96)? != address(fee_address)?
                || decimal_word(args, 128)? != *nonce
                || word_u64(args, 160)? != *deadline
            {
                return Err(invalid(
                    "registerSessionKeyBySig calldata differs from its signed fields.",
                ));
            }
        }
        PapertradeTypedIntent::OpenPosition {
            instrument_id,
            is_long,
            size,
            leverage,
            use_debt,
            nonce,
            deadline,
            ..
        } => {
            if word_u64(args, 32)? != *instrument_id as u64
                || bool_word(args, 64)? != *is_long
                || word_u256(args, 96)? != U256::from(*size)
                || word_u64(args, 128)? != *leverage as u64
                || bool_word(args, 160)? != *use_debt
                || decimal_word(args, 192)? != *nonce
                || word_u64(args, 224)? != *deadline
            {
                return Err(invalid(
                    "openPositionBySig calldata differs from its signed typed fields.",
                ));
            }
        }
        PapertradeTypedIntent::Close {
            position_ids,
            nonce,
            deadline,
            ..
        } => {
            let got = dynamic_array(args, 32, 12)?;
            let want = position_ids
                .iter()
                .map(|x| U256::from_str(x).map_err(|_| invalid("Close position ID is malformed.")))
                .collect::<Result<Vec<_>>>()?;
            if got != want || decimal_word(args, 64)? != *nonce || word_u64(args, 96)? != *deadline
            {
                return Err(invalid(
                    "closeBySig calldata differs from the exact approved position IDs or signed fields.",
                ));
            }
        }
        PapertradeTypedIntent::WithdrawToCore {
            amount,
            nonce,
            deadline,
            ..
        }
        | PapertradeTypedIntent::Stake {
            amount,
            nonce,
            deadline,
            ..
        }
        | PapertradeTypedIntent::Unstake {
            amount,
            nonce,
            deadline,
            ..
        } => {
            if decimal_word(args, 32)? != *amount
                || decimal_word(args, 64)? != *nonce
                || word_u64(args, 96)? != *deadline
            {
                return Err(invalid(
                    "amount-by-signature calldata differs from its signed fields.",
                ));
            }
        }
        PapertradeTypedIntent::Claim {
            nonce, deadline, ..
        } => {
            if decimal_word(args, 32)? != *nonce || word_u64(args, 64)? != *deadline {
                return Err(invalid(
                    "claimBySig calldata differs from its signed fields.",
                ));
            }
        }
        _ => unreachable!(),
    }
    let recovered_id = recover_intent(intent, args, signature_index)?;
    if recovered_id != expected_id.to_ascii_lowercase() {
        return Err(invalid(
            "Recovered Papertrade signature intent ID differs from its durable submission identity.",
        ));
    }
    Ok(selector)
}

fn word_address_topic(value: &str) -> Result<Address> {
    let bytes = b256(value)?;
    if bytes[..12].iter().any(|b| *b != 0) {
        return Err(invalid("Indexed address has nonzero high padding."));
    }
    Ok(Address::from_slice(&bytes[12..]))
}
fn action_events(
    intent: &PapertradeTypedIntent,
    logs: &[CanonicalLog],
) -> Result<(Vec<String>, Vec<ProtocolSettlementEvent>)> {
    let mut ids = Vec::new();
    let mut events = Vec::new();
    let owner = match intent {
        PapertradeTypedIntent::RegisterSessionKey { user, .. }
        | PapertradeTypedIntent::OpenPosition { user, .. }
        | PapertradeTypedIntent::Close { user, .. }
        | PapertradeTypedIntent::WithdrawToCore { user, .. }
        | PapertradeTypedIntent::Stake { user, .. }
        | PapertradeTypedIntent::Unstake { user, .. }
        | PapertradeTypedIntent::Claim { user, .. }
        | PapertradeTypedIntent::CancelIntent { user, .. } => address(user)?,
    };
    for (log_position, log) in logs.iter().enumerate() {
        let topics = log
            .topics
            .iter()
            .map(|t| b256(t))
            .collect::<Result<Vec<_>>>()?;
        let addr = address(&log.address)?;
        let data = hex_bytes(&log.data)?;
        if topics.is_empty() {
            continue;
        }
        let topic = format!("0x{}", hex::encode(topics[0]));
        match (intent, topic.as_str()) {
            (
                PapertradeTypedIntent::RegisterSessionKey { key, expiry, .. },
                SESSION_REGISTERED_TOPIC,
            ) if addr == address(SESSION_KEY_MANAGER)? => {
                if topics.len() != 3
                    || word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner
                    || word_address_topic(&format!("0x{}", hex::encode(topics[2])))?
                        != address(key)?
                    || decimal_word(&data, 0)? != expiry.to_string()
                {
                    return Err(invalid(
                        "SessionKeyRegistered event differs from the exact owner, broker key, or expiry.",
                    ));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::SessionKeyRegistered {
                    owner: canon_address(owner),
                    key: key.to_ascii_lowercase(),
                    expiry: expiry.to_string(),
                });
            }
            (
                PapertradeTypedIntent::OpenPosition {
                    instrument_id,
                    is_long,
                    size,
                    leverage,
                    use_debt,
                    ..
                },
                POSITION_OPENED_TOPIC,
            ) if addr == address(EXCHANGE)? => {
                if topics.len() != 3
                    || word_address_topic(&format!("0x{}", hex::encode(topics[2])))? != owner
                    || word_u64(&data, 0)? != *instrument_id as u64
                    || bool_word(&data, 32)? != *is_long
                    || bool_word(&data, 64)? != *use_debt
                    || word_u64(&data, 160)? != *leverage as u64
                    || word_u256(&data, 128)? != U256::from(*size)
                {
                    return Err(invalid(
                        "PositionOpened event does not match the approved owner, instrument, direction, collateral mode, margin, or leverage.",
                    ));
                }
                let position_id = U256::from_be_bytes(topics[1]).to_string();
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::PositionOpened {
                    position_id,
                    owner: canon_address(owner),
                    instrument_id: *instrument_id,
                    is_long: *is_long,
                    use_debt: *use_debt,
                    entry_price: decimal_word(&data, 96)?,
                    margin: size.to_string(),
                    leverage: *leverage,
                });
            }
            (PapertradeTypedIntent::Close { position_ids, .. }, POSITION_CLOSED_TOPIC)
                if addr == address(EXCHANGE)? =>
            {
                if topics.len() != 3
                    || word_address_topic(&format!("0x{}", hex::encode(topics[2])))? != owner
                {
                    return Err(invalid("PositionClosed event has a different owner."));
                }
                let id = U256::from_be_bytes(topics[1]).to_string();
                if !position_ids.contains(&id) {
                    return Err(invalid(
                        "PositionClosed event names a position outside the exact close request.",
                    ));
                }
                if data.len() != 160 {
                    return Err(invalid(
                        "PositionClosed event does not match the reviewed five-word ABI layout.",
                    ));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::PositionClosed {
                    position_id: id,
                    owner: canon_address(owner),
                    raw_data: log.data.clone(),
                });
            }
            (PapertradeTypedIntent::Close { position_ids, .. }, LIQUIDATED_TOPIC)
                if addr == address(EXCHANGE)? =>
            {
                if topics.len() != 3
                    || data.len() != 160
                    || word_address_topic(&format!("0x{}", hex::encode(topics[2])))? != owner
                    || bool_word(&data, 64)?
                {
                    return Err(invalid(
                        "Liquidated event differs from the cash-only owner or reviewed ABI layout.",
                    ));
                }
                let id = U256::from_be_bytes(topics[1]).to_string();
                if !position_ids.contains(&id) {
                    return Err(invalid(
                        "Liquidated event names a position outside the exact close request.",
                    ));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::Liquidated {
                    position_id: id,
                    owner: canon_address(owner),
                    raw_data: log.data.clone(),
                });
            }
            (PapertradeTypedIntent::Close { .. }, PAPER_LIQUIDATION_MINT_TOPIC)
            | (PapertradeTypedIntent::Close { .. }, PAPER_CLOSE_MINT_TOPIC)
                if addr == address(PAPER_TOKENOMICS)? =>
            {
                let liquidation = topic == PAPER_LIQUIDATION_MINT_TOPIC;
                if topics.len() != if liquidation { 3 } else { 2 } || data.len() != 192 {
                    return Err(invalid(
                        "PAPER mint event has an unexpected owner, position, or data layout.",
                    ));
                }
                if word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner {
                    return Err(invalid("PAPER mint event has a different owner."));
                }
                if liquidation && !bool_word(&data, 0)? {
                    return Err(invalid(
                        "Liquidation mint event does not mark liquidation issuance.",
                    ));
                }
                let position_id = if liquidation {
                    Some(U256::from_be_bytes(topics[2]).to_string())
                } else {
                    None
                };
                let amount = decimal_word(&data, 160)?;
                let previous = log_position
                    .checked_sub(1)
                    .and_then(|index| logs.get(index));
                let Some(transfer) = previous else {
                    return Err(invalid("PAPER mint event has no preceding Transfer log."));
                };
                let transfer_topics = transfer
                    .topics
                    .iter()
                    .map(|value| b256(value))
                    .collect::<Result<Vec<_>>>()?;
                if canon_address(address(&transfer.address)?)
                    != canon_address(address(PAPER_TOKEN)?)
                    || transfer_topics.len() != 3
                    || format!("0x{}", hex::encode(transfer_topics[0])) != ERC20_TRANSFER_TOPIC
                    || word_address_topic(&format!("0x{}", hex::encode(transfer_topics[1])))?
                        != Address::ZERO
                    || word_address_topic(&format!("0x{}", hex::encode(transfer_topics[2])))?
                        != owner
                    || decimal_word(&hex_bytes(&transfer.data)?, 0)? != amount
                {
                    return Err(invalid(
                        "PAPER mint event is not correlated to the immediately preceding exact zero-to-owner token transfer.",
                    ));
                }
                ids.push(transfer.log_index.clone());
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::PaperMinted {
                    owner: canon_address(owner),
                    position_id,
                    liquidation,
                    amount,
                });
            }
            (PapertradeTypedIntent::Close { .. }, EXCHANGE_CASH_CREDITED_TOPIC)
                if addr == address(EXCHANGE)? =>
            {
                if topics.len() != 2
                    || data.len() != 128
                    || word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner
                {
                    return Err(invalid(
                        "Canonical close cash event has an unexpected owner or four-word layout.",
                    ));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::PositionsClosedCashSettlement {
                    owner: canon_address(owner),
                    amount_wad: decimal_word(&data, 0)?,
                    resulting_balance_wad: decimal_word(&data, 32)?,
                });
            }
            (PapertradeTypedIntent::Stake { amount, .. }, STAKED_TOPIC)
                if addr == address(PAPER_STAKING)? =>
            {
                let got = decimal_word(&data, 0)?;
                if topics.len() != 2
                    || word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner
                    || &got != amount
                {
                    return Err(invalid(
                        "Staked event does not match exact owner and amount.",
                    ));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::Staked {
                    owner: canon_address(owner),
                    amount: got,
                });
            }
            (PapertradeTypedIntent::Unstake { amount, .. }, UNSTAKED_TOPIC)
                if addr == address(PAPER_STAKING)? =>
            {
                let got = decimal_word(&data, 0)?;
                if topics.len() != 2
                    || word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner
                    || &got != amount
                {
                    return Err(invalid(
                        "Unstaked event does not match exact owner and amount.",
                    ));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::Unstaked {
                    owner: canon_address(owner),
                    amount: got,
                });
            }
            (PapertradeTypedIntent::Claim { .. }, CLAIMED_TOPIC)
                if addr == address(PAPER_STAKING)? =>
            {
                let amount = decimal_word(&data, 0)?;
                if topics.len() != 2
                    || word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner
                {
                    return Err(invalid("Claimed event has a different owner."));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::Claimed {
                    owner: canon_address(owner),
                    amount,
                });
            }
            (PapertradeTypedIntent::WithdrawToCore { amount, .. }, CORE_WITHDRAWAL_TOPIC)
                if addr == address(EXCHANGE)? =>
            {
                let received_core_8dp = decimal_word(&data, 32)?
                    .parse::<u128>()
                    .map_err(|_| invalid("CoreWithdrawal Core amount is malformed."))?;
                let expected_usdc_6dp = amount
                    .parse::<u128>()
                    .map_err(|_| invalid("CoreWithdrawal intent amount is malformed."))?;
                let expected_core_8dp = expected_usdc_6dp
                    .checked_mul(100)
                    .ok_or_else(|| invalid("CoreWithdrawal intent amount overflowed."))?;
                if topics.len() != 2
                    || word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner
                    || received_core_8dp != expected_core_8dp
                {
                    return Err(invalid(
                        "CoreWithdrawal event does not match exact owner and 6dp USDC amount after 8dp Core conversion.",
                    ));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::CoreWithdrawal {
                    owner: canon_address(owner),
                    removed_wad: decimal_word(&data, 0)?,
                    received_usdc_atoms: decimal_word(&data, 32)?,
                });
            }
            (PapertradeTypedIntent::WithdrawToCore { .. }, SWEPT_TOPIC)
                if addr == address(EXCHANGE)? =>
            {
                if topics.len() != 2
                    || word_address_topic(&format!("0x{}", hex::encode(topics[1])))? != owner
                {
                    return Err(invalid("Swept event has a different owner."));
                }
                ids.push(log.log_index.clone());
                events.push(ProtocolSettlementEvent::Swept {
                    owner: canon_address(owner),
                    amount_wad: decimal_word(&data, 0)?,
                });
            }
            _ => {}
        }
    }
    match intent {
        PapertradeTypedIntent::RegisterSessionKey { .. } if events.iter().filter(|e| matches!(e, ProtocolSettlementEvent::SessionKeyRegistered { .. })).count() != 1 => return Err(invalid("Canonical receipt does not contain exactly one matching SessionKeyRegistered event.")),
        PapertradeTypedIntent::OpenPosition { .. } if events.iter().filter(|e|matches!(e,ProtocolSettlementEvent::PositionOpened{..})).count()!=1 => return Err(invalid("Canonical receipt does not contain exactly one matching PositionOpened event.")),
        PapertradeTypedIntent::Close { position_ids,.. } if position_ids.iter().any(|id| !events.iter().any(|e|matches!(e,ProtocolSettlementEvent::PositionClosed{position_id,..}|ProtocolSettlementEvent::Liquidated{position_id,..} if position_id==id))) => return Err(invalid("Canonical receipt does not contain a matching full-close or liquidation event for every exact position ID.")),
        PapertradeTypedIntent::Stake { .. } if events.iter().filter(|e|matches!(e,ProtocolSettlementEvent::Staked{..})).count()!=1 => return Err(invalid("Canonical receipt does not contain exactly one matching Staked event.")),
        PapertradeTypedIntent::Unstake { .. } if events.iter().filter(|e|matches!(e,ProtocolSettlementEvent::Unstaked{..})).count()!=1 => return Err(invalid("Canonical receipt does not contain exactly one matching Unstaked event.")),
        PapertradeTypedIntent::WithdrawToCore { .. } if !events.iter().any(|e|matches!(e,ProtocolSettlementEvent::CoreWithdrawal{..})) => return Err(invalid("Canonical receipt does not contain a matching CoreWithdrawal event.")),
        _ => {}
    }
    if matches!(intent, PapertradeTypedIntent::Close { .. }) {
        let normal_ids = events
            .iter()
            .filter_map(|event| match event {
                ProtocolSettlementEvent::PositionClosed { position_id, .. } => Some(position_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let liquidation_ids = events
            .iter()
            .filter_map(|event| match event {
                ProtocolSettlementEvent::Liquidated { position_id, .. } => Some(position_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let cash_events = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ProtocolSettlementEvent::PositionsClosedCashSettlement { .. }
                )
            })
            .count();
        if (!normal_ids.is_empty() && cash_events != 1)
            || (normal_ids.is_empty() && cash_events != 0)
        {
            return Err(invalid(
                "PositionsClosed cash settlement must be present exactly for ordinary full closes, not liquidation-only closes.",
            ));
        }
        for position_id in liquidation_ids {
            if !events.iter().any(|event| matches!(event, ProtocolSettlementEvent::PaperMinted { position_id: Some(minted_position_id), liquidation: true, .. } if minted_position_id == position_id)) {
                return Err(invalid("Liquidated position does not have an exact position-correlated PAPER mint proof."));
            }
        }
    }
    Ok((ids, events))
}

pub fn verify_canonical_call(
    tx: &CanonicalTransaction,
    intent: &PapertradeTypedIntent,
    expected_intent_id: &str,
) -> Result<VerifiedPapertradeCall> {
    b256(&tx.transaction_hash)?;
    b256(&tx.block_hash)?;
    if !tx.canonical
        || !tx.success
        || tx.block_number == 0
        || canon_address(address(&tx.transaction_to)?) != BATCH_EXECUTOR
    {
        return Err(invalid(
            "Transaction is not a successful canonical BatchExecutor receipt.",
        ));
    }
    if matches!(intent, PapertradeTypedIntent::Close { .. })
        && (tx.exchange_implementation.as_deref() != Some(PINNED_EXCHANGE_IMPLEMENTATION)
            || tx.exchange_code_hash.as_deref() != Some(PINNED_EXCHANGE_CODE_HASH))
    {
        return Err(invalid(
            "Exchange implementation does not match the reviewed close-settlement code pin.",
        ));
    }
    let mut previous_log_index = None;
    for log in &tx.logs {
        let index = U256::from_str(&log.log_index)
            .or_else(|_| U256::from_str_radix(log.log_index.trim_start_matches("0x"), 16))
            .map_err(|_| invalid("Receipt log index is malformed."))?;
        if previous_log_index.is_some_and(|previous| index <= previous) {
            return Err(invalid(
                "Receipt logs are not in strictly increasing canonical order.",
            ));
        }
        previous_log_index = Some(index);
    }
    let items = decode_batch(&tx.transaction_input)?;
    let mut matched = Vec::new();
    for log in &tx.logs {
        if canon_address(address(&log.address)?) != BATCH_EXECUTOR
            || log
                .topics
                .first()
                .is_none_or(|t| !t.eq_ignore_ascii_case(CALL_EXECUTED_TOPIC))
        {
            continue;
        }
        if log.topics.len() != 4 {
            return Err(invalid(
                "CallExecuted event has an unexpected indexed-field layout.",
            ));
        }
        let index: usize = U256::from_be_bytes(b256(&log.topics[2])?)
            .try_into()
            .map_err(|_| invalid("CallExecuted batch index overflows."))?;
        let item = items
            .get(index)
            .ok_or_else(|| invalid("CallExecuted index exceeds the transaction batch."))?;
        if b256(&log.topics[3])? != item.item_id {
            return Err(invalid(
                "CallExecuted item identity differs from the encoded batch item.",
            ));
        }
        let data = hex_bytes(&log.data)?;
        if !bool_word(&data, 0)? {
            continue;
        }
        let wanted = b256(expected_intent_id)?;
        if item.item_id == wanted {
            matched.push((index, item, log, b256(&log.topics[1])?));
        }
    }
    if matched.len() != 1 {
        return Err(invalid(
            "Canonical receipt must contain exactly one successful CallExecuted record for the durable intent ID.",
        ));
    }
    let (index, item, matched_call_log, batch_id) = matched[0];
    let selector = verify_signed_call(intent, item.target, &item.data, expected_intent_id)?;
    let call_position = tx
        .logs
        .iter()
        .position(|log| std::ptr::eq(log, matched_call_log))
        .ok_or_else(|| invalid("Matched batch marker is missing from canonical receipt logs."))?;
    let segment_start = tx.logs[..call_position]
        .iter()
        .rposition(|log| {
            address(&log.address).is_ok_and(|address| canon_address(address) == BATCH_EXECUTOR)
        })
        .map_or(0, |position| position + 1);
    let (event_ids, events) = action_events(intent, &tx.logs[segment_start..call_position])?;
    let call_return_data = call_executed_return(&hex_bytes(&matched_call_log.data)?)?;
    if matches!(intent, PapertradeTypedIntent::Claim { .. }) {
        let claims = events
            .iter()
            .filter_map(|event| match event {
                ProtocolSettlementEvent::Claimed { amount, .. } => Some(amount),
                _ => None,
            })
            .collect::<Vec<_>>();
        if claims.len() > 1 {
            return Err(invalid(
                "Canonical receipt contains duplicate Claimed events for one intent.",
            ));
        }
        if let Some(amount) = claims.first() {
            if call_return_data.len() != 32
                || U256::from_be_slice(&call_return_data).to_string() != **amount
            {
                return Err(invalid(
                    "Claimed event amount differs from the exact successful call return value.",
                ));
            }
        } else if call_return_data.len() != 32
            || U256::from_be_slice(&call_return_data) != U256::ZERO
            || tx.staking_implementation.as_deref() != Some(PINNED_STAKING_IMPLEMENTATION)
            || tx.staking_code_hash.as_deref() != Some(PINNED_STAKING_CODE_HASH)
        {
            return Err(invalid(
                "A zero-reward claim without Claimed event requires the pinned PaperStaking implementation and an exact zero return.",
            ));
        }
    }
    let owner = intent_evm(intent)?.user().to_ascii_lowercase();
    let action = match intent {
        PapertradeTypedIntent::OpenPosition { .. } => "openPosition",
        PapertradeTypedIntent::Close { .. } => "closePositions",
        PapertradeTypedIntent::WithdrawToCore { .. } => "withdrawToCore",
        PapertradeTypedIntent::Stake { .. } => "paperStake",
        PapertradeTypedIntent::Unstake { .. } => "paperUnstake",
        PapertradeTypedIntent::Claim { .. } => "paperClaim",
        PapertradeTypedIntent::RegisterSessionKey { .. } => "registerSessionKey",
        PapertradeTypedIntent::CancelIntent { .. } => "cancelIntent",
    };
    Ok(VerifiedPapertradeCall {
        transaction_hash: tx.transaction_hash.to_ascii_lowercase(),
        block_hash: tx.block_hash.to_ascii_lowercase(),
        block_number: tx.block_number,
        batch_id: format!("0x{}", hex::encode(batch_id)),
        batch_item_index: index as u64,
        intent_id: expected_intent_id.to_ascii_lowercase(),
        action: action.into(),
        owner,
        target: canon_address(item.target),
        selector,
        call_return_data: Some(format!("0x{}", hex::encode(call_return_data))),
        event_ids,
        events,
    })
}

/// Verify a terminal no-fill outcome from a canonical batch receipt. This
/// accepts only a false CallExecuted, CallRejected, or CallSkipped record
/// tied to the exact encoded signed item and the pinned BatchExecutor code.
pub fn verify_rejected_call(
    tx: &CanonicalTransaction,
    intent: &PapertradeTypedIntent,
    expected_intent_id: &str,
) -> Result<Option<VerifiedRejectedCall>> {
    b256(&tx.transaction_hash)?;
    b256(&tx.block_hash)?;
    if !tx.canonical
        || !tx.success
        || tx.block_number == 0
        || canon_address(address(&tx.transaction_to)?) != BATCH_EXECUTOR
    {
        return Err(invalid(
            "Transaction is not a successful canonical BatchExecutor receipt.",
        ));
    }
    if tx.batch_implementation.as_deref() != Some(PINNED_BATCH_IMPLEMENTATION)
        || tx.batch_code_hash.as_deref() != Some(PINNED_BATCH_CODE_HASH)
    {
        return Err(invalid(
            "BatchExecutor implementation does not match the reviewed failure-event code pin.",
        ));
    }
    let mut previous_log_index = None;
    for log in &tx.logs {
        let index = U256::from_str(&log.log_index)
            .or_else(|_| U256::from_str_radix(log.log_index.trim_start_matches("0x"), 16))
            .map_err(|_| invalid("Receipt log index is malformed."))?;
        if previous_log_index.is_some_and(|previous| index <= previous) {
            return Err(invalid(
                "Receipt logs are not in strictly increasing canonical order.",
            ));
        }
        previous_log_index = Some(index);
    }
    let wanted = b256(expected_intent_id)?;
    let items = decode_batch(&tx.transaction_input)?;
    let (item_index, item) = items
        .iter()
        .enumerate()
        .find(|(_, item)| item.item_id == wanted)
        .ok_or_else(|| {
            invalid("The canonical batch does not contain the exact durable intent item.")
        })?;
    verify_signed_call(intent, item.target, &item.data, expected_intent_id)?;
    let mut outcomes = Vec::new();
    for log in &tx.logs {
        if canon_address(address(&log.address)?) != BATCH_EXECUTOR
            || log.topics.len() != 4
            || b256(&log.topics[2])? != U256::from(item_index).to_be_bytes::<32>()
            || b256(&log.topics[3])? != wanted
        {
            continue;
        }
        let topic = log.topics[0].to_ascii_lowercase();
        let data = hex_bytes(&log.data)?;
        let kind = if topic == CALL_EXECUTED_TOPIC {
            if bool_word(&data, 0)? {
                continue;
            }
            if U256::from_be_slice(&word(&data, 32)?) != U256::from(64) {
                return Err(invalid(
                    "Failed CallExecuted event has an invalid return-data offset.",
                ));
            }
            let length: usize = U256::from_be_slice(&word(&data, 64)?)
                .try_into()
                .map_err(|_| invalid("Failed CallExecuted return-data length overflows."))?;
            let padded = length
                .checked_add(31)
                .ok_or_else(|| invalid("Call return-data length overflows."))?
                / 32
                * 32;
            if data.len() != 96 + padded || data[96 + length..].iter().any(|byte| *byte != 0) {
                return Err(invalid("Failed CallExecuted return-data ABI is malformed."));
            }
            RejectedCallKind::Reverted {
                return_data: format!("0x{}", hex::encode(&data[96..96 + length])),
            }
        } else if topic == CALL_REJECTED_TOPIC {
            if U256::from_be_slice(&word(&data, 0)?) != U256::from(32) {
                return Err(invalid("CallRejected reason has an invalid ABI offset."));
            }
            let length: usize = U256::from_be_slice(&word(&data, 32)?)
                .try_into()
                .map_err(|_| invalid("CallRejected reason length overflows."))?;
            let padded = length
                .checked_add(31)
                .ok_or_else(|| invalid("CallRejected reason length overflows."))?
                / 32
                * 32;
            if data.len() != 64 + padded || data[64 + length..].iter().any(|byte| *byte != 0) {
                return Err(invalid("CallRejected reason ABI is malformed."));
            }
            RejectedCallKind::Rejected {
                reason_hex: format!("0x{}", hex::encode(&data[64..64 + length])),
            }
        } else if topic == CALL_SKIPPED_TOPIC {
            if data.len() != 32 {
                return Err(invalid("CallSkipped group key has an invalid ABI layout."));
            }
            RejectedCallKind::Skipped {
                group_key: format!("0x{}", hex::encode(data)),
            }
        } else {
            continue;
        };
        outcomes.push((log, b256(&log.topics[1])?, kind));
    }
    if outcomes.len() > 1 {
        return Err(invalid(
            "Canonical receipt contains multiple terminal outcomes for the exact intent item.",
        ));
    }
    let Some((log, batch_id, kind)) = outcomes.pop() else {
        return Ok(None);
    };
    Ok(Some(VerifiedRejectedCall {
        transaction_hash: tx.transaction_hash.to_ascii_lowercase(),
        block_hash: tx.block_hash.to_ascii_lowercase(),
        block_number: tx.block_number,
        batch_id: format!("0x{}", hex::encode(batch_id)),
        batch_item_index: item_index as u64,
        intent_id: expected_intent_id.to_ascii_lowercase(),
        kind,
        event_id: log.log_index.clone(),
    }))
}

pub fn verify_session_revocation(
    tx: &CanonicalTransaction,
    binding: &crate::BoundSessionRevoke,
) -> Result<VerifiedSessionRevocation> {
    validate_session_revocation_transaction(tx, binding, true)?;
    let owner = address(&binding.owner)?;
    let key = address(&binding.session_key)?;
    let matches = session_revocation_event_indices(tx, owner, key)?;
    if matches.len() != 1 {
        return Err(invalid(
            "Canonical receipt does not contain exactly one matching SessionKeyRevoked event.",
        ));
    }
    Ok(VerifiedSessionRevocation {
        transaction_hash: tx.transaction_hash.to_ascii_lowercase(),
        block_hash: tx.block_hash.to_ascii_lowercase(),
        block_number: tx.block_number,
        owner: canon_address(owner),
        key: canon_address(key),
        event_id: matches[0].clone(),
        actual_gas_fee_atoms: session_revocation_actual_fee(tx, binding)?,
    })
}

/// Proves an exact frozen owner revocation transaction reverted canonically.
/// This proves a terminal failed write and its actual gas debit; it deliberately
/// does not deactivate the session key.
pub fn verify_session_revocation_failure(
    tx: &CanonicalTransaction,
    binding: &crate::BoundSessionRevoke,
) -> Result<VerifiedSessionRevocationFailure> {
    validate_session_revocation_transaction(tx, binding, false)?;
    let owner = address(&binding.owner)?;
    let key = address(&binding.session_key)?;
    if !session_revocation_event_indices(tx, owner, key)?.is_empty() {
        return Err(invalid(
            "Reverted revocation receipt unexpectedly contains a matching revocation event.",
        ));
    }
    Ok(VerifiedSessionRevocationFailure {
        transaction_hash: tx.transaction_hash.to_ascii_lowercase(),
        block_hash: tx.block_hash.to_ascii_lowercase(),
        block_number: tx.block_number,
        owner: canon_address(owner),
        key: canon_address(key),
        actual_gas_fee_atoms: session_revocation_actual_fee(tx, binding)?,
    })
}

fn validate_session_revocation_transaction(
    tx: &CanonicalTransaction,
    binding: &crate::BoundSessionRevoke,
    success: bool,
) -> Result<()> {
    b256(&tx.transaction_hash)?;
    b256(&tx.block_hash)?;
    binding
        .validate()
        .map_err(|_| invalid("Frozen session revocation request is invalid."))?;
    let owner = canon_address(address(&binding.owner)?);
    if !tx.canonical
        || tx.success != success
        || tx.block_number == 0
        || canon_address(address(&tx.transaction_to)?) != SESSION_KEY_MANAGER
        || tx
            .transaction_from
            .as_deref()
            .map(address)
            .transpose()?
            .map(canon_address)
            .as_deref()
            != Some(owner.as_str())
        || canon_address(address(&binding.transaction.sender)?) != owner
    {
        return Err(invalid(
            "Transaction is not the exact canonical owner-signed SessionKeyManager receipt.",
        ));
    }
    let call = hex_bytes(&tx.transaction_input)?;
    let expected = hex_bytes(&binding.transaction.calldata)?;
    if call != expected || call.len() != 36 || call[..4] != [0x84, 0xf4, 0xfc, 0x6a] {
        return Err(invalid(
            "Canonical revokeSessionKey transaction differs from its frozen owner transaction.",
        ));
    }
    Ok(())
}

fn session_revocation_event_indices(
    tx: &CanonicalTransaction,
    owner: Address,
    key: Address,
) -> Result<Vec<String>> {
    let mut matches = Vec::new();
    for log in &tx.logs {
        if canon_address(address(&log.address)?) != SESSION_KEY_MANAGER
            || log.topics.len() != 3
            || !log.topics[0].eq_ignore_ascii_case(SESSION_REVOKED_TOPIC)
        {
            continue;
        }
        if word_address_topic(&log.topics[1])? == owner
            && word_address_topic(&log.topics[2])? == key
        {
            matches.push(log.log_index.clone());
        }
    }
    Ok(matches)
}

/// Computes receipt gas from the broker's canonical receipt fields and checks
/// it against the immutable EIP-1559 maximum cost before exposing it to ledger
/// accounting.
pub fn session_revocation_actual_fee(
    tx: &CanonicalTransaction,
    binding: &crate::BoundSessionRevoke,
) -> Result<String> {
    let gas_used = gofin_domain::Quantity::new(
        tx.gas_used
            .as_deref()
            .ok_or_else(|| invalid("Canonical revocation receipt omitted gasUsed."))?,
    )?;
    let gas_price = gofin_domain::Quantity::new(
        tx.effective_gas_price
            .as_deref()
            .ok_or_else(|| invalid("Canonical revocation receipt omitted effectiveGasPrice."))?,
    )?;
    let actual =
        gofin_domain::Quantity::new((gas_used.integer() * gas_price.integer()).to_string())?;
    let maximum = gofin_domain::Quantity::new(binding.transaction.maximum_cost()?.to_string())?;
    if actual > maximum {
        return Err(invalid(
            "Canonical revocation gas exceeds its immutable approved maximum cost.",
        ));
    }
    Ok(actual.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_evm::LocalEnvelopeSigner;
    use papertrade_signing::PapertradeTypedIntent as EvmIntent;

    fn push_word(out: &mut Vec<u8>, value: U256) {
        out.extend_from_slice(&value.to_be_bytes::<32>());
    }

    fn session_revoke_fixture(success: bool) -> (CanonicalTransaction, crate::BoundSessionRevoke) {
        let owner = "0x1111111111111111111111111111111111111111";
        let key = "0x2222222222222222222222222222222222222222";
        let calldata = format!("0x84f4fc6a{}", "00".repeat(12) + &key[2..]);
        let binding = crate::BoundSessionRevoke {
            owner: owner.into(),
            session_key: key.into(),
            registration_id: "registration-fixture".into(),
            submission_id: "submission-fixture".into(),
            transaction: gofin_evm::FrozenTransaction {
                chain_id: crate::CHAIN_ID,
                sender: owner.into(),
                to: crate::SESSION_KEY_MANAGER.into(),
                value: gofin_domain::Quantity::zero(),
                calldata: calldata.clone(),
                nonce: 7,
                gas: 50_000,
                max_fee_per_gas: gofin_domain::Quantity::from_u64(3),
                max_priority_fee_per_gas: gofin_domain::Quantity::from_u64(1),
                rollup_fee: None,
            },
        };
        let mut topics = vec![
            SESSION_REVOKED_TOPIC.into(),
            format!("0x{}{}", "00".repeat(12), &owner[2..]),
            format!("0x{}{}", "00".repeat(12), &key[2..]),
        ];
        if !success {
            topics.clear();
        }
        let logs = if success {
            vec![CanonicalLog {
                address: crate::SESSION_KEY_MANAGER.into(),
                topics,
                data: "0x".into(),
                log_index: "0x4".into(),
            }]
        } else {
            vec![]
        };
        (
            CanonicalTransaction {
                transaction_hash: format!("0x{}", "11".repeat(32)),
                transaction_from: Some(owner.into()),
                transaction_to: crate::SESSION_KEY_MANAGER.into(),
                transaction_input: calldata,
                block_hash: format!("0x{}", "22".repeat(32)),
                block_number: 42,
                canonical: true,
                success,
                gas_used: Some("21000".into()),
                effective_gas_price: Some("2".into()),
                logs,
                exchange_implementation: None,
                exchange_code_hash: None,
                staking_implementation: None,
                staking_code_hash: None,
                batch_implementation: None,
                batch_code_hash: None,
            },
            binding,
        )
    }

    #[test]
    fn owner_session_revocation_proof_accounts_actual_success_and_revert_gas() {
        let (tx, binding) = session_revoke_fixture(true);
        let proof = verify_session_revocation(&tx, &binding).unwrap();
        assert_eq!(proof.actual_gas_fee_atoms, "42000");

        let (tx, binding) = session_revoke_fixture(false);
        let failure = verify_session_revocation_failure(&tx, &binding).unwrap();
        assert_eq!(failure.actual_gas_fee_atoms, "42000");
        assert!(verify_session_revocation(&tx, &binding).is_err());
    }

    #[test]
    fn owner_session_revocation_gas_proof_fails_closed_on_wrong_sender_or_over_cap() {
        let (mut tx, binding) = session_revoke_fixture(false);
        tx.transaction_from = Some("0x3333333333333333333333333333333333333333".into());
        assert!(verify_session_revocation_failure(&tx, &binding).is_err());

        let (mut tx, binding) = session_revoke_fixture(false);
        tx.gas_used = Some("50001".into());
        tx.effective_gas_price = Some("4".into());
        assert!(verify_session_revocation_failure(&tx, &binding).is_err());
    }
    fn word_u64_bytes(value: u64) -> [u8; 32] {
        U256::from(value).to_be_bytes::<32>()
    }
    fn address_word(value: Address) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[12..].copy_from_slice(value.as_slice());
        w
    }
    fn owner_topic(owner: Address) -> String {
        format!("0x{}", hex::encode(address_word(owner)))
    }
    fn synthetic_liquidation(position_id: u64, paper_amount: u128) -> CanonicalTransaction {
        let owner = address("0x1111111111111111111111111111111111111111").unwrap();
        let mut transfer_data = Vec::new();
        push_word(&mut transfer_data, U256::from(paper_amount));
        let transfer = CanonicalLog {
            address: PAPER_TOKEN.into(),
            topics: vec![
                ERC20_TRANSFER_TOPIC.into(),
                owner_topic(Address::ZERO),
                owner_topic(owner),
            ],
            data: format!("0x{}", hex::encode(transfer_data)),
            log_index: "0x1".into(),
        };
        let mut mint_data = Vec::new();
        for value in [
            U256::from(1),
            U256::from(25),
            U256::ZERO,
            U256::from(25),
            U256::ZERO,
            U256::from(paper_amount),
        ] {
            push_word(&mut mint_data, value);
        }
        let mint = CanonicalLog {
            address: PAPER_TOKENOMICS.into(),
            topics: vec![
                PAPER_LIQUIDATION_MINT_TOPIC.into(),
                owner_topic(owner),
                format!("0x{}", hex::encode(word_u64_bytes(position_id))),
            ],
            data: format!("0x{}", hex::encode(mint_data)),
            log_index: "0x2".into(),
        };
        let mut liquidation_data = Vec::new();
        for value in [
            U256::from(100),
            U256::from(101),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
        ] {
            push_word(&mut liquidation_data, value);
        }
        let liquidation = CanonicalLog {
            address: EXCHANGE.into(),
            topics: vec![
                LIQUIDATED_TOPIC.into(),
                format!("0x{}", hex::encode(word_u64_bytes(position_id))),
                owner_topic(owner),
            ],
            data: format!("0x{}", hex::encode(liquidation_data)),
            log_index: "0x3".into(),
        };
        CanonicalTransaction {
            transaction_hash: format!("0x{}", "a1".repeat(32)),
            transaction_from: None,
            transaction_to: EXCHANGE.into(),
            transaction_input: "0x".into(),
            block_hash: format!("0x{}", "b2".repeat(32)),
            block_number: 42,
            canonical: true,
            success: true,
            gas_used: None,
            effective_gas_price: None,
            logs: vec![transfer, mint, liquidation],
            exchange_implementation: Some(PINNED_EXCHANGE_IMPLEMENTATION.into()),
            exchange_code_hash: Some(PINNED_EXCHANGE_CODE_HASH.into()),
            staking_implementation: None,
            staking_code_hash: None,
            batch_implementation: None,
            batch_code_hash: None,
        }
    }

    #[test]
    fn keeper_liquidation_proof_is_position_specific_and_independent_of_user_intent() {
        let tx = synthetic_liquidation(42, 25_000_000_000_000_000_000);
        let proof =
            verify_liquidation_receipt(&tx, "0x1111111111111111111111111111111111111111", "42")
                .unwrap();
        assert_eq!(proof.position_id, "42");
        assert_eq!(proof.paper_minted, "25000000000000000000");
        assert!(
            verify_liquidation_receipt(&tx, "0x2222222222222222222222222222222222222222", "42",)
                .is_err()
        );
        assert!(
            verify_liquidation_receipt(&tx, "0x1111111111111111111111111111111111111111", "43",)
                .is_err()
        );
        let mut unpinned = tx;
        unpinned.exchange_code_hash = Some(format!("0x{}", "00".repeat(32)));
        assert!(
            verify_liquidation_receipt(
                &unpinned,
                "0x1111111111111111111111111111111111111111",
                "42",
            )
            .is_err()
        );
    }
    fn encode_batch(target: Address, data: &[u8], intent_id: [u8; 32]) -> String {
        let mut out = hex::decode(EXECUTE_SELECTOR).unwrap();
        push_word(&mut out, U256::from(32));
        push_word(&mut out, U256::from(1));
        push_word(&mut out, U256::from(32));
        out.extend_from_slice(&address_word(target));
        push_word(&mut out, U256::from(160));
        push_word(&mut out, U256::from(1));
        push_word(&mut out, U256::ZERO);
        out.extend_from_slice(&intent_id);
        push_word(&mut out, U256::from(data.len()));
        out.extend_from_slice(data);
        while out.len() % 32 != 4 {
            out.push(0);
        }
        format!("0x{}", hex::encode(out))
    }
    #[test]
    fn canonical_open_binds_signature_batch_item_and_position_event() {
        let signer = LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap();
        let owner = signer.address();
        let intent = EvmIntent::OpenPosition {
            user: format!("{owner:#x}"),
            instrument_id: 0,
            is_long: true,
            size: 1_000_000_000_000_000_000,
            leverage: 10,
            use_debt: false,
            nonce: "7".into(),
            deadline: 3_600,
        };
        let signed = signer.sign_papertrade_intent(&intent, 1_000).unwrap();
        let mut call = hex::decode("ec6783f0").unwrap();
        call.extend_from_slice(&address_word(owner));
        for x in [
            U256::ZERO,
            U256::from(1),
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(10),
            U256::ZERO,
            U256::from(7),
            U256::from(3_600),
            U256::from(signed.v.parse::<u64>().unwrap()),
        ] {
            push_word(&mut call, x);
        }
        call.extend(hex::decode(signed.r.trim_start_matches("0x")).unwrap());
        call.extend(hex::decode(signed.s.trim_start_matches("0x")).unwrap());
        let intent_id = b256(&signed.intent_id).unwrap();
        let tx_input = encode_batch(address(EXCHANGE).unwrap(), &call, intent_id);
        let batch_id = format!("0x{}", "11".repeat(32));
        let mut executed_data = Vec::new();
        push_word(&mut executed_data, U256::from(1));
        push_word(&mut executed_data, U256::from(64));
        push_word(&mut executed_data, U256::ZERO);
        let call_log = CanonicalLog {
            address: BATCH_EXECUTOR.into(),
            topics: vec![
                CALL_EXECUTED_TOPIC.into(),
                batch_id.clone(),
                format!("0x{}", hex::encode(word_u64_bytes(0))),
                signed.intent_id.clone(),
            ],
            data: format!("0x{}", hex::encode(executed_data)),
            log_index: "0x3".into(),
        };
        let position_id = word_u64_bytes(42);
        let mut event_data = Vec::new();
        for value in [
            U256::ZERO,
            U256::from(1),
            U256::ZERO,
            U256::from(50_000_000),
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(10),
        ] {
            push_word(&mut event_data, value);
        }
        let position_log = CanonicalLog {
            address: EXCHANGE.into(),
            topics: vec![
                POSITION_OPENED_TOPIC.into(),
                format!("0x{}", hex::encode(position_id)),
                format!("0x{}", hex::encode(address_word(owner))),
            ],
            data: format!("0x{}", hex::encode(event_data)),
            log_index: "0x2".into(),
        };
        // A previous operation for the same owner emits a PositionOpened
        // event in this receipt. The prior CallExecuted marker must fence it
        // out of the selected intent's settlement segment.
        let mut prior_event_data = Vec::new();
        for value in [
            U256::ZERO,
            U256::from(1),
            U256::ZERO,
            U256::from(50_000_000),
            U256::from(9_000_000_000_000_000_000u128),
            U256::from(10),
        ] {
            push_word(&mut prior_event_data, value);
        }
        let prior_position_log = CanonicalLog {
            address: EXCHANGE.into(),
            topics: vec![
                POSITION_OPENED_TOPIC.into(),
                format!("0x{}", hex::encode(word_u64_bytes(41))),
                format!("0x{}", hex::encode(address_word(owner))),
            ],
            data: format!("0x{}", hex::encode(prior_event_data)),
            log_index: "0x0".into(),
        };
        let prior_call_log = CanonicalLog {
            address: BATCH_EXECUTOR.into(),
            // This represents a preceding batch executor marker, not a
            // second executable item; it is only the event-segment boundary.
            topics: vec![format!("0x{}", "55".repeat(32))],
            data: "0x".into(),
            log_index: "0x1".into(),
        };
        let tx = CanonicalTransaction {
            transaction_hash: format!("0x{}", "22".repeat(32)),
            transaction_from: None,
            transaction_to: BATCH_EXECUTOR.into(),
            transaction_input: tx_input,
            block_hash: format!("0x{}", "33".repeat(32)),
            block_number: 4,
            canonical: true,
            success: true,
            gas_used: None,
            effective_gas_price: None,
            logs: vec![prior_position_log, prior_call_log, position_log, call_log],
            exchange_implementation: None,
            exchange_code_hash: None,
            staking_implementation: None,
            staking_code_hash: None,
            batch_implementation: None,
            batch_code_hash: None,
        };
        let typed = PapertradeTypedIntent::OpenPosition {
            user: format!("{owner:#x}"),
            instrument_id: 0,
            is_long: true,
            size: 1_000_000_000_000_000_000,
            leverage: 10,
            use_debt: false,
            nonce: "7".into(),
            deadline: 3_600,
        };
        let proof = verify_canonical_call(&tx, &typed, &signed.intent_id).unwrap();
        assert_eq!(proof.intent_id, signed.intent_id);
        assert_eq!(proof.action, "openPosition");
        assert_eq!(proof.event_ids, vec!["0x2"]);
        let failure_marker = |topic: &str, data: Vec<u8>| CanonicalLog {
            address: BATCH_EXECUTOR.into(),
            topics: vec![
                topic.into(),
                format!("0x{}", "44".repeat(32)),
                format!("0x{}", hex::encode(word_u64_bytes(0))),
                signed.intent_id.clone(),
            ],
            data: format!("0x{}", hex::encode(data)),
            log_index: "0x0".into(),
        };
        let mut rejected = Vec::new();
        push_word(&mut rejected, U256::from(32));
        push_word(&mut rejected, U256::from(4));
        rejected.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        rejected.extend_from_slice(&[0; 28]);
        let mut failed_tx = tx.clone();
        failed_tx.batch_implementation = Some(PINNED_BATCH_IMPLEMENTATION.into());
        failed_tx.batch_code_hash = Some(PINNED_BATCH_CODE_HASH.into());
        failed_tx.logs = vec![failure_marker(CALL_REJECTED_TOPIC, rejected)];
        let rejection = verify_rejected_call(&failed_tx, &typed, &signed.intent_id)
            .unwrap()
            .unwrap();
        assert!(
            matches!(rejection.kind, RejectedCallKind::Rejected { reason_hex } if reason_hex == "0xdeadbeef")
        );

        let mut skipped_data = Vec::new();
        push_word(&mut skipped_data, U256::from(9));
        failed_tx.logs = vec![failure_marker(CALL_SKIPPED_TOPIC, skipped_data)];
        let skipped = verify_rejected_call(&failed_tx, &typed, &signed.intent_id)
            .unwrap()
            .unwrap();
        assert!(
            matches!(skipped.kind, RejectedCallKind::Skipped { group_key } if group_key == format!("0x{:064x}", 9))
        );

        let mut reverted_data = Vec::new();
        push_word(&mut reverted_data, U256::ZERO);
        push_word(&mut reverted_data, U256::from(64));
        push_word(&mut reverted_data, U256::ZERO);
        failed_tx.logs = vec![failure_marker(CALL_EXECUTED_TOPIC, reverted_data)];
        let reverted = verify_rejected_call(&failed_tx, &typed, &signed.intent_id)
            .unwrap()
            .unwrap();
        assert!(
            matches!(reverted.kind, RejectedCallKind::Reverted { return_data } if return_data == "0x")
        );

        let mut tampered = typed.clone();
        if let PapertradeTypedIntent::OpenPosition { instrument_id, .. } = &mut tampered {
            *instrument_id = 1;
        }
        assert!(verify_canonical_call(&tx, &tampered, &signed.intent_id).is_err());
    }

    #[test]
    fn call_executed_return_data_is_exact_abi_and_preserves_zero() {
        let mut zero = Vec::new();
        push_word(&mut zero, U256::from(1));
        push_word(&mut zero, U256::from(64));
        push_word(&mut zero, U256::from(32));
        push_word(&mut zero, U256::ZERO);
        assert_eq!(call_executed_return(&zero).unwrap(), vec![0; 32]);

        let mut malformed = zero.clone();
        malformed.push(0);
        assert!(call_executed_return(&malformed).is_err());
        assert!(call_executed_return(&[0; 96]).is_err());
    }

    #[test]
    fn zero_reward_claim_requires_exact_return_and_staking_implementation_pin() {
        let signer = LocalEnvelopeSigner::from_private_key(&format!("0x{:064x}", 1)).unwrap();
        let owner = signer.address();
        let typed_evm = EvmIntent::Claim {
            user: format!("{owner:#x}"),
            nonce: "3".into(),
            deadline: 3_600,
        };
        let signed = signer.sign_papertrade_intent(&typed_evm, 1_000).unwrap();
        let mut call = hex::decode("9c96d4a1").unwrap();
        call.extend_from_slice(&address_word(owner));
        push_word(&mut call, U256::from(3));
        push_word(&mut call, U256::from(3_600));
        push_word(&mut call, U256::from(signed.v.parse::<u64>().unwrap()));
        call.extend(hex::decode(signed.r.trim_start_matches("0x")).unwrap());
        call.extend(hex::decode(signed.s.trim_start_matches("0x")).unwrap());
        let intent_id = b256(&signed.intent_id).unwrap();
        let tx_input = encode_batch(address(PAPER_STAKING).unwrap(), &call, intent_id);
        let mut returned = Vec::new();
        push_word(&mut returned, U256::from(1));
        push_word(&mut returned, U256::from(64));
        push_word(&mut returned, U256::from(32));
        push_word(&mut returned, U256::ZERO);
        let batch_id = format!("0x{}", "11".repeat(32));
        let call_log = CanonicalLog {
            address: BATCH_EXECUTOR.into(),
            topics: vec![
                CALL_EXECUTED_TOPIC.into(),
                batch_id,
                format!("0x{}", hex::encode(word_u64_bytes(0))),
                signed.intent_id.clone(),
            ],
            data: format!("0x{}", hex::encode(returned)),
            log_index: "0x0".into(),
        };
        let tx = CanonicalTransaction {
            transaction_hash: format!("0x{}", "22".repeat(32)),
            transaction_from: None,
            transaction_to: BATCH_EXECUTOR.into(),
            transaction_input: tx_input,
            block_hash: format!("0x{}", "33".repeat(32)),
            block_number: 4,
            canonical: true,
            success: true,
            gas_used: None,
            effective_gas_price: None,
            logs: vec![call_log],
            exchange_implementation: None,
            exchange_code_hash: None,
            staking_implementation: Some(PINNED_STAKING_IMPLEMENTATION.into()),
            staking_code_hash: Some(PINNED_STAKING_CODE_HASH.into()),
            batch_implementation: None,
            batch_code_hash: None,
        };
        let typed = PapertradeTypedIntent::Claim {
            user: format!("{owner:#x}"),
            nonce: "3".into(),
            deadline: 3_600,
        };
        let proof = verify_canonical_call(&tx, &typed, &signed.intent_id).unwrap();
        assert_eq!(
            proof.call_return_data,
            Some(format!("0x{}", "00".repeat(32)))
        );
        assert!(proof.events.is_empty());

        let mut unpinned = tx;
        unpinned.staking_code_hash = None;
        assert!(verify_canonical_call(&unpinned, &typed, &signed.intent_id).is_err());
    }
}
