//! Stateful Trigger fill controls for complete synthetic lifecycle rehearsals.
use super::*;
use ed25519_dalek::Signer as _;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

const FILL_CONTROL_ENV: &str = "GOFIN_DEV_SOLANA_TRIGGER_FILL_FILE";
const EXTERNAL_ORDER_CONTROL_ENV: &str = "GOFIN_DEV_SOLANA_TRIGGER_EXTERNAL_ORDER_FILE";
const SYNTHETIC_FEE_LAMPORTS: u64 = 5_000;

static NEXT_TRIGGER_REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static PENDING_TRIGGER_MESSAGES: OnceLock<Mutex<BTreeMap<String, [u8; 32]>>> = OnceLock::new();

pub(super) fn next_execute_request_id() -> String {
    let sequence = NEXT_TRIGGER_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("trigger-fixture-{sequence}")
}

pub(super) fn bind_execute_request(request_id: &str, unsigned_transaction: &str) -> Result<()> {
    let digest = trigger_message_digest(unsigned_transaction)?;
    let pending = PENDING_TRIGGER_MESSAGES.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut pending = pending.lock().map_err(|_| fixture_state_error())?;
    if request_id.is_empty() || request_id.len() > 128 || pending.contains_key(request_id) {
        return Err(fixture_state_error());
    }
    if pending.len() >= 32 {
        return Err(GoFinError::validation(
            "Too many synthetic Trigger transactions await execution.",
        ));
    }
    pending.insert(request_id.to_owned(), digest);
    Ok(())
}

pub(super) fn validate_execute_request(request_id: &str, signed_transaction: &str) -> Result<()> {
    let submitted_digest = trigger_message_digest(signed_transaction)?;
    let pending = PENDING_TRIGGER_MESSAGES.get_or_init(|| Mutex::new(BTreeMap::new()));
    let expected_digest = pending
        .lock()
        .map_err(|_| fixture_state_error())?
        .remove(request_id)
        .ok_or_else(|| {
            GoFinError::validation("The synthetic Trigger request ID has no prepared transaction.")
        })?;
    if expected_digest != submitted_digest {
        return Err(GoFinError::validation(
            "The synthetic Trigger signed message does not match its prepared request ID.",
        ));
    }
    Ok(())
}

fn trigger_message_digest(encoded: &str) -> Result<[u8; 32]> {
    if encoded.len() > 256 * 1024 {
        return Err(fixture_state_error());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| fixture_state_error())?;
    let transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| fixture_state_error())?;
    let message = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&transaction.message)
        .map_err(|_| fixture_state_error())?;
    Ok(Sha256::digest(message).into())
}

#[cfg(test)]
mod request_binding_tests {
    use super::*;

