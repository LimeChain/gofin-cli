//! Synthetic, proof-backed Trigger V1 placement and cancellation lifecycle.
use super::*;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;

#[path = "trigger/lifecycle.rs"]
mod lifecycle;

const TRIGGER_PROGRAM: &str = "j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X";
const ORDER_DISC: [u8; 8] = [134, 173, 223, 185, 77, 86, 28, 51];
const INITIALIZE_DISC: [u8; 8] = [133, 110, 74, 175, 112, 159, 245, 159];
const CANCEL_DISC: [u8; 8] = [95, 129, 237, 240, 8, 49, 223, 132];
const CREATE_EVENT_DISC: [u8; 8] = [49, 142, 72, 166, 230, 29, 84, 84];
const CANCEL_EVENT_DISC: [u8; 8] = [174, 66, 141, 17, 4, 224, 162, 77];
const RENT: u64 = 2_039_280;

fn key(value: &str) -> Result<Pubkey> {
    value
        .parse()
        .map_err(|_| FinError::validation("A synthetic Trigger address is invalid."))
}

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

// Keep fields positional so this synthetic encoder stays auditable against
// the pinned Trigger account layout and its exact wire order.
#[allow(clippy::too_many_arguments)]
fn order_layout(
    wallet: &str,
    input_mint: &str,
    output_mint: &str,
    input_program: &str,
    output_program: &str,
    reserve: &str,
    unique_id: u64,
    making: u64,
    taking: u64,
    expired_at: Option<i64>,
) -> Result<Vec<u8>> {
    let mut bytes = ORDER_DISC.to_vec();
    for value in [
        wallet,
        input_mint,
        output_mint,
        input_program,
        output_program,
        reserve,
    ] {
        bytes.extend_from_slice(key(value)?.as_ref());
    }
    for value in [unique_id, making, taking, making, taking, 0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    if let Some(expired_at) = expired_at {
        bytes.push(1);
        bytes.extend_from_slice(&expired_at.to_le_bytes());
    } else {
        bytes.push(0);
    }
    bytes.extend_from_slice(&0u16.to_le_bytes()); // feeBps
    bytes.extend_from_slice(pda(&[b"fee"], &key(TRIGGER_PROGRAM)?).as_ref());
    bytes.extend_from_slice(&[0; 16]);
    bytes.push(0); // borrow metadata
    bytes.extend_from_slice(&0u16.to_le_bytes()); // slippageBps
    Ok(bytes)
}

// This test-only builder takes the complete signed order tuple explicitly;
// bundling it would obscure the relationship to the serialized instruction.
#[allow(clippy::too_many_arguments)]
fn order_transaction(
    rpc: &FixtureRpc,
    wallet: &str,
    input_mint: &str,
    output_mint: &str,
    making: u64,
    taking: u64,
    unique_id: u64,
    expired_at: Option<i64>,
) -> Result<(String, String)> {
    let wallet_key = key(wallet)?;
    let input = key(input_mint)?;
    let output = key(output_mint)?;
    let input_program = key(mint_profile(input_mint).0)?;
    let output_program = key(mint_profile(output_mint).0)?;
    let trigger = key(TRIGGER_PROGRAM)?;
    let order = pda(
        &[b"order", wallet_key.as_ref(), &unique_id.to_le_bytes()],
        &trigger,
    );
    let associated = key(ASSOCIATED_TOKEN_PROGRAM)?;
    let reserve = pda(
        &[order.as_ref(), input_program.as_ref(), input.as_ref()],
        &associated,
    );
    let maker_ata =
        fin_solana::associated_token_address(wallet, input_mint, mint_profile(input_mint).0)?;
    let fee = pda(&[b"fee"], &trigger);
    let event_authority = pda(&[b"__event_authority"], &trigger);
    let system = key("11111111111111111111111111111111")?;
    let instruction = Instruction {
        program_id: trigger,
        accounts: vec![
            AccountMeta::new(wallet_key, true),
            AccountMeta::new_readonly(wallet_key, true),
            AccountMeta::new(order, false),
            AccountMeta::new(reserve, false),
            AccountMeta::new(
                maker_ata
                    .parse()
                    .map_err(|_| FinError::validation("The synthetic ATA is invalid."))?,
                false,
            ),
            AccountMeta::new_readonly(fee, false),
            AccountMeta::new_readonly(trigger, false), // absent optional referral sentinel
            AccountMeta::new_readonly(input, false),
            AccountMeta::new_readonly(output, false),
            AccountMeta::new_readonly(input_program, false),
            AccountMeta::new_readonly(output_program, false),
            AccountMeta::new_readonly(system, false),
            AccountMeta::new_readonly(associated, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(trigger, false),
        ],
        data: {
            let mut data = INITIALIZE_DISC.to_vec();
            data.extend_from_slice(&unique_id.to_le_bytes());
            data.extend_from_slice(&making.to_le_bytes());
            data.extend_from_slice(&taking.to_le_bytes());
            if let Some(expired_at) = expired_at {
                data.push(1);
                data.extend_from_slice(&expired_at.to_le_bytes());
            } else {
                data.push(0);
            }
            data.extend_from_slice(&[0, 0]); // fee and slippage are None
            data
        },
    };
    let blockhash = rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[instruction], Some(&wallet_key), &blockhash);
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(message),
    };
    let wire = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .serialize(&transaction)
        .map_err(|_| FinError::validation("The synthetic Trigger transaction is invalid."))?;
    Ok((STANDARD.encode(wire), order.to_string()))
}

