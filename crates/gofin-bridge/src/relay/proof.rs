//! Strict decoding of Relay's source depository events. This verifies event
//! contents only; callers must still establish receipt inclusion, canonicality
//! and configured finality with the shared EVM runtime before settlement.
use super::{bridge_error, parse_address};
use alloy::primitives::{Address, B256, U256, keccak256};
use gofin_domain::{GoFinError, Quantity, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawEvmLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedSourceDeposit {
    pub depository: String,
    pub sender: String,
    pub token: Option<String>,
    pub amount: Quantity,
    pub order_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedSourceEvent {
    pub sender: String,
    pub token: Option<String>,
    pub amount: Quantity,
    pub order_id: String,
}

/// Returns a typed match only when the configured depository emitted the exact
/// native/ERC-20 deposit event. Transaction receipt and finality remain external.
pub fn verify_source_deposit_log(
    log: &RawEvmLog,
    expected: &ExpectedSourceDeposit,
) -> Result<VerifiedSourceEvent> {
    let depository = parse_address(&expected.depository)?;
    let event_address = parse_address(&log.address)?;
    ensure(
        event_address == depository,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Source event was emitted by a contract other than the reviewed depository.",
    )?;
    ensure(
        log.topics.len() == 1,
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "Relay source depository event has unexpected indexed topics.",
    )?;
    let topic = parse_b256(&log.topics[0])?;
    let data = decode_hex(&log.data)?;
    let (sender, token, amount, order_id) = if expected.token.is_none() {
        ensure(
            topic == keccak256("RelayNativeDeposit(address,uint256,bytes32)"),
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "Source log is not RelayNativeDeposit.",
        )?;
        ensure(
            data.len() == 96,
            "BRIDGE_PROOF_INVALID",
            "reconcile",
            "Native deposit event data has an invalid ABI length.",
        )?;
        (
            word_address(&data[0..32])?,
            None,
            word_u256(&data[32..64])?,
            word_b256(&data[64..96])?,
        )
    } else {
        ensure(
            topic == keccak256("RelayErc20Deposit(address,address,uint256,bytes32)"),
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "Source log is not RelayErc20Deposit.",
        )?;
        ensure(
            data.len() == 128,
            "BRIDGE_PROOF_INVALID",
            "reconcile",
            "ERC-20 deposit event data has an invalid ABI length.",
        )?;
        (
            word_address(&data[0..32])?,
            Some(format!("{:#x}", word_address(&data[32..64])?)),
            word_u256(&data[64..96])?,
            word_b256(&data[96..128])?,
        )
    };
    ensure(
        sender == parse_address(&expected.sender)?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Relay source event sender differs from the selected wallet.",
    )?;
    if let Some(expected_token) = &expected.token {
        ensure(
            token.as_deref() == Some(format!("{:#x}", parse_address(expected_token)?).as_str()),
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "Relay source event token differs from the selected asset.",
        )?;
    }
    ensure(
        amount
            == U256::from_str_radix(expected.amount.as_str(), 10)
                .map_err(|_| proof_invalid("source amount exceeds uint256"))?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Relay source event amount differs from the approved deposit.",
    )?;
    ensure(
        order_id == parse_b256(&expected.order_id)?,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Relay source event order id differs from the saved protocol order.",
    )?;
    Ok(VerifiedSourceEvent {
        sender: format!("{sender:#x}"),
        token,
        amount: Quantity::new(amount.to_string())?,
        order_id: format!("{order_id:#x}"),
    })
}
/// Canonical transaction fields needed to prove a direct Relay payout. The
/// observer must separately fetch this transaction by the requested hash and
/// establish receipt inclusion, canonicality, and configured finality.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectPaymentTransaction {
    pub transaction_hash: String,
    pub chain_id: u64,
    pub transaction_type: u8,
    pub from: String,
    pub to: String,
    pub value: Quantity,
    pub input: String,
}

/// Immutable expected payout terms derived from the broker-protected quote.
/// Provider request ids are intentionally not accepted as payout correlations.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedDirectPayment {
    pub recipient: String,
    /// `None` represents native currency; `Some` is the exact reviewed ERC-20.
    pub token: Option<String>,
    pub minimum_amount: Quantity,
    pub protocol_order_id: String,
}

/// A receipt log with its actual canonical receipt position. ERC-20 payout
/// verification requires that position so downstream records never invent one.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectTokenTransfer {
    pub log_index: u32,
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedDirectPayment {
    pub transaction_hash: String,
    pub chain_id: u64,
    pub sender: String,
    pub recipient: String,
    pub token: Option<String>,
    pub amount: Quantity,
    pub protocol_order_id: String,
    pub transfer_log_index: Option<u32>,
    pub proof_method: String,
}

/// One parsed EIP-2930 access-list tuple. Native intrinsic-credit verification
/// requires this list to be empty so no storage warmup/refund path can affect
/// the gas comparison.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectAccessListEntry {
    pub address: String,
    pub storage_keys: Vec<String>,
}