    fn wire(message: VersionedMessage, signature: Signature) -> String {
        let bytes = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&VersionedTransaction {
                signatures: vec![signature],
                message,
            })
            .unwrap();
        STANDARD.encode(bytes)
    }

    #[test]
    fn execute_request_id_is_bound_to_message_and_allows_signature_completion() {
        let payer = Pubkey::new_from_array([13; 32]);
        let message = VersionedMessage::Legacy(Message::new(&[], Some(&payer)));
        let unsigned = wire(message.clone(), Signature::default());
        let signed = wire(message.clone(), Signature::from([23; 64]));
        let request_id = next_execute_request_id();
        bind_execute_request(&request_id, &unsigned).unwrap();
        validate_execute_request(&request_id, &signed).unwrap();

        let request_id = next_execute_request_id();
        bind_execute_request(&request_id, &unsigned).unwrap();
        let mut changed = match message {
            VersionedMessage::Legacy(message) => message,
            VersionedMessage::V0(_) | VersionedMessage::V1(_) => unreachable!(),
        };
        changed.recent_blockhash = solana_hash::Hash::new_from_array([24; 32]);
        let tampered = wire(VersionedMessage::Legacy(changed), Signature::from([23; 64]));
        assert!(validate_execute_request(&request_id, &tampered).is_err());
        assert!(validate_execute_request(&request_id, &signed).is_err());
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FillControl {
    order_key: String,
    making_atoms: String,
    taking_atoms: String,
    #[serde(default)]
    close_rent_accounts: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalOrderControl {
    input_mint: String,
    output_mint: String,
    making_atoms: String,
    taking_atoms: String,
}

/// Seed one finalized wallet-owned order that has no GoFin journal parent. The
/// harness calls this only while serving its synthetic active-order listing.
pub(super) fn seed_external_order(rpc: &FixtureRpc, wallet: &str) -> Result<()> {
    let Some(path) = control_file(EXTERNAL_ORDER_CONTROL_ENV)? else {
        return Ok(());
    };
    let contents = std::fs::read(&path).map_err(|_| fixture_state_error())?;
    if contents.len() > 4096 {
        return Err(fixture_state_error());
    }
    let control: ExternalOrderControl =
        serde_json::from_slice(&contents).map_err(|_| fixture_state_error())?;
    let making = control
        .making_atoms
        .parse::<u64>()
        .map_err(|_| fixture_state_error())?;
    let taking = control
        .taking_atoms
        .parse::<u64>()
        .map_err(|_| fixture_state_error())?;
    if making == 0 || taking == 0 {
        return Err(fixture_state_error());
    }
    let unique_id = {
        let state = rpc.0.lock().map_err(|_| fixture_state_error())?;
        u64::try_from(state.transactions.len())
            .map_err(|_| fixture_state_error())?
            .checked_add(900_000)
            .ok_or_else(fixture_state_error)?
    };
    let (wire, _order_key) = super::order_transaction(
        rpc,
        wallet,
        &control.input_mint,
        &control.output_mint,
        making,
        taking,
        unique_id,
        None,
    )?;
    let signed_wire = sign_synthetic_wallet_transaction(&wire, wallet)?;
    if super::submit(rpc, &signed_wire)?.is_none() {
        return Err(GoFinError::validation(
            "The synthetic external Trigger order was not recognized.",
        ));
    }
    std::fs::remove_file(path).map_err(|_| fixture_state_error())
}

fn sign_synthetic_wallet_transaction(encoded: &str, wallet: &str) -> Result<String> {
    let keypair = bs58::decode(SYNTHETIC_KEYPAIR)
        .into_vec()
        .map_err(|_| fixture_state_error())?;
    let seed: [u8; 32] = keypair
        .get(..32)
        .ok_or_else(fixture_state_error)?
        .try_into()
        .map_err(|_| fixture_state_error())?;
    let signer = ed25519_dalek::SigningKey::from_bytes(&seed);
    let signer_address = Pubkey::new_from_array(signer.verifying_key().to_bytes()).to_string();
    if signer_address != wallet {
        return Err(GoFinError::validation(
            "The synthetic external order wallet does not match its test signer.",
        ));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| fixture_state_error())?;
    let mut transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| fixture_state_error())?;
    let message = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&transaction.message)
        .map_err(|_| fixture_state_error())?;
    let Some(signature) = transaction.signatures.first_mut() else {
        return Err(fixture_state_error());
    };
    *signature = Signature::from(signer.sign(&message).to_bytes());
    let signed = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&transaction)
        .map_err(|_| fixture_state_error())?;
    Ok(STANDARD.encode(signed))
}