fn cancel_transaction(rpc: &FixtureRpc, wallet: &str, order: &str, bytes: &[u8]) -> Result<String> {
    let wallet_key = key(wallet)?;
    let order_key = key(order)?;
    let program = key(TRIGGER_PROGRAM)?;
    let input_mint = read_key(bytes, 8 + 32)?;
    let input_program = read_key(bytes, 8 + 32 * 3)?;
    let reserve = read_key(bytes, 8 + 32 * 5)?;
    let maker_ata = fin_solana::associated_token_address(wallet, &input_mint, &input_program)?;
    let event_authority = pda(&[b"__event_authority"], &program);
    let instruction = Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(wallet_key, true),
            AccountMeta::new(wallet_key, false),
            AccountMeta::new(order_key, false),
            AccountMeta::new(key(&reserve)?, false),
            AccountMeta::new(
                maker_ata
                    .parse()
                    .map_err(|_| FinError::validation("The synthetic ATA is invalid."))?,
                false,
            ),
            AccountMeta::new_readonly(key(&input_mint)?, false),
            AccountMeta::new_readonly(key(&input_program)?, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program, false),
        ],
        data: CANCEL_DISC.to_vec(),
    };
    let blockhash = rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[instruction], Some(&wallet_key), &blockhash);
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(message),
    };
    let wire = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .serialize(&transaction)
        .map_err(|_| FinError::validation("The synthetic Trigger transaction is invalid."))?;
    Ok(STANDARD.encode(wire))
}

fn read_key(bytes: &[u8], offset: usize) -> Result<String> {
    let slice = bytes
        .get(offset..offset + 32)
        .ok_or_else(|| FinError::validation("The synthetic Trigger order data is malformed."))?;
    Ok(bs58::encode(slice).into_string())
}

// The explicit positional fields mirror the pinned finalized CreateEvent
// layout used by the fixture's independent proof path.
#[allow(clippy::too_many_arguments)]
fn create_event(
    order: &str,
    wallet: &str,
    input_mint: &str,
    output_mint: &str,
    input_program: &str,
    output_program: &str,
    making: u64,
    taking: u64,
    expired_at: Option<i64>,
) -> Result<Vec<u8>> {
    let trigger = key(TRIGGER_PROGRAM)?;
    let fee = pda(&[b"fee"], &trigger);
    let mut data = CREATE_EVENT_DISC.to_vec();
    for address in [
        order,
        wallet,
        input_mint,
        output_mint,
        input_program,
        output_program,
    ] {
        data.extend_from_slice(key(address)?.as_ref());
    }
    data.extend_from_slice(&making.to_le_bytes());
    data.extend_from_slice(&taking.to_le_bytes());
    if let Some(expired_at) = expired_at {
        data.push(1);
        data.extend_from_slice(&expired_at.to_le_bytes());
    } else {
        data.push(0);
    }
    data.extend_from_slice(&0u16.to_le_bytes()); // fee bps
    data.extend_from_slice(fee.as_ref());
    data.extend_from_slice(&0u16.to_le_bytes()); // slippage bps
    Ok(data)
}