/// Verifies a direct destination payout whose transaction input commits to the
/// exact protocol order id. This intentionally does not trust provider status,
/// a guessed hot-wallet identity, Router metadata, or a provider request id.
/// Callers remain responsible for binding the chain to the expected destination
/// and checking the transaction's canonical receipt/finality and recipient code.
pub fn verify_direct_payment_transaction(
    transaction: &DirectPaymentTransaction,
    transfers: &[DirectTokenTransfer],
    expected: &ExpectedDirectPayment,
) -> Result<VerifiedDirectPayment> {
    let tx_hash = parse_b256(&transaction.transaction_hash)?;
    let sender = parse_address(&transaction.from)?;
    let recipient = parse_address(&expected.recipient)?;
    let tx_to = parse_address(&transaction.to)?;
    let order_id = parse_b256(&expected.protocol_order_id)?;
    let minimum = parse_quantity(&expected.minimum_amount, "minimum payout")?;
    let tx_value = parse_quantity(&transaction.value, "transaction value")?;

    ensure(
        transaction.chain_id != 0 && transaction.transaction_type == 2,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Destination payout transaction is not a typed EIP-1559 transaction on a known chain.",
    )?;
    ensure(
        !sender.is_zero() && sender != recipient,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Destination payout transaction has a zero or self-transfer sender.",
    )?;
    let input = decode_hex(&transaction.input)
        .map_err(|_| proof_invalid("transaction input is not canonical 0x-prefixed hex"))?;

    let (token, amount, transfer_log_index) = if let Some(expected_token) = &expected.token {
        let token = parse_address(expected_token)?;
        ensure(
            tx_to == token && tx_value.is_zero(),
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "ERC-20 payout transaction target or native value differs from the approved token transfer.",
        )?;
        // Only the two canonical ERC-20 transfer shapes, each followed by
        // exactly one protocol order id, can establish a tagged direct credit.
        // transferFrom may be sent by a relayer; the token owner is separately
        // bound by calldata and the exact receipt Transfer event.
        let (token_sender, call_recipient, call_amount, footer) = match input.as_slice() {
            [0xa9, 0x05, 0x9c, 0xbb, ..] if input.len() == 100 => (
                sender,
                word_address(&input[4..36])?,
                word_u256(&input[36..68])?,
                &input[68..100],
            ),
            [0x23, 0xb8, 0x72, 0xdd, ..] if input.len() == 132 => (
                word_address(&input[4..36])?,
                word_address(&input[36..68])?,
                word_u256(&input[68..100])?,
                &input[100..132],
            ),
            _ => {
                return Err(bridge_error(
                    "BRIDGE_PROOF_MISMATCH",
                    "reconcile",
                    "ERC-20 payout calldata is not an exact transfer or transferFrom call with a 32-byte order-id footer.",
                    "Keep the original transfer unresolved and inspect its destination receipt.",
                ));
            }
        };
        ensure(
            word_b256(footer)? == order_id,
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "ERC-20 payout calldata footer does not equal the saved protocol order id.",
        )?;
        ensure(
            !token_sender.is_zero()
                && token_sender != recipient
                && call_recipient == recipient
                && call_amount >= minimum
                && !call_amount.is_zero(),
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "ERC-20 payout calldata recipient or amount differs from the approved minimum receipt.",
        )?;
        let transfer_topic = keccak256("Transfer(address,address,uint256)");
        let mut matching_index = None;
        for transfer in transfers {
            if parse_address(&transfer.address)? != token
                || transfer.topics.len() != 3
                || parse_b256(&transfer.topics[0])? != transfer_topic
            {
                continue;
            }
            let transfer_from = topic_address(&transfer.topics[1])?;
            let transfer_to = topic_address(&transfer.topics[2])?;
            let transfer_amount = word_u256(&decode_hex(&transfer.data)?)?;
            if transfer_from == token_sender
                && transfer_to == recipient
                && transfer_amount == call_amount
            {
                ensure(
                    matching_index.is_none(),
                    "BRIDGE_PROOF_INVALID",
                    "reconcile",
                    "Destination receipt contains duplicate matching ERC-20 payout logs.",
                )?;
                matching_index = Some(transfer.log_index);
            }
        }
        let index = matching_index.ok_or_else(|| {
            bridge_error(
                "BRIDGE_PROOF_MISMATCH",
                "reconcile",
                "ERC-20 payout transaction lacks one exact Transfer log from its sender to the approved recipient.",
                "Keep the transfer unresolved and inspect the canonical destination receipt.",
            )
        })?;
        (Some(format!("{token:#x}")), call_amount, Some(index))
    } else {
        ensure(
            tx_to == recipient && !tx_value.is_zero(),
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "Native payout transaction does not transfer value to the approved recipient.",
        )?;
        ensure(
            input.len() == 32 && word_b256(&input)? == order_id,
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "Native payout transaction input must be exactly the saved 32-byte protocol order id.",
        )?;
        (None, tx_value, None)
    };

    ensure(
        amount >= minimum,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Destination payout amount is below the approved minimum receipt.",
    )?;
    Ok(VerifiedDirectPayment {
        transaction_hash: format!("{tx_hash:#x}"),
        chain_id: transaction.chain_id,
        sender: format!("{sender:#x}"),
        recipient: format!("{recipient:#x}"),
        token,
        amount: Quantity::new(amount.to_string())?,
        protocol_order_id: format!("{order_id:#x}"),
        transfer_log_index,
        proof_method: "direct_transaction_order_tag".into(),
    })
}