/// Apply one test-only fill request before the next finalized signature scan.
/// The file must live inside the isolated synthetic state directory, and is
/// removed only after the corresponding proof has been durably recorded.
pub(super) fn apply_fill_control(rpc: &FixtureRpc, requested_order: &str) -> Result<()> {
    let Some(raw_path) = std::env::var_os(FILL_CONTROL_ENV) else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(raw_path);
    if path.as_os_str().len() > 4096
        || !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(fixture_state_error());
    }
    let Some(state_home) = std::env::var_os("GOFIN_DEV_STATE_HOME").map(std::path::PathBuf::from)
    else {
        return Err(fixture_state_error());
    };
    if !path.starts_with(state_home) {
        return Err(fixture_state_error());
    }
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Ok(());
    };
    if metadata.len() > 4096 {
        return Err(fixture_state_error());
    }
    let contents = std::fs::read(&path).map_err(|_| fixture_state_error())?;
    let control: FillControl =
        serde_json::from_slice(&contents).map_err(|_| fixture_state_error())?;
    if control.order_key != requested_order {
        return Ok(());
    }
    let making = control
        .making_atoms
        .parse::<u64>()
        .map_err(|_| fixture_state_error())?;
    let taking = control
        .taking_atoms
        .parse::<u64>()
        .map_err(|_| fixture_state_error())?;
    apply_synthetic_fill(
        rpc,
        &control.order_key,
        making,
        taking,
        control.close_rent_accounts,
    )?;
    std::fs::remove_file(path).map_err(|_| fixture_state_error())
}

fn control_file(variable: &str) -> Result<Option<std::path::PathBuf>> {
    let Some(raw_path) = std::env::var_os(variable) else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(raw_path);
    if path.as_os_str().len() > 4096
        || !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(fixture_state_error());
    }
    let Some(state_home) = std::env::var_os("GOFIN_DEV_STATE_HOME").map(std::path::PathBuf::from)
    else {
        return Err(fixture_state_error());
    };
    if !path.starts_with(state_home) || !path.is_file() {
        return Ok(None);
    }
    Ok(Some(path))
}

pub(super) fn transaction_mentions_address(transaction: &StoredTransaction, address: &str) -> bool {
    let Ok(bytes) = STANDARD.decode(&transaction.wire) else {
        return false;
    };
    let Ok(decoded) = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize::<VersionedTransaction>(&bytes)
    else {
        return false;
    };
    decoded
        .message
        .static_account_keys()
        .iter()
        .any(|key| key.to_string() == address)
}