pub(super) async fn rpc_request(
    rpc: &FixtureRpc,
    method: &str,
    params: &Value,
) -> Result<Option<Value>> {
    if method == "getSignaturesForAddress" {
        let address = params
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_str)
            .ok_or_else(fixture_state_error)?;
        fin_solana::parse_address(address)?;
        let state = rpc.0.lock().map_err(|_| fixture_state_error())?;
        let rows = state
            .transactions
            .iter()
            .rev()
            .filter(|transaction| lifecycle::transaction_mentions_address(transaction, address))
            .map(|transaction| {
                json!({"signature":transaction.signature,"confirmationStatus":"finalized","slot":100})
            })
            .collect::<Vec<_>>();
        return Ok(Some(Value::Array(rows)));
    }
    if method != "getAccountInfo" {
        return Ok(None);
    }
    let address = params
        .as_array()
        .and_then(|params| params.first())
        .and_then(Value::as_str)
        .unwrap_or_default();
    if fin_solana::parse_address(address).is_ok() {
        lifecycle::apply_fill_control(rpc, address)?;
    }
    let state = rpc.0.lock().unwrap();
    let Some((owner, data)) = state.account_data.get(address) else {
        return Ok(None);
    };
    if owner != TRIGGER_PROGRAM {
        return Ok(None);
    }
    Ok(Some(
        json!({"context":{"slot":100},"value":{"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":RENT}}),
    ))
}