/// Verifies a directly tagged native payout as an address-level ETH credit
/// using Nitro's split gas accounting. This does not claim the recipient is
/// code-less: a zero-gas `STOP` handler can receive and retain the value.
///
/// The inference is limited to reviewed Nitro networks (Arbitrum One and
/// Robinhood Chain), successful finalized type-2 transactions, an empty access
/// list, no authorization-list envelope (implied by type 2), and a receipt with
/// `gasUsedForL1 <= gasUsed`. After removing Nitro's L1 poster component, L2
/// gas must equal the EIP-2028 intrinsic cost for the exact order-id calldata.
/// EIP-3529's refund cap and removal of SELFDESTRUCT refunds prevent normal
/// CALL-value forwarding or destruction costs from being refunded to zero; an
/// empty access list prevents storage-warming from creating the SSTORE refund
/// edge case. If the chain's receipt gas schedule does not meet this equality,
/// callers must use another independently verified proof or leave settlement
/// pending. EIP-7623 floor gas is not assumed here.
pub fn verify_native_intrinsic_credit(
    transaction: &DirectPaymentTransaction,
    access_list: &[DirectAccessListEntry],
    gas_used: &Quantity,
    gas_used_for_l1: Option<&Quantity>,
    expected: &ExpectedDirectPayment,
) -> Result<VerifiedDirectPayment> {
    ensure(
        expected.token.is_none(),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Intrinsic-gas retained-credit proof applies only to native ETH payouts.",
    )?;
    ensure(
        transaction.chain_id == 42161 || transaction.chain_id == 4663,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Intrinsic-gas retained-credit proof is limited to reviewed Nitro destination chains.",
    )?;
    ensure(
        access_list.is_empty(),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Intrinsic-gas retained-credit proof requires an empty transaction access list.",
    )?;
    let poster_gas = gas_used_for_l1.ok_or_else(|| {
        proof_invalid("Nitro receipt is missing its gasUsedForL1 poster-gas component")
    })?;
    let total_gas = parse_quantity(gas_used, "receipt gasUsed")?;
    let poster_gas = parse_quantity(poster_gas, "receipt gasUsedForL1")?;
    ensure(
        poster_gas <= total_gas,
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "Nitro receipt poster gas exceeds total gas used.",
    )?;
    let execution_gas = total_gas - poster_gas;
    let input = decode_hex(&transaction.input)
        .map_err(|_| proof_invalid("transaction input is not canonical 0x-prefixed hex"))?;
    let zero_bytes = input.iter().filter(|byte| **byte == 0).count();
    let nonzero_bytes = input.len() - zero_bytes;
    let intrinsic_gas = U256::from(21_000u64)
        + U256::from(zero_bytes) * U256::from(4u8)
        + U256::from(nonzero_bytes) * U256::from(16u8);
    ensure(
        execution_gas == intrinsic_gas,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Nitro L2 execution gas is not exactly the EIP-2028 intrinsic cost for the tagged payout.",
    )?;
    let mut payment = verify_direct_payment_transaction(transaction, &[], expected)?;
    payment.proof_method = "intrinsic_gas_retained_credit".into();
    Ok(payment)
}

fn parse_quantity(value: &Quantity, label: &str) -> Result<U256> {
    U256::from_str_radix(value.as_str(), 10)
        .map_err(|_| proof_invalid(&format!("{label} exceeds uint256")))
}