/// Append a finalized synthetic Trigger trade receipt to the shared local
/// ledger. This enters the same production history decoder as public CLI runs.
pub(super) fn apply_synthetic_fill(
    rpc: &FixtureRpc,
    order_key: &str,
    making: u64,
    taking: u64,
    close_rent_accounts: bool,
) -> Result<String> {
    if making == 0 || taking == 0 {
        return Err(GoFinError::validation(
            "A synthetic Trigger fill must have positive input and output amounts.",
        ));
    }
    let blockhash = rpc.next_fixture_blockhash()?;
    let mut state = rpc.0.lock().map_err(|_| fixture_state_error())?;
    let (owner, order_data) = state
        .account_data
        .get(order_key)
        .cloned()
        .ok_or_else(|| GoFinError::validation("The synthetic Trigger order is absent."))?;
    if owner != TRIGGER_PROGRAM {
        return Err(GoFinError::validation(
            "The synthetic Trigger order has the wrong owner.",
        ));
    }
    let decoded = decode_fixture_order(&order_data)?;
    if making > decoded.remaining_making || taking > decoded.remaining_taking {
        return Err(GoFinError::validation(
            "The synthetic Trigger fill exceeds the open order remainder.",
        ));
    }
    let remaining_making = decoded.remaining_making - making;
    let remaining_taking = decoded.remaining_taking - taking;
    if close_rent_accounts && (remaining_making != 0 || remaining_taking != 0) {
        return Err(GoFinError::validation(
            "Synthetic Trigger rent return is only valid on the terminal full fill.",
        ));
    }
    let reserve_before = state
        .token_amounts
        .get(&decoded.reserve)
        .copied()
        .ok_or_else(|| GoFinError::validation("The synthetic Trigger escrow is absent."))?;
    if reserve_before != decoded.remaining_making {
        return Err(GoFinError::validation(
            "The synthetic Trigger escrow disagrees with the verified order remainder.",
        ));
    }
    let reserve_after = reserve_before - making;
    let output_ata = gofin_solana::associated_token_address(
        &decoded.maker,
        &decoded.output_mint,
        &decoded.output_token_program,
    )?;
    let output_before = state
        .token_amounts
        .get(&output_ata)
        .copied()
        .ok_or_else(|| {
            GoFinError::validation("The synthetic Trigger output token account is absent.")
        })?;
    let output_after = output_before
        .checked_add(taking)
        .ok_or_else(|| GoFinError::validation("The synthetic Trigger output overflows."))?;

    let taker_signer = ed25519_dalek::SigningKey::from_bytes(&[29; 32]);
    let taker_key = Pubkey::new_from_array(taker_signer.verifying_key().to_bytes());
    let order_pubkey = key(order_key)?;
    let reserve_pubkey = key(&decoded.reserve)?;
    let maker_pubkey = key(&decoded.maker)?;
    let output_pubkey = key(&output_ata)?;
    let program = key(TRIGGER_PROGRAM)?;
    let instruction = Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(order_pubkey, false),
            AccountMeta::new(reserve_pubkey, false),
            AccountMeta::new(output_pubkey, false),
            AccountMeta::new(maker_pubkey, false),
            AccountMeta::new_readonly(taker_key, true),
        ],
        data: Vec::new(),
    };
    let message = Message::new_with_blockhash(&[instruction], Some(&taker_key), &blockhash);
    let message_bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&message)
        .map_err(|_| GoFinError::validation("The synthetic Trigger message is malformed."))?;
    let signature = Signature::from(taker_signer.sign(&message_bytes).to_bytes());
    let transaction = VersionedTransaction {
        signatures: vec![signature],
        message: VersionedMessage::Legacy(message),
    };
    let wire = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .serialize(&transaction)
        .map_err(|_| GoFinError::validation("The synthetic Trigger fill is malformed."))?;
    let keys = transaction
        .message
        .static_account_keys()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let index = |address: &str| {
        keys.iter()
            .position(|key| key == address)
            .ok_or_else(|| GoFinError::validation("A synthetic Trigger proof account is missing."))
    };
    let taker_address = taker_key.to_string();
    let maker_address = decoded.maker.clone();
    let taker_index = index(&taker_address)?;
    let maker_index = index(&maker_address)?;
    let order_index = index(order_key)?;
    let reserve_index = index(&decoded.reserve)?;
    let output_index = index(&output_ata)?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[taker_index] = SYNTHETIC_FEE_LAMPORTS * 2;
    post_balances[taker_index] = SYNTHETIC_FEE_LAMPORTS;
    pre_balances[order_index] = RENT;
    post_balances[order_index] = if close_rent_accounts { 0 } else { RENT };
    pre_balances[reserve_index] = RENT;
    post_balances[reserve_index] = if close_rent_accounts { 0 } else { RENT };
    pre_balances[maker_index] = state.lamports;
    let rent_refund = if close_rent_accounts {
        RENT.checked_mul(2)
            .ok_or_else(|| GoFinError::validation("The synthetic Trigger rent refund overflows."))?
    } else {
        0
    };
    post_balances[maker_index] = state
        .lamports
        .checked_add(rent_refund)
        .ok_or_else(|| GoFinError::validation("The synthetic Trigger rent refund overflows."))?;
    if state.account_data.contains_key(&output_ata) {
        pre_balances[output_index] = RENT;
        post_balances[output_index] = RENT;
    }
    let input_decimals = mint_profile(&decoded.input_mint).1;
    let output_decimals = mint_profile(&decoded.output_mint).1;
    let pre_token_balances = vec![
        token_balance(
            reserve_index,
            &decoded.input_mint,
            order_key,
            &decoded.input_token_program,
            input_decimals,
            reserve_before,
        ),
        token_balance(
            output_index,
            &decoded.output_mint,
            &decoded.maker,
            &decoded.output_token_program,
            output_decimals,
            output_before,
        ),
    ];
    let mut post_token_balances = vec![
        token_balance(
            reserve_index,
            &decoded.input_mint,
            order_key,
            &decoded.input_token_program,
            input_decimals,
            reserve_after,
        ),
        token_balance(
            output_index,
            &decoded.output_mint,
            &decoded.maker,
            &decoded.output_token_program,
            output_decimals,
            output_after,
        ),
    ];
    if close_rent_accounts {
        post_token_balances.retain(|balance| {
            balance.get("accountIndex").and_then(Value::as_u64) != Some(reserve_index as u64)
        });
    }
    let mut updated_order = order_data;
    let remaining_making_offset = 8 + 32 * 6 + 8 * 3;
    let remaining_taking_offset = remaining_making_offset + 8;
    updated_order[remaining_making_offset..remaining_making_offset + 8]
        .copy_from_slice(&remaining_making.to_le_bytes());
    updated_order[remaining_taking_offset..remaining_taking_offset + 8]
        .copy_from_slice(&remaining_taking.to_le_bytes());
    if close_rent_accounts {
        state.account_data.remove(order_key);
        state.account_data.remove(&decoded.reserve);
        state.token_amounts.remove(&decoded.reserve);
        state.lamports = state.lamports.checked_add(rent_refund).ok_or_else(|| {
            GoFinError::validation("The synthetic Trigger rent refund overflows.")
        })?;
    } else {
        state.account_data.insert(
            order_key.to_owned(),
            (TRIGGER_PROGRAM.to_owned(), updated_order),
        );
        state.token_amounts.insert(decoded.reserve, reserve_after);
    }
    state.token_amounts.insert(output_ata, output_after);
    let signature = signature.to_string();
    let mut logs = vec![format!("Program {TRIGGER_PROGRAM} invoke [1]")];
    let mut event = vec![189, 219, 127, 211, 78, 230, 97, 238];
    event.extend_from_slice(key(order_key)?.as_ref());
    event.extend_from_slice(taker_key.as_ref());
    event.extend_from_slice(&remaining_making.to_le_bytes());
    event.extend_from_slice(&remaining_taking.to_le_bytes());
    event.extend_from_slice(&making.to_le_bytes());
    event.extend_from_slice(&taking.to_le_bytes());
    logs.push(format!("Program data: {}", STANDARD.encode(event)));
    logs.push(format!("Program {TRIGGER_PROGRAM} success"));
    state.transactions.push(StoredTransaction {
        signature: signature.clone(),
        wire: STANDARD.encode(wire),
        pre_balances,
        post_balances,
        pre_token_balances,
        post_token_balances,
        logs,
    });
    rpc.record_submission(&mut state, signature.clone())?;
    Ok(signature)
}