pub(super) fn submit(rpc: &FixtureRpc, encoded: &str) -> Result<Option<String>> {
    if encoded.len() > 256 * 1024 {
        return Ok(None);
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| FinError::validation("The synthetic Trigger transaction is malformed."))?;
    let transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| FinError::validation("The synthetic Trigger transaction is malformed."))?;
    let keys = transaction
        .message
        .static_account_keys()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let Some(ix) = transaction.message.instructions().iter().find(|ix| {
        keys.get(usize::from(ix.program_id_index))
            .is_some_and(|program| program == TRIGGER_PROGRAM)
    }) else {
        return Ok(None);
    };
    let account = |position: usize| -> Result<String> {
        let index = usize::from(*ix.accounts.get(position).ok_or_else(|| {
            FinError::validation("The synthetic Trigger account list is malformed.")
        })?);
        keys.get(index).cloned().ok_or_else(|| {
            FinError::validation("The synthetic Trigger account index is malformed.")
        })
    };
    let signature = transaction
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| FinError::validation("The synthetic Trigger signature is missing."))?;
    let mut state = rpc.0.lock().unwrap();
    let mut pre_balances = vec![0u64; keys.len()];
    let mut post_balances = vec![0u64; keys.len()];
    let mut pre_token_balances = Vec::new();
    let mut post_token_balances = Vec::new();
    let mut logs = vec![format!("Program {TRIGGER_PROGRAM} invoke [1]")];
    let fee = 5_000u64;
    let wallet_index = keys
        .iter()
        .position(|key| key == &state.wallet)
        .ok_or_else(|| FinError::validation("The Trigger fixture omitted the wallet."))?;
    pre_balances[wallet_index] = state.lamports;

    if ix.data.starts_with(&INITIALIZE_DISC) {
        let wallet = account(1)?;
        let order = account(2)?;
        let reserve = account(3)?;
        let source = account(4)?;
        let input_mint = account(7)?;
        let output_mint = account(8)?;
        let input_program = account(9)?;
        let output_program = account(10)?;
        let unique_id = u64::from_le_bytes(
            ix.data
                .get(8..16)
                .ok_or_else(|| FinError::validation("The Trigger args are truncated."))?
                .try_into()
                .map_err(|_| FinError::validation("The Trigger args are truncated."))?,
        );
        let making = u64::from_le_bytes(
            ix.data
                .get(16..24)
                .ok_or_else(|| FinError::validation("The Trigger args are truncated."))?
                .try_into()
                .map_err(|_| FinError::validation("The Trigger args are truncated."))?,
        );
        let taking = u64::from_le_bytes(
            ix.data
                .get(24..32)
                .ok_or_else(|| FinError::validation("The Trigger args are truncated."))?
                .try_into()
                .map_err(|_| FinError::validation("The Trigger args are truncated."))?,
        );
        let expired_at = match ix.data.get(32).copied() {
            Some(0) => None,
            Some(1) => Some(i64::from_le_bytes(
                ix.data
                    .get(33..41)
                    .ok_or_else(|| FinError::validation("The Trigger expiry is truncated."))?
                    .try_into()
                    .map_err(|_| FinError::validation("The Trigger expiry is malformed."))?,
            )),
            _ => {
                return Err(FinError::validation(
                    "The synthetic Trigger expiry option is malformed.",
                ));
            }
        };
        if wallet != state.wallet
            || state.account_data.contains_key(&order)
            || state.account_data.contains_key(&reserve)
        {
            return Err(FinError::validation(
                "The synthetic Trigger order identity is already used.",
            ));
        }
        let source_before = state
            .token_amounts
            .get(&source)
            .copied()
            .ok_or_else(|| FinError::validation("The Trigger input account is absent."))?;
        let source_after = source_before.checked_sub(making).ok_or_else(|| {
            FinError::validation("The Trigger wallet token balance is insufficient.")
        })?;
        let order_index = keys
            .iter()
            .position(|key| key == &order)
            .ok_or_else(|| FinError::validation("The order PDA is missing from the message."))?;
        let reserve_index = keys
            .iter()
            .position(|key| key == &reserve)
            .ok_or_else(|| FinError::validation("The reserve PDA is missing from the message."))?;
        let source_index = keys
            .iter()
            .position(|key| key == &source)
            .ok_or_else(|| FinError::validation("The input ATA is missing from the message."))?;
        let wallet_after = state.lamports.checked_sub(RENT * 2 + fee).ok_or_else(|| {
            FinError::validation("The fixture wallet lacks rent for Trigger accounts.")
        })?;
        pre_balances[order_index] = 0;
        post_balances[order_index] = RENT;
        pre_balances[reserve_index] = 0;
        post_balances[reserve_index] = RENT;
        post_balances[wallet_index] = wallet_after;
        let decimals = mint_profile(&input_mint).1;
        pre_token_balances.push(token_balance(
            source_index,
            &input_mint,
            &wallet,
            &input_program,
            decimals,
            source_before,
        ));
        post_token_balances.push(token_balance(
            source_index,
            &input_mint,
            &wallet,
            &input_program,
            decimals,
            source_after,
        ));
        pre_token_balances.push(token_balance(
            reserve_index,
            &input_mint,
            &order,
            &input_program,
            decimals,
            0,
        ));
        post_token_balances.push(token_balance(
            reserve_index,
            &input_mint,
            &order,
            &input_program,
            decimals,
            making,
        ));
        let order_data = order_layout(
            &wallet,
            &input_mint,
            &output_mint,
            &input_program,
            &output_program,
            &reserve,
            unique_id,
            making,
            taking,
            expired_at,
        )?;
        state
            .account_data
            .insert(order.clone(), (TRIGGER_PROGRAM.into(), order_data));
        state.account_data.insert(
            reserve.clone(),
            (
                input_program.clone(),
                token_account_data(&input_mint, &order, making),
            ),
        );
        state.token_amounts.insert(source.clone(), source_after);
        state.token_amounts.insert(reserve.clone(), making);
        let event = create_event(
            &order,
            &wallet,
            &input_mint,
            &output_mint,
            &input_program,
            &output_program,
            making,
            taking,
            expired_at,
        )?;
        logs.push(format!("Program data: {}", STANDARD.encode(event)));
        state.lamports = wallet_after;
    } else if ix.data == CANCEL_DISC {
        let order = account(2)?;
        let reserve = account(3)?;
        let destination = account(4)?;
        let (owner, order_data) = state
            .account_data
            .get(&order)
            .cloned()
            .ok_or_else(|| FinError::validation("The Trigger order is absent."))?;
        if owner != TRIGGER_PROGRAM {
            return Err(FinError::validation(
                "The Trigger order has the wrong owner.",
            ));
        }
        let input_mint = read_key(&order_data, 8 + 32)?;
        let input_program = read_key(&order_data, 8 + 32 * 3)?;
        let amount = u64::from_le_bytes(
            order_data
                .get(8 + 32 * 6 + 8 * 3..8 + 32 * 6 + 8 * 4)
                .ok_or_else(|| FinError::validation("The Trigger order data is truncated."))?
                .try_into()
                .map_err(|_| FinError::validation("The Trigger order data is truncated."))?,
        );
        let destination_before = state
            .token_amounts
            .get(&destination)
            .copied()
            .ok_or_else(|| FinError::validation("The cancellation ATA is absent."))?;
        let destination_after = destination_before
            .checked_add(amount)
            .ok_or_else(|| FinError::validation("The cancellation return overflows."))?;
        let reserve_index = keys
            .iter()
            .position(|key| key == &reserve)
            .ok_or_else(|| FinError::validation("The reserve PDA is missing from the message."))?;
        let destination_index = keys
            .iter()
            .position(|key| key == &destination)
            .ok_or_else(|| FinError::validation("The wallet ATA is missing from the message."))?;
        let order_index = keys
            .iter()
            .position(|key| key == &order)
            .ok_or_else(|| FinError::validation("The order PDA is missing from the message."))?;
        let wallet_after = state
            .lamports
            .checked_add(RENT * 2)
            .and_then(|value| value.checked_sub(fee))
            .ok_or_else(|| FinError::validation("The cancellation rent return overflows."))?;
        pre_balances[order_index] = RENT;
        // Token atoms are represented only in pre/post token balances; the
        // reserve account itself holds its fixed rent-exempt lamports.
        pre_balances[reserve_index] = RENT;
        post_balances[wallet_index] = wallet_after;
        let decimals = mint_profile(&input_mint).1;
        pre_token_balances.push(token_balance(
            reserve_index,
            &input_mint,
            &order,
            &input_program,
            decimals,
            amount,
        ));
        post_token_balances.push(token_balance(
            reserve_index,
            &input_mint,
            &order,
            &input_program,
            decimals,
            0,
        ));
        pre_token_balances.push(token_balance(
            destination_index,
            &input_mint,
            &state.wallet,
            &input_program,
            decimals,
            destination_before,
        ));
        post_token_balances.push(token_balance(
            destination_index,
            &input_mint,
            &state.wallet,
            &input_program,
            decimals,
            destination_after,
        ));
        state.account_data.remove(&order);
        state.account_data.remove(&reserve);
        state.token_amounts.remove(&reserve);
        state.token_amounts.insert(destination, destination_after);
        state.lamports = wallet_after;
        let mut event = CANCEL_EVENT_DISC.to_vec();
        event.extend_from_slice(key(&order)?.as_ref());
        logs.push(format!("Program data: {}", STANDARD.encode(event)));
    } else {
        return Ok(None);
    }
    state.transactions.push(StoredTransaction {
        signature: signature.clone(),
        wire: encoded.to_owned(),
        pre_balances,
        post_balances,
        pre_token_balances,
        post_token_balances,
        logs: {
            logs.push(format!("Program {TRIGGER_PROGRAM} success"));
            logs
        },
    });
    Ok(Some(rpc.record_submission(&mut state, signature)?))
}