fn ensure(ok: bool, code: &str, stage: &str, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(bridge_error(
            code,
            stage,
            message,
            "Keep the transfer unresolved and inspect its original source transaction.",
        ))
    }
}
fn parse_b256(v: &str) -> Result<B256> {
    B256::from_str(v).map_err(|_| proof_invalid("expected exact 32-byte hex"))
}
fn decode_hex(v: &str) -> Result<Vec<u8>> {
    let raw = v
        .strip_prefix("0x")
        .ok_or_else(|| proof_invalid("event data lacks 0x prefix"))?;
    hex::decode(raw).map_err(|_| proof_invalid("event data is not valid hex"))
}
fn word_address(word: &[u8]) -> Result<Address> {
    if word.len() != 32 || word[..12].iter().any(|b| *b != 0) {
        return Err(proof_invalid("event address word is malformed"));
    }
    Ok(Address::from_slice(&word[12..]))
}
fn word_u256(word: &[u8]) -> Result<U256> {
    if word.len() != 32 {
        return Err(proof_invalid("event uint word is malformed"));
    }
    Ok(U256::from_be_slice(word))
}
fn word_b256(word: &[u8]) -> Result<B256> {
    if word.len() != 32 {
        return Err(proof_invalid("event bytes32 word is malformed"));
    }
    Ok(B256::from_slice(word))
}
fn proof_invalid(msg: &str) -> GoFinError {
    bridge_error(
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        format!("Relay source event is invalid: {msg}."),
        "Keep the transfer unresolved and inspect its original source transaction.",
    )
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedRouterPayment {
    pub router: String,
    /// The canonical receipt transaction sender must be the quote's pinned solver.
    pub expected_transaction_sender: String,
    pub recipient: String,
    pub token: Option<String>,
    pub minimum_amount: Quantity,
    pub order_id: String,
    pub provider_request_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedRouterPayment {
    pub router: String,
    pub recipient: String,
    pub token: Option<String>,
    pub amount: Quantity,
    pub correlation_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RawTokenTransfer {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
}

/// Decodes Relay RouterV3 `FundsMovement` and requires exact metadata equality
/// with one immutable broker quote identity plus the pinned solver as canonical
/// transaction sender. For native output this event proves `safeTransferETH`
/// under the reviewed RouterV3 Solidity semantics: the contract emits the event
/// only after the transfer succeeds. ERC-20 output additionally requires the
/// canonical Transfer log to verify the actual received amount (including
/// fee-on-transfer behavior).
pub fn verify_router_payment_log(
    log: &RawEvmLog,
    token_transfers: &[RawTokenTransfer],
    transaction_sender: &str,
    expected: &ExpectedRouterPayment,
) -> Result<VerifiedRouterPayment> {
    let router = parse_address(&expected.router)?;
    let fill_authority = parse_address(&expected.expected_transaction_sender)?;
    let recipient = parse_address(&expected.recipient)?;
    ensure(
        parse_address(transaction_sender)? == fill_authority,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Destination receipt transaction was not submitted by the quote's pinned Relay fill authority.",
    )?;
    ensure(
        parse_address(&log.address)? == router,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "FundsMovement was emitted by an unreviewed RouterV3.",
    )?;
    ensure(
        log.topics.len() == 1
            && parse_b256(&log.topics[0])?
                == keccak256("FundsMovement(address,address,address,uint256,bytes)"),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "Log is not the exact RouterV3 FundsMovement event.",
    )?;
    let data = decode_hex(&log.data)?;
    ensure(
        data.len() >= 192 && data.len() % 32 == 0,
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "FundsMovement event data has an invalid ABI shape.",
    )?;
    let from = word_address(&data[0..32])?;
    let to = word_address(&data[32..64])?;
    let currency = word_address(&data[64..96])?;
    let amount = word_u256(&data[96..128])?;
    let offset = word_u256(&data[128..160])?.to::<usize>();
    ensure(
        offset == 160 && offset + 32 <= data.len(),
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "FundsMovement metadata offset is noncanonical.",
    )?;
    let metadata_len = word_u256(&data[offset..offset + 32])?.to::<usize>();
    ensure(
        metadata_len == 32 && offset + 32 + 32 <= data.len(),
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "FundsMovement metadata must contain exactly one 32-byte order correlation.",
    )?;
    let metadata = &data[offset + 32..offset + 64];
    ensure(
        data[offset + 64..].iter().all(|b| *b == 0),
        "BRIDGE_PROOF_INVALID",
        "reconcile",
        "FundsMovement metadata has noncanonical trailing bytes.",
    )?;
    let order_id = parse_b256(&expected.order_id)?;
    let request_id = parse_b256(&expected.provider_request_id)?;
    let correlated = if metadata == order_id.as_slice() {
        order_id
    } else if metadata == request_id.as_slice() {
        request_id
    } else {
        return Err(bridge_error(
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "FundsMovement metadata does not exactly match the saved provider request id or protocol order id.",
            "Keep the transfer unresolved; inspect the canonical receipt and original Relay quote.",
        ));
    };
    let expected_currency = expected
        .token
        .as_deref()
        .map(parse_address)
        .transpose()?
        .unwrap_or(Address::ZERO);
    ensure(
        from == router && to == recipient && currency == expected_currency,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "FundsMovement router, recipient or currency differs from the approved transfer.",
    )?;
    let minimum = U256::from_str_radix(expected.minimum_amount.as_str(), 10)
        .map_err(|_| proof_invalid("minimum output exceeds uint256"))?;
    ensure(
        amount >= minimum,
        "BRIDGE_PROOF_MISMATCH",
        "reconcile",
        "FundsMovement amount is below the approved minimum net receipt.",
    )?;
    if expected.token.is_some() {
        let token = parse_address(expected.token.as_deref().unwrap())?;
        let mut matched = false;
        for transfer in token_transfers {
            if parse_address(&transfer.address)? != token
                || transfer.topics.len() != 3
                || parse_b256(&transfer.topics[0])?
                    != keccak256("Transfer(address,address,uint256)")
            {
                continue;
            }
            let from = topic_address(&transfer.topics[1])?;
            let to = topic_address(&transfer.topics[2])?;
            let value = word_u256(&decode_hex(&transfer.data)?)?;
            if from == router && to == recipient && value >= minimum && value == amount {
                matched = true;
                break;
            }
        }
        ensure(
            matched,
            "BRIDGE_PROOF_MISMATCH",
            "reconcile",
            "ERC-20 FundsMovement lacks its matching token Transfer from RouterV3 to the recipient.",
        )?;
    }
    Ok(VerifiedRouterPayment {
        router: format!("{router:#x}"),
        recipient: format!("{recipient:#x}"),
        token: expected.token.as_deref().map(|_| format!("{currency:#x}")),
        amount: Quantity::new(amount.to_string())?,
        correlation_id: format!("{correlated:#x}"),
    })
}
fn topic_address(topic: &str) -> Result<Address> {
    let raw = decode_hex(topic)?;
    word_address(&raw)
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;

    fn direct_tx(
        sender: Address,
        to: Address,
        value: u64,
        input: Vec<u8>,
    ) -> DirectPaymentTransaction {
        DirectPaymentTransaction {
            transaction_hash: format!("{:#x}", B256::repeat_byte(0x51)),
            chain_id: 42161,
            transaction_type: 2,
            from: format!("{sender:#x}"),
            to: format!("{to:#x}"),
            value: Quantity::from_u64(value),
            input: format!("0x{}", hex::encode(input)),
        }
    }

    fn token_transfer(
        index: u32,
        token: Address,
        sender: Address,
        recipient: Address,
        amount: u64,
    ) -> DirectTokenTransfer {
        DirectTokenTransfer {
            log_index: index,
            address: format!("{token:#x}"),
            topics: vec![
                format!("{:#x}", keccak256("Transfer(address,address,uint256)")),
                format!("0x{}", hex::encode(sender.into_word())),
                format!("0x{}", hex::encode(recipient.into_word())),
            ],
            data: format!("0x{}", hex::encode(U256::from(amount).to_be_bytes::<32>())),
        }
    }

    fn erc20_transfer_input(recipient: Address, amount: u64, order_id: B256) -> Vec<u8> {
        let mut input = vec![0xa9, 0x05, 0x9c, 0xbb];
        input.extend_from_slice(recipient.into_word().as_slice());
        input.extend_from_slice(U256::from(amount).to_be_bytes::<32>().as_slice());
        input.extend_from_slice(order_id.as_slice());
        input
    }

    #[test]
    fn native_direct_payout_requires_exact_order_id_input_and_recipient() {
        let sender = Address::from_slice(&[0x11; 20]);
        let recipient = Address::from_slice(&[0x22; 20]);
        let order_id = B256::repeat_byte(0x33);
        let tx = direct_tx(sender, recipient, 105, order_id.as_slice().to_vec());
        let expected = ExpectedDirectPayment {
            recipient: format!("{recipient:#x}"),
            token: None,
            minimum_amount: Quantity::from_u64(100),
            protocol_order_id: format!("{order_id:#x}"),
        };
        let verified = verify_direct_payment_transaction(&tx, &[], &expected).unwrap();
        assert_eq!(verified.amount, Quantity::from_u64(105));
        assert_eq!(verified.protocol_order_id, format!("{order_id:#x}"));
        assert_eq!(verified.transfer_log_index, None);
        assert_eq!(verified.proof_method, "direct_transaction_order_tag");

        let mut wrong_order = tx.clone();
        wrong_order.input = format!("0x{}", hex::encode(B256::repeat_byte(0x34)));
        assert!(verify_direct_payment_transaction(&wrong_order, &[], &expected).is_err());
        let mut appended = tx.clone();
        appended.input.push_str("00");
        assert!(verify_direct_payment_transaction(&appended, &[], &expected).is_err());
        let mut below_floor = tx.clone();
        below_floor.value = Quantity::from_u64(99);
        assert!(verify_direct_payment_transaction(&below_floor, &[], &expected).is_err());
        let mut wrong_recipient = tx.clone();
        wrong_recipient.to = format!("{:#x}", Address::from_slice(&[0x23; 20]));
        assert!(verify_direct_payment_transaction(&wrong_recipient, &[], &expected).is_err());
        let mut self_transfer = tx.clone();
        self_transfer.from = format!("{recipient:#x}");
        assert!(verify_direct_payment_transaction(&self_transfer, &[], &expected).is_err());
        let mut wrong_type = tx;
        wrong_type.transaction_type = 0;
        assert!(verify_direct_payment_transaction(&wrong_type, &[], &expected).is_err());
    }

    #[test]
    fn native_intrinsic_gas_proves_only_retained_address_credit() {
        let sender = Address::from_slice(&[0x11; 20]);
        let recipient = Address::from_slice(&[0x22; 20]);
        let order_id = B256::repeat_byte(0x33);
        let tx = direct_tx(sender, recipient, 105, order_id.as_slice().to_vec());
        let expected = ExpectedDirectPayment {
            recipient: format!("{recipient:#x}"),
            token: None,
            minimum_amount: Quantity::from_u64(100),
            protocol_order_id: format!("{order_id:#x}"),
        };
        // 32 nonzero bytes => intrinsic 21,512; Nitro reports poster gas
        // separately, so total gas used is 21,512 + 384.
        let gas_used = Quantity::from_u64(21_896);
        let poster_gas = Quantity::from_u64(384);
        let verified =
            verify_native_intrinsic_credit(&tx, &[], &gas_used, Some(&poster_gas), &expected)
                .unwrap();
        assert_eq!(verified.amount, Quantity::from_u64(105));
        assert_eq!(verified.proof_method, "intrinsic_gas_retained_credit");

        // Calldata pricing counts zero bytes at 4 gas and nonzero bytes at 16.
        let mut mixed_hash = [0x01; 32];
        mixed_hash[0] = 0;
        let mixed_order = B256::from(mixed_hash);
        let mixed_tx = direct_tx(sender, recipient, 105, mixed_order.as_slice().to_vec());
        let mixed_expected = ExpectedDirectPayment {
            protocol_order_id: format!("{mixed_order:#x}"),
            ..expected.clone()
        };
        let mixed_gas = Quantity::from_u64(21_511); // 21,500 L2 + 11 poster
        let mixed_poster = Quantity::from_u64(11);
        assert!(
            verify_native_intrinsic_credit(
                &mixed_tx,
                &[],
                &mixed_gas,
                Some(&mixed_poster),
                &mixed_expected,
            )
            .is_ok()
        );

        // Any additional net execution gas fails closed, including the cost
        // scale needed for ordinary CALL-value forwarding or SELFDESTRUCT.
        for extra in [1u64, 5_000, 9_000] {
            let gas = Quantity::from_u64(21_896 + extra);
            assert!(
                verify_native_intrinsic_credit(&tx, &[], &gas, Some(&poster_gas), &expected,)
                    .is_err(),
                "extra L2 gas {extra} must fail"
            );
        }
        assert!(verify_native_intrinsic_credit(&tx, &[], &gas_used, None, &expected).is_err());
        let too_much_poster = Quantity::from_u64(21_897);
        assert!(
            verify_native_intrinsic_credit(&tx, &[], &gas_used, Some(&too_much_poster), &expected,)
                .is_err()
        );
        let nonempty_access_list = [DirectAccessListEntry {
            address: format!("{recipient:#x}"),
            storage_keys: vec![format!("{:#x}", B256::repeat_byte(0x44))],
        }];
        assert!(
            verify_native_intrinsic_credit(
                &tx,
                &nonempty_access_list,
                &gas_used,
                Some(&poster_gas),
                &expected,
            )
            .is_err()
        );

        let mut wrong_chain = tx.clone();
        wrong_chain.chain_id = 8453;
        assert!(
            verify_native_intrinsic_credit(
                &wrong_chain,
                &[],
                &gas_used,
                Some(&poster_gas),
                &expected,
            )
            .is_err()
        );
        let mut wrong_order = tx.clone();
        wrong_order.input = format!("0x{}", hex::encode(B256::repeat_byte(0x34)));
        assert!(
            verify_native_intrinsic_credit(
                &wrong_order,
                &[],
                &gas_used,
                Some(&poster_gas),
                &expected,
            )
            .is_err()
        );
    }

    #[test]
    fn erc20_transfer_from_payout_binds_owner_recipient_order_and_exact_log() {
        let sender = Address::repeat_byte(0x11);
        let owner = Address::repeat_byte(0x12);
        let recipient = Address::repeat_byte(0x22);
        let token = Address::repeat_byte(0x33);
        let order = B256::repeat_byte(0x44);
        let mut input = vec![0x23, 0xb8, 0x72, 0xdd];
        input.extend_from_slice(owner.into_word().as_slice());
        input.extend_from_slice(recipient.into_word().as_slice());
        input.extend_from_slice(U256::from(105).to_be_bytes::<32>().as_slice());
        input.extend_from_slice(order.as_slice());
        let tx = direct_tx(sender, token, 0, input.clone());
        let expected = ExpectedDirectPayment {
            recipient: format!("{recipient:#x}"),
            token: Some(format!("{token:#x}")),
            minimum_amount: Quantity::from_u64(100),
            protocol_order_id: format!("{order:#x}"),
        };
        let transfer = token_transfer(7, token, owner, recipient, 105);
        let proof =
            verify_direct_payment_transaction(&tx, std::slice::from_ref(&transfer), &expected)
                .unwrap();
        assert_eq!(proof.amount, Quantity::from_u64(105));
        assert_eq!(proof.sender, format!("{sender:#x}"));
        assert_eq!(proof.transfer_log_index, Some(7));
        for (offset, replacement) in [
            (4, recipient.into_word().to_vec()), // self-credit is not net receipt
            (4, Address::ZERO.into_word().to_vec()),
            (4, vec![1; 32]), // noncanonical address padding
            (36, owner.into_word().to_vec()),
            (68, U256::from(99).to_be_bytes::<32>().to_vec()),
            (100, B256::repeat_byte(0x45).to_vec()),
        ] {
            let mut changed = input.clone();
            changed[offset..offset + 32].copy_from_slice(&replacement);
            let bad = direct_tx(sender, token, 0, changed);
            assert!(
                verify_direct_payment_transaction(&bad, std::slice::from_ref(&transfer), &expected)
                    .is_err()
            );
        }
        for changed in [input[..100].to_vec(), [input.clone(), vec![0]].concat()] {
            let bad = direct_tx(sender, token, 0, changed);
            assert!(
                verify_direct_payment_transaction(&bad, std::slice::from_ref(&transfer), &expected)
                    .is_err()
            );
        }
        for logs in [
            vec![],
            vec![token_transfer(7, token, sender, recipient, 105)],
            vec![token_transfer(7, token, owner, recipient, 104)],
            vec![token_transfer(7, owner, owner, recipient, 105)],
            vec![transfer.clone(), transfer],
        ] {
            assert!(verify_direct_payment_transaction(&tx, &logs, &expected).is_err());
        }
    }

    #[test]
    fn erc20_direct_payout_requires_canonical_footer_and_exact_receipt_transfer() {
        let sender = Address::from_slice(&[0x11; 20]);
        let recipient = Address::from_slice(&[0x22; 20]);
        let token = Address::from_slice(&[0x33; 20]);
        let order_id = B256::repeat_byte(0x44);
        let tx = direct_tx(
            sender,
            token,
            0,
            erc20_transfer_input(recipient, 105, order_id),
        );
        let expected = ExpectedDirectPayment {
            recipient: format!("{recipient:#x}"),
            token: Some(format!("{token:#x}")),
            minimum_amount: Quantity::from_u64(100),
            protocol_order_id: format!("{order_id:#x}"),
        };
        let transfer = token_transfer(7, token, sender, recipient, 105);
        let verified =
            verify_direct_payment_transaction(&tx, std::slice::from_ref(&transfer), &expected)
                .unwrap();
        assert_eq!(verified.amount, Quantity::from_u64(105));
        assert_eq!(verified.transfer_log_index, Some(7));

        assert!(verify_direct_payment_transaction(&tx, &[], &expected).is_err());
        let mut wrong_footer = tx.clone();
        wrong_footer.input = format!(
            "0x{}",
            hex::encode(erc20_transfer_input(
                recipient,
                105,
                B256::repeat_byte(0x45)
            ))
        );
        assert!(
            verify_direct_payment_transaction(
                &wrong_footer,
                std::slice::from_ref(&transfer),
                &expected
            )
            .is_err()
        );
        let mut wrong_calldata_recipient = tx.clone();
        wrong_calldata_recipient.input = format!(
            "0x{}",
            hex::encode(erc20_transfer_input(
                Address::from_slice(&[0x23; 20]),
                105,
                order_id
            ))
        );
        assert!(
            verify_direct_payment_transaction(
                &wrong_calldata_recipient,
                std::slice::from_ref(&transfer),
                &expected
            )
            .is_err()
        );
        let mut extra_bytes = tx.clone();
        extra_bytes.input.push_str("00");
        assert!(
            verify_direct_payment_transaction(
                &extra_bytes,
                std::slice::from_ref(&transfer),
                &expected
            )
            .is_err()
        );
        let mut wrong_selector = tx.clone();
        let mut data = hex::decode(wrong_selector.input.trim_start_matches("0x")).unwrap();
        data[0] = 0xa8;
        wrong_selector.input = format!("0x{}", hex::encode(data));
        assert!(
            verify_direct_payment_transaction(
                &wrong_selector,
                std::slice::from_ref(&transfer),
                &expected
            )
            .is_err()
        );
        let mut noncanonical_recipient_word = tx.clone();
        let mut data =
            hex::decode(noncanonical_recipient_word.input.trim_start_matches("0x")).unwrap();
        data[4] = 1;
        noncanonical_recipient_word.input = format!("0x{}", hex::encode(data));
        assert!(
            verify_direct_payment_transaction(
                &noncanonical_recipient_word,
                std::slice::from_ref(&transfer),
                &expected
            )
            .is_err()
        );
        let mut wrong_token = tx.clone();
        wrong_token.to = format!("{:#x}", Address::from_slice(&[0x34; 20]));
        assert!(
            verify_direct_payment_transaction(
                &wrong_token,
                std::slice::from_ref(&transfer),
                &expected
            )
            .is_err()
        );
        let mut nonzero_value = tx.clone();
        nonzero_value.value = Quantity::from_u64(1);
        assert!(
            verify_direct_payment_transaction(
                &nonzero_value,
                std::slice::from_ref(&transfer),
                &expected
            )
            .is_err()
        );
        let wrong_sender_log = token_transfer(7, token, recipient, recipient, 105);
        assert!(verify_direct_payment_transaction(&tx, &[wrong_sender_log], &expected).is_err());
        let wrong_amount_log = token_transfer(7, token, sender, recipient, 104);
        assert!(verify_direct_payment_transaction(&tx, &[wrong_amount_log], &expected).is_err());
        assert!(
            verify_direct_payment_transaction(&tx, &[transfer.clone(), transfer], &expected)
                .is_err()
        );
        let mut self_transfer = tx;
        self_transfer.from = format!("{recipient:#x}");
        assert!(
            verify_direct_payment_transaction(
                &self_transfer,
                &[token_transfer(7, token, recipient, recipient, 105)],
                &expected
            )
            .is_err()
        );
    }

    #[test]
    fn funds_movement_requires_exact_router_asset_recipient_and_metadata() {
        let router = Address::from_slice(&[0x44; 20]);
        let recipient = Address::from_slice(&[0x22; 20]);
        let token = Address::from_slice(&[0x33; 20]);
        let order = B256::repeat_byte(9);
        let request = B256::repeat_byte(8);
        let mut data = Vec::new();
        data.extend_from_slice(router.into_word().as_slice());
        data.extend_from_slice(recipient.into_word().as_slice());
        data.extend_from_slice(token.into_word().as_slice());
        data.extend_from_slice(U256::from(500u64).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(U256::from(160u64).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(U256::from(32u64).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(order.as_slice());
        let log = RawEvmLog {
            address: format!("{router:#x}"),
            topics: vec![format!(
                "{:#x}",
                keccak256("FundsMovement(address,address,address,uint256,bytes)")
            )],
            data: format!("0x{}", hex::encode(data)),
        };
        let transfer = RawTokenTransfer {
            address: format!("{token:#x}"),
            topics: vec![
                format!("{:#x}", keccak256("Transfer(address,address,uint256)")),
                format!("0x{}", hex::encode(router.into_word())),
                format!("0x{}", hex::encode(recipient.into_word())),
            ],
            data: format!("0x{}", hex::encode(U256::from(500u64).to_be_bytes::<32>())),
        };
        let expected = ExpectedRouterPayment {
            router: format!("{router:#x}"),
            expected_transaction_sender: "0xf70da97812cb96acdf810712aa562db8dfa3dbef".into(),
            recipient: format!("{recipient:#x}"),
            token: Some(format!("{token:#x}")),
            minimum_amount: Quantity::from_u64(500),
            order_id: format!("{order:#x}"),
            provider_request_id: format!("{request:#x}"),
        };
        assert!(
            verify_router_payment_log(
                &log,
                std::slice::from_ref(&transfer),
                &expected.expected_transaction_sender,
                &expected
            )
            .is_ok(),
            "{:?}",
            verify_router_payment_log(
                &log,
                std::slice::from_ref(&transfer),
                &expected.expected_transaction_sender,
                &expected
            )
        );
        assert!(
            verify_router_payment_log(
                &log,
                std::slice::from_ref(&transfer),
                "0x1111111111111111111111111111111111111111",
                &expected
            )
            .is_err()
        );
        let mut bad = expected.clone();
        bad.minimum_amount = Quantity::from_u64(501);
        assert!(
            verify_router_payment_log(
                &log,
                std::slice::from_ref(&transfer),
                &bad.expected_transaction_sender,
                &bad
            )
            .is_err()
        );
        let mut bad = expected;
        bad.order_id = format!("{:#x}", B256::repeat_byte(7));
        bad.provider_request_id = format!("{:#x}", B256::repeat_byte(6));
        assert!(
            verify_router_payment_log(&log, &[transfer], &bad.expected_transaction_sender, &bad)
                .is_err()
        );
    }
    #[test]
    fn native_deposit_log_must_match_every_bound_field() {
        let from = Address::from_slice(&[0x11; 20]);
        let depository = Address::from_slice(&[0x44; 20]);
        let id = B256::repeat_byte(7);
        let mut data = Vec::new();
        data.extend_from_slice(from.into_word().as_slice());
        data.extend_from_slice(U256::from(300u64).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(id.as_slice());
        let log = RawEvmLog {
            address: format!("{depository:#x}"),
            topics: vec![format!(
                "{:#x}",
                keccak256("RelayNativeDeposit(address,uint256,bytes32)")
            )],
            data: format!("0x{}", hex::encode(data)),
        };
        let expected = ExpectedSourceDeposit {
            depository: format!("{depository:#x}"),
            sender: format!("{from:#x}"),
            token: None,
            amount: Quantity::from_u64(300),
            order_id: format!("{id:#x}"),
        };
        assert!(verify_source_deposit_log(&log, &expected).is_ok());
        let mut tampered = expected.clone();
        tampered.amount = Quantity::from_u64(301);
        assert!(verify_source_deposit_log(&log, &tampered).is_err());
        let mut tampered = expected;
        tampered.order_id = format!("{:#x}", B256::repeat_byte(8));
        assert!(verify_source_deposit_log(&log, &tampered).is_err());
    }
}