struct FixtureOrder {
    maker: String,
    input_mint: String,
    output_mint: String,
    input_token_program: String,
    output_token_program: String,
    reserve: String,
    remaining_making: u64,
    remaining_taking: u64,
}

fn decode_fixture_order(data: &[u8]) -> Result<FixtureOrder> {
    if data.len() < 8 + 6 * 32 + 6 * 8 || data.get(..8) != Some(&ORDER_DISC) {
        return Err(GoFinError::validation(
            "The synthetic Trigger order data is malformed.",
        ));
    }
    let read_amount = |offset: usize| -> Result<u64> {
        Ok(u64::from_le_bytes(
            data.get(offset..offset + 8)
                .ok_or_else(|| {
                    GoFinError::validation("The synthetic Trigger amount is truncated.")
                })?
                .try_into()
                .map_err(|_| {
                    GoFinError::validation("The synthetic Trigger amount is malformed.")
                })?,
        ))
    };
    Ok(FixtureOrder {
        maker: read_key(data, 8)?,
        input_mint: read_key(data, 8 + 32)?,
        output_mint: read_key(data, 8 + 64)?,
        input_token_program: read_key(data, 8 + 96)?,
        output_token_program: read_key(data, 8 + 128)?,
        reserve: read_key(data, 8 + 160)?,
        remaining_making: read_amount(8 + 32 * 6 + 8 * 3)?,
        remaining_taking: read_amount(8 + 32 * 6 + 8 * 4)?,
    })
}