pub(super) async fn api_request(
    api: &FixtureJupiterApi,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Option<Value>> {
    if method == Method::POST && path == "/trigger/v1/execute" {
        let body = body.ok_or_else(|| {
            FinError::validation("The synthetic Trigger execute body is missing.")
        })?;
        let request_id = body
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("The synthetic Trigger request ID is missing."))?;
        let encoded = body
            .get("signedTransaction")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                FinError::validation("The synthetic Trigger signed transaction is missing.")
            })?;
        lifecycle::validate_execute_request(request_id, encoded)?;
        let signature = api.rpc.submit(encoded)?;
        return Ok(Some(json!({"status":"Success","signature":signature})));
    }
    if method == Method::GET && path.starts_with("/trigger/v1/getTriggerOrders?") {
        lifecycle::seed_external_order(&api.rpc, &api.wallet)?;
        let state = api.rpc.0.lock().unwrap();
        let orders = state
            .account_data
            .iter()
            .filter_map(|(order, (owner, data))| {
                if owner != TRIGGER_PROGRAM || !data.starts_with(&ORDER_DISC) {
                    return None;
                }
                let maker = read_key(data, 8).ok()?;
                if maker != api.wallet {
                    return None;
                }
                let input = read_key(data, 8 + 32).ok()?;
                let output = read_key(data, 8 + 64).ok()?;
                let making = u64::from_le_bytes(
                    data.get(8 + 32 * 6 + 8 * 3..8 + 32 * 6 + 8 * 4)?
                        .try_into()
                        .ok()?,
                );
                let taking = u64::from_le_bytes(
                    data.get(8 + 32 * 6 + 8 * 4..8 + 32 * 6 + 8 * 5)?
                        .try_into()
                        .ok()?,
                );
                Some(json!({"orderKey":order,"inputMint":input,"outputMint":output,"rawMakingAmount":making.to_string(),"rawTakingAmount":taking.to_string(),"rawRemainingMakingAmount":making.to_string(),"rawRemainingTakingAmount":taking.to_string(),"status":"active"}))
            })
            .collect::<Vec<_>>();
        return Ok(Some(
            json!({"user":api.wallet,"orders":orders,"totalPages":1}),
        ));
    }
    if method != Method::POST
        || !matches!(path, "/trigger/v1/createOrder" | "/trigger/v1/cancelOrder")
    {
        return Ok(None);
    }
    let body =
        body.ok_or_else(|| FinError::validation("The synthetic Trigger request is missing."))?;
    let request_id = lifecycle::next_execute_request_id();
    if path.ends_with("createOrder") {
        let input_mint = body
            .get("inputMint")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("The synthetic Trigger input mint is missing."))?;
        let output_mint = body
            .get("outputMint")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("The synthetic Trigger output mint is missing."))?;
        let making = body
            .pointer("/params/makingAmount")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| {
                FinError::validation("The synthetic Trigger making amount is invalid.")
            })?;
        let taking = body
            .pointer("/params/takingAmount")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| {
                FinError::validation("The synthetic Trigger taking amount is invalid.")
            })?;
        let unique_id = api.rpc.0.lock().unwrap().transactions.len() as u64 + 77;
        let expired_at = body.pointer("/params/expiredAt").and_then(Value::as_i64);
        let (transaction, order) = order_transaction(
            &api.rpc,
            &api.wallet,
            input_mint,
            output_mint,
            making,
            taking,
            unique_id,
            expired_at,
        )?;
        lifecycle::bind_execute_request(&request_id, &transaction)?;
        Ok(Some(
            json!({"transaction":transaction,"requestId":request_id,"order":order}),
        ))
    } else {
        let order = body
            .get("order")
            .and_then(Value::as_str)
            .ok_or_else(|| FinError::validation("The synthetic Trigger order key is missing."))?;
        let data = {
            let state = api.rpc.0.lock().unwrap();
            let (owner, data) = state
                .account_data
                .get(order)
                .ok_or_else(|| FinError::validation("The synthetic Trigger order is absent."))?;
            if owner != TRIGGER_PROGRAM {
                return Err(FinError::validation(
                    "The synthetic Trigger account has the wrong owner.",
                ));
            }
            data.clone()
        };
        if data.is_empty() {
            return Err(FinError::validation(
                "The synthetic Trigger order account is empty.",
            ));
        }
        let transaction = cancel_transaction(&api.rpc, &api.wallet, order, &data)?;
        lifecycle::bind_execute_request(&request_id, &transaction)?;
        Ok(Some(
            json!({"transaction":transaction,"requestId":request_id,"order":order}),
        ))
    }
}
