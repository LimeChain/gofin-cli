//! Synthetic Meteora pool fixture for public CLI rehearsals.
//!
//! The transport still executes the real GoFin adapter, candidate validation,
//! unsigned simulations, signing, journal, finalized proof decoder, and
//! settlement. Only RPC accounts and the protocol's simulated ledger are
//! controlled here; no network client or live signer is used.
use super::*;

pub(super) const DLMM_POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
pub(super) const DAMM_POOL: &str = "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie";
const DLMM_DISC: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
const DAMM_DISC: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
const BIN_ARRAY_DISC: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
const DLMM_SWAP_EVENT: [u8; 8] = [46, 116, 82, 215, 148, 27, 84, 77];
const DLMM_LEGACY_SWAP_EVENT: [u8; 8] = [81, 108, 227, 190, 205, 208, 10, 196];
const ANCHOR_EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
const DAMM_PROGRAM: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";
const DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const DLMM_EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const TOKEN: &str = gofin_solana::token::TOKEN_PROGRAM_ID;
const RENT: u64 = 2_039_280;
const LAMPORTS_PER_SIGNATURE: u64 = 5_000;

fn fixture_transaction_fee(transaction: &VersionedTransaction) -> Result<u64> {
    u64::try_from(transaction.signatures.len())
        .ok()
        .and_then(|count| count.checked_mul(LAMPORTS_PER_SIGNATURE))
        .ok_or_else(fixture_state_error)
}

fn key(seed: u8) -> String {
    solana_pubkey::Pubkey::new_from_array([seed; 32]).to_string()
}

fn put_key(data: &mut [u8], offset: usize, address: &str) {
    let bytes = bs58::decode(address)
        .into_vec()
        .expect("synthetic public key");
    data[offset..offset + 32].copy_from_slice(&bytes);
}

fn dlmm_event_cpi(parent_index: usize, event: &[u8]) -> Result<Value> {
    let program = DLMM_PROGRAM
        .parse::<solana_pubkey::Pubkey>()
        .map_err(|_| fixture_state_error())?;
    let (authority, _) =
        solana_pubkey::Pubkey::find_program_address(&[DLMM_EVENT_AUTHORITY_SEED], &program);
    let mut data = ANCHOR_EVENT_IX_TAG.to_vec();
    data.extend_from_slice(event);
    Ok(json!({
        "index": parent_index,
        "instructions": [{
            "programId": DLMM_PROGRAM,
            "accounts": [authority.to_string()],
            "data": bs58::encode(data).into_string()
        }]
    }))
}

fn dlmm_legacy_swap_event(swap2: &[u8]) -> Result<Vec<u8>> {
    if swap2.len() != 155 || swap2[..8] != DLMM_SWAP_EVENT {
        return Err(fixture_state_error());
    }
    let mut event = vec![0; 137];
    event[..8].copy_from_slice(&DLMM_LEGACY_SWAP_EVENT);
    event[8..80].copy_from_slice(&swap2[8..80]);
    event[80..88].copy_from_slice(&swap2[97..105]);
    event[88..96].copy_from_slice(&swap2[113..121]);
    event[96] = swap2[80];
    event[97..105].copy_from_slice(&0u64.to_le_bytes());
    event[105..113].copy_from_slice(&swap2[129..137]);
    event[113..129].copy_from_slice(&swap2[81..97]);
    event[129..137].copy_from_slice(&swap2[145..153]);
    Ok(event)
}

fn account_data(mint: &str, owner: &str, amount: u64) -> Vec<u8> {
    let mut bytes = vec![0; 165];
    put_key(&mut bytes, 0, mint);
    put_key(&mut bytes, 32, owner);
    bytes[64..72].copy_from_slice(&amount.to_le_bytes());
    bytes[108] = 1;
    bytes
}

fn program_account(data: &[u8], owner: &str, address: &str) -> (String, Vec<u8>) {
    let _ = address;
    (owner.to_owned(), data.to_vec())
}

pub(super) fn initialize(state: &mut FixtureState) -> Result<()> {
    let wallet = state.wallet.clone();
    let vault_sol = key(31);
    let vault_usdc = key(32);
    let reserve_sol = key(33);
    let reserve_usdc = key(34);
    let dlmm_bin = bin_array(&DLMM_POOL.parse().expect("fixed pool"), 0)?;

    let mut pair = vec![0; 8 + 576];
    pair[..8].copy_from_slice(&DLMM_DISC);
    let d = &mut pair[8..];
    d[68..72].copy_from_slice(&0i32.to_le_bytes());
    d[72..74].copy_from_slice(&25u16.to_le_bytes());
    d[74] = 0;
    put_key(d, 80, SOL_MINT);
    put_key(d, 112, USDC_MINT);
    put_key(d, 144, &reserve_sol);
    put_key(d, 176, &reserve_usdc);
    state.account_data.insert(
        DLMM_POOL.to_owned(),
        program_account(&pair, DLMM_PROGRAM, DLMM_POOL),
    );

    let mut array = vec![0; 8 + 40 + 70 * 128];
    array[..8].copy_from_slice(&BIN_ARRAY_DISC);
    let body = &mut array[8..];
    body[..8].copy_from_slice(&0i64.to_le_bytes());
    put_key(body, 8, DLMM_POOL);
    let bin = 40;
    body[bin..bin + 8].copy_from_slice(&1_000_000_000u64.to_le_bytes());
    body[bin + 8..bin + 16].copy_from_slice(&1_000_000_000u64.to_le_bytes());
    body[bin + 16..bin + 32].copy_from_slice(&(1u128 << 64).to_le_bytes());
    body[bin + 32..bin + 48].copy_from_slice(&1_000_000_000u128.to_le_bytes());
    state.account_data.insert(
        dlmm_bin.clone(),
        program_account(&array, DLMM_PROGRAM, &dlmm_bin),
    );
    state.account_data.insert(
        reserve_sol.clone(),
        (
            TOKEN.to_owned(),
            account_data(SOL_MINT, DLMM_POOL, 1_000_000_000),
        ),
    );
    state.account_data.insert(
        reserve_usdc.clone(),
        (
            TOKEN.to_owned(),
            account_data(USDC_MINT, DLMM_POOL, 1_000_000_000),
        ),
    );

    let mut pool = vec![0; 1112];
    pool[..8].copy_from_slice(&DAMM_DISC);
    let d = &mut pool[8..];
    put_key(d, 160, SOL_MINT);
    put_key(d, 192, USDC_MINT);
    put_key(d, 224, &vault_sol);
    put_key(d, 256, &vault_usdc);
    d[352..368].copy_from_slice(&1_000_000_000u128.to_le_bytes());
    let q64 = 1u128 << 64;
    d[416..432].copy_from_slice(&(q64 / 2).to_le_bytes());
    d[432..448].copy_from_slice(&(q64 * 2).to_le_bytes());
    d[448..464].copy_from_slice(&q64.to_le_bytes());
    d[472] = 0;
    d[473] = 0;
    d[476] = 0;
    d[477] = 1; // compounding pool: reserve-ratio model
    d[478] = 0;
    d[672..680].copy_from_slice(&1_000_000_000u64.to_le_bytes());
    d[680..688].copy_from_slice(&1_000_000_000u64.to_le_bytes());
    for (address, mint) in [(&vault_sol, SOL_MINT), (&vault_usdc, USDC_MINT)] {
        state.account_data.insert(
            address.clone(),
            (
                TOKEN.to_owned(),
                account_data(mint, DAMM_POOL, 1_000_000_000),
            ),
        );
    }
    state.account_data.insert(
        DAMM_POOL.to_owned(),
        program_account(&pool, DAMM_PROGRAM, DAMM_POOL),
    );

    // Most rehearsal runs start with 100 USDC. The public named-plan test can
    // start from zero to prove its second leg is funded only by a finalized
    // predecessor credit. This override is confined to the development fixture.
    let initial_usdc = std::env::var("GOFIN_DEV_METEORA_REHEARSAL_INITIAL_USDC_ATOMS")
        .map(|value| value.parse::<u64>().map_err(|_| fixture_state_error()))
        .unwrap_or(Ok(100_000_000))?;
    for (mint, amount) in [(SOL_MINT, 0), (USDC_MINT, initial_usdc)] {
        let ata = gofin_solana::associated_token_address(&wallet, mint, TOKEN)?;
        state.account_data.insert(
            ata.clone(),
            (TOKEN.into(), account_data(mint, &wallet, amount)),
        );
        state.token_amounts.insert(ata, amount);
    }
    Ok(())
}

fn bin_array(pair: &solana_pubkey::Pubkey, index: i64) -> Result<String> {
    let program = DLMM_PROGRAM
        .parse::<solana_pubkey::Pubkey>()
        .map_err(|_| fixture_state_error())?;
    let (pda, _) = solana_pubkey::Pubkey::find_program_address(
        &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
        &program,
    );
    Ok(pda.to_string())
}

fn rpc_account(state: &FixtureState, address: &str) -> Value {
    if address == state.wallet {
        return json!({"owner":"11111111111111111111111111111111","data":[STANDARD.encode([]),"base64"],"lamports":state.lamports,"executable":false,"rentEpoch":0});
    }
    let Some((owner, raw)) = state.account_data.get(address) else {
        return Value::Null;
    };
    let mut data = raw.clone();
    let amount = state.token_amounts.get(address).copied();
    if let Some(amount) = amount {
        data[64..72].copy_from_slice(&amount.to_le_bytes());
    }
    let lamports = if amount.is_some()
        && account_mint(&state.account_data, address).ok().as_deref() == Some(SOL_MINT)
    {
        RENT + amount.unwrap_or(0)
    } else {
        RENT
    };
    json!({"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":lamports,"executable":false,"rentEpoch":0})
}

pub(super) fn simulate(_rpc: &FixtureRpc, params: &Value) -> Result<Option<Value>> {
    if std::env::var_os("GOFIN_DEV_METEORA_REHEARSAL").is_none() {
        return Ok(None);
    }
    let args = params.as_array().ok_or_else(fixture_state_error)?;
    let encoded = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(fixture_state_error)?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| fixture_state_error())?;
    let tx: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| fixture_state_error())?;
    let transaction_fee = fixture_transaction_fee(&tx)?;
    let keys = tx.message.static_account_keys();
    let native_temp = tx.message.instructions().iter().find_map(|ix| {
        let program = keys.get(usize::from(ix.program_id_index))?.to_string();
        if program != "11111111111111111111111111111111"
            || ix.data.len() != 52
            || ix.data.get(..4) != Some(&[0, 0, 0, 0])
        {
            return None;
        }
        ix.accounts
            .get(1)
            .and_then(|index| keys.get(usize::from(*index)))
            .map(ToString::to_string)
    });
    let create_lamports = tx.message.instructions().iter().find_map(|ix| {
        let program = keys.get(usize::from(ix.program_id_index))?.to_string();
        if program == "11111111111111111111111111111111"
            && ix.data.len() == 52
            && ix.data.get(..4) == Some(&[0, 0, 0, 0])
        {
            ix.data
                .get(4..12)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_le_bytes)
        } else {
            None
        }
    });
    let instruction = tx.message.instructions().iter().find_map(|ix| {
        let program = keys.get(usize::from(ix.program_id_index))?.to_string();
        matches!(program.as_str(), DAMM_PROGRAM | DLMM_PROGRAM).then_some((program, ix))
    });
    let addresses = args
        .get(1)
        .and_then(|v| v.get("accounts"))
        .and_then(|v| v.get("addresses"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let state = _rpc.0.lock().map_err(|_| fixture_state_error())?;
    let mut overlay = state.account_data.clone();
    let mut amounts = state.token_amounts.clone();
    let mut logs = Vec::new();
    let mut inner_instructions = Vec::new();
    let mut native_cash_delta = 0i128;
    if let Some((program, ix)) = instruction.as_ref() {
        let (source_index, destination_index, amount_offset) = if program == DAMM_PROGRAM {
            (2, 3, 8)
        } else {
            (4, 5, 8)
        };
        let source = ix
            .accounts
            .get(source_index)
            .and_then(|i| keys.get(usize::from(*i)))
            .ok_or_else(fixture_state_error)?
            .to_string();
        let dest = ix
            .accounts
            .get(destination_index)
            .and_then(|i| keys.get(usize::from(*i)))
            .ok_or_else(fixture_state_error)?
            .to_string();
        let amount = u64::from_le_bytes(
            ix.data
                .get(amount_offset..amount_offset + 8)
                .ok_or_else(fixture_state_error)?
                .try_into()
                .map_err(|_| fixture_state_error())?,
        );
        let native_input = native_temp.as_deref() == Some(source.as_str());
        let native_output = native_temp.as_deref() == Some(dest.as_str());
        if let Some(temp) = native_temp.as_deref() {
            let initial = if native_input {
                create_lamports.unwrap_or(RENT).saturating_sub(RENT)
            } else {
                0
            };
            overlay.insert(
                temp.to_owned(),
                (
                    TOKEN.to_owned(),
                    account_data(SOL_MINT, &state.wallet, initial),
                ),
            );
            amounts.insert(temp.to_owned(), initial);
        }
        let source_mint = if native_input {
            SOL_MINT.to_owned()
        } else {
            account_mint(&overlay, &source)?
        };
        let destination_mint = if native_output {
            SOL_MINT.to_owned()
        } else {
            account_mint(&overlay, &dest)?
        };
        let is_usdc_to_sol = source_mint == USDC_MINT && destination_mint == SOL_MINT;
        let output = if is_usdc_to_sol {
            amount.saturating_mul(1000)
        } else {
            amount / 1000
        };
        if native_input {
            native_cash_delta -= i128::from(amount);
        }
        if native_output {
            native_cash_delta += i128::from(output);
        }
        let in_before = amounts.get(&source).copied().unwrap_or(0);
        let out_before = amounts.get(&dest).copied().unwrap_or(0);
        amounts.insert(source.clone(), in_before.saturating_sub(amount));
        amounts.insert(dest.clone(), out_before.saturating_add(output));
        for (address, value) in [
            (&source, in_before.saturating_sub(amount)),
            (&dest, out_before.saturating_add(output)),
        ] {
            if let Some((_, data)) = overlay.get_mut(address) {
                data[64..72].copy_from_slice(&value.to_le_bytes());
            }
        }
        if program == DLMM_PROGRAM {
            let pool_ix = 0;
            let pool = ix
                .accounts
                .get(pool_ix)
                .and_then(|i| keys.get(usize::from(*i)))
                .ok_or_else(fixture_state_error)?
                .to_bytes();
            let owner = keys.first().ok_or_else(fixture_state_error)?.to_bytes();
            let mut event = vec![0u8; 155];
            event[..8].copy_from_slice(&DLMM_SWAP_EVENT);
            event[8..40].copy_from_slice(&pool);
            event[40..72].copy_from_slice(&owner);
            event[97..105].copy_from_slice(&amount.to_le_bytes());
            event[113..121].copy_from_slice(&output.to_le_bytes());
            let legacy_event = dlmm_legacy_swap_event(&event)?;
            logs.push(format!("Program {DLMM_PROGRAM} invoke [1]"));
            logs.push(format!("Program data: {}", STANDARD.encode(&event)));
            logs.push(format!("Program data: {}", STANDARD.encode(&legacy_event)));
            logs.push(format!("Program {DLMM_PROGRAM} success"));
            let parent_index = tx
                .message
                .instructions()
                .iter()
                .position(|candidate| {
                    candidate.program_id_index == ix.program_id_index
                        && candidate.accounts == ix.accounts
                        && candidate.data == ix.data
                })
                .ok_or_else(fixture_state_error)?;
            inner_instructions.push(dlmm_event_cpi(parent_index, &event)?);
            inner_instructions.push(dlmm_event_cpi(parent_index, &legacy_event)?);
        }
    }
    let simulated_wallet_lamports = (i128::from(state.lamports) + native_cash_delta
        - i128::from(transaction_fee))
    .max(0) as u64;
    let rows = addresses.iter().map(|value| {
        let Some(address)=value.as_str() else {return Value::Null};
        if native_temp.as_deref()==Some(address) { return Value::Null; }
        if address == state.wallet {
            let mut account=rpc_account(&state,address);
            account["lamports"]=json!(simulated_wallet_lamports);
            return account;
        }
        if let Some((owner,raw))=overlay.get(address) {
            let mut data=raw.clone();
            if let Some(amount)=amounts.get(address) { data[64..72].copy_from_slice(&amount.to_le_bytes()); }
            let lamports = if account_mint(&overlay,address).ok().as_deref() == Some(SOL_MINT) { RENT+amounts.get(address).copied().unwrap_or(0) } else { RENT };
            json!({"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":lamports,"executable":false,"rentEpoch":0})
        } else { Value::Null }
    }).collect::<Vec<_>>();
    Ok(Some(
        json!({"context":{"slot":100},"value":{"err":null,"unitsConsumed":100_000,"fee":transaction_fee,"accounts":rows,"logs":logs,"innerInstructions":inner_instructions}}),
    ))
}

pub(super) fn submit(rpc: &FixtureRpc, encoded: &str) -> Result<Option<String>> {
    if std::env::var_os("GOFIN_DEV_METEORA_REHEARSAL").is_none() {
        return Ok(None);
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| GoFinError::validation("The synthetic Meteora transaction is malformed."))?;
    let tx: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| GoFinError::validation("The synthetic Meteora transaction is malformed."))?;
    let transaction_fee = fixture_transaction_fee(&tx)?;
    let keys = tx.message.static_account_keys();
    let native_temp = tx.message.instructions().iter().find_map(|ix| {
        let program = keys.get(usize::from(ix.program_id_index))?.to_string();
        if program != "11111111111111111111111111111111"
            || ix.data.len() != 52
            || ix.data.get(..4) != Some(&[0, 0, 0, 0])
        {
            return None;
        }
        ix.accounts
            .get(1)
            .and_then(|index| keys.get(usize::from(*index)))
            .map(ToString::to_string)
    });
    let Some((program, ix)) = tx.message.instructions().iter().find_map(|ix| {
        let p = keys.get(usize::from(ix.program_id_index))?.to_string();
        matches!(p.as_str(), DAMM_PROGRAM | DLMM_PROGRAM).then_some((p, ix))
    }) else {
        return Ok(None);
    };
    let (si, di) = if program == DAMM_PROGRAM {
        (2, 3)
    } else {
        (4, 5)
    };
    let source = keys
        .get(usize::from(
            *ix.accounts.get(si).ok_or_else(fixture_state_error)?,
        ))
        .ok_or_else(fixture_state_error)?
        .to_string();
    let dest = keys
        .get(usize::from(
            *ix.accounts.get(di).ok_or_else(fixture_state_error)?,
        ))
        .ok_or_else(fixture_state_error)?
        .to_string();
    let amount = u64::from_le_bytes(
        ix.data
            .get(8..16)
            .ok_or_else(fixture_state_error)?
            .try_into()
            .map_err(|_| fixture_state_error())?,
    );
    let sig = tx
        .signatures
        .first()
        .ok_or_else(fixture_state_error)?
        .to_string();
    let mut state = rpc.0.lock().map_err(|_| fixture_state_error())?;
    let native_input = native_temp.as_deref() == Some(source.as_str());
    let native_output = native_temp.as_deref() == Some(dest.as_str());
    let source_mint = if native_input {
        SOL_MINT.to_owned()
    } else {
        account_mint(&state.account_data, &source)?
    };
    let dest_mint = if native_output {
        SOL_MINT.to_owned()
    } else {
        account_mint(&state.account_data, &dest)?
    };
    for (address, is_temporary) in [(&source, native_input), (&dest, native_output)] {
        if is_temporary {
            continue;
        }
        let account = state
            .account_data
            .get(address)
            .ok_or_else(fixture_state_error)?;
        if bs58::encode(&account.1[32..64]).into_string() != state.wallet {
            return Err(GoFinError::validation(
                "Synthetic Meteora swap accounts are not wallet-owned.",
            ));
        }
    }
    if native_input && native_output {
        return Err(GoFinError::validation(
            "The synthetic fixture cannot swap native SOL to itself.",
        ));
    }
    let is_usdc_to_sol = source_mint == USDC_MINT && dest_mint == SOL_MINT;
    let output = if is_usdc_to_sol {
        amount.saturating_mul(1000)
    } else {
        amount / 1000
    };
    let pre_source = if native_input {
        0
    } else {
        state.token_amounts.get(&source).copied().unwrap_or(0)
    };
    let pre_dest = if native_output {
        0
    } else {
        state.token_amounts.get(&dest).copied().unwrap_or(0)
    };
    let post_source = if native_input {
        0
    } else {
        pre_source.checked_sub(amount).ok_or_else(|| {
            GoFinError::validation("Synthetic wallet input balance is insufficient.")
        })?
    };
    let post_dest = if native_output {
        0
    } else {
        pre_dest
            .checked_add(output)
            .ok_or_else(fixture_state_error)?
    };
    if !native_input {
        state.token_amounts.insert(source.clone(), post_source);
        if let Some((_, data)) = state.account_data.get_mut(&source) {
            data[64..72].copy_from_slice(&post_source.to_le_bytes());
        }
    }
    if !native_output {
        state.token_amounts.insert(dest.clone(), post_dest);
        if let Some((_, data)) = state.account_data.get_mut(&dest) {
            data[64..72].copy_from_slice(&post_dest.to_le_bytes());
        }
    }
    let account_keys = tx.message.static_account_keys();
    let source_index = account_keys
        .iter()
        .position(|k| k.to_string() == source)
        .ok_or_else(fixture_state_error)?;
    let dest_index = account_keys
        .iter()
        .position(|k| k.to_string() == dest)
        .ok_or_else(fixture_state_error)?;
    let mut logs = Vec::new();
    let mut inner_instructions = Vec::new();
    if program == DLMM_PROGRAM {
        let pool = keys
            .get(usize::from(
                *ix.accounts.first().ok_or_else(fixture_state_error)?,
            ))
            .ok_or_else(fixture_state_error)?
            .to_bytes();
        let owner = keys.first().ok_or_else(fixture_state_error)?.to_bytes();
        let mut event = vec![0u8; 155];
        event[..8].copy_from_slice(&DLMM_SWAP_EVENT);
        event[8..40].copy_from_slice(&pool);
        event[40..72].copy_from_slice(&owner);
        event[97..105].copy_from_slice(&amount.to_le_bytes());
        event[113..121].copy_from_slice(&output.to_le_bytes());
        let legacy_event = dlmm_legacy_swap_event(&event)?;
        logs = vec![
            format!("Program {DLMM_PROGRAM} invoke [1]"),
            format!("Program data: {}", STANDARD.encode(&event)),
            format!("Program data: {}", STANDARD.encode(&legacy_event)),
            format!("Program {DLMM_PROGRAM} success"),
        ];
        let parent_index = tx
            .message
            .instructions()
            .iter()
            .position(|candidate| {
                candidate.program_id_index == ix.program_id_index
                    && candidate.accounts == ix.accounts
                    && candidate.data == ix.data
            })
            .ok_or_else(fixture_state_error)?;
        inner_instructions.push(dlmm_event_cpi(parent_index, &event)?);
        inner_instructions.push(dlmm_event_cpi(parent_index, &legacy_event)?);
    }
    let pre_wallet = state.lamports;
    let native_delta = if native_input {
        -i128::from(amount)
    } else if native_output {
        i128::from(output)
    } else {
        0
    };
    state.lamports = u64::try_from(
        (i128::from(state.lamports) + native_delta - i128::from(transaction_fee)).max(0),
    )
    .map_err(|_| fixture_state_error())?;
    let post_wallet = state.lamports;
    let mut pre = vec![pre_wallet; account_keys.len()];
    let mut post = vec![post_wallet; account_keys.len()];
    let wallet_index = account_keys
        .iter()
        .position(|k| k.to_string() == state.wallet)
        .ok_or_else(fixture_state_error)?;
    pre[wallet_index] = pre_wallet;
    post[wallet_index] = post_wallet;
    let decimals = |mint: &str| if mint == USDC_MINT { 6 } else { 9 };
    let stored = StoredTransaction {
        signature: sig.clone(),
        wire: encoded.into(),
        pre_balances: pre,
        post_balances: post,
        pre_token_balances: [
            (!native_input).then(|| {
                token_balance(
                    source_index,
                    &source_mint,
                    &state.wallet,
                    TOKEN,
                    decimals(&source_mint),
                    pre_source,
                )
            }),
            (!native_output).then(|| {
                token_balance(
                    dest_index,
                    &dest_mint,
                    &state.wallet,
                    TOKEN,
                    decimals(&dest_mint),
                    pre_dest,
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect(),
        post_token_balances: [
            (!native_input).then(|| {
                token_balance(
                    source_index,
                    &source_mint,
                    &state.wallet,
                    TOKEN,
                    decimals(&source_mint),
                    post_source,
                )
            }),
            (!native_output).then(|| {
                token_balance(
                    dest_index,
                    &dest_mint,
                    &state.wallet,
                    TOKEN,
                    decimals(&dest_mint),
                    post_dest,
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect(),
        logs,
        inner_instructions,
    };
    state.transactions.push(stored);
    let returned = rpc.record_submission(&mut state, sig)?;
    Ok(Some(returned))
}

pub(super) fn rpc_request(rpc: &FixtureRpc, method: &str, params: &Value) -> Result<Option<Value>> {
    if std::env::var_os("GOFIN_DEV_METEORA_REHEARSAL").is_none() {
        return Ok(None);
    }
    let state = rpc.0.lock().map_err(|_| fixture_state_error())?;
    let Some(args) = params.as_array() else {
        return Ok(None);
    };
    let result = match method {
        "getAccountInfo" => {
            let address = args.first().and_then(Value::as_str).unwrap_or_default();
            json!({"context":{"slot":100},"value":rpc_account(&state,address)})
        }
        "getMultipleAccounts" => {
            let Some(addresses) = args.first().and_then(Value::as_array) else {
                return Ok(None);
            };
            let rows = addresses
                .iter()
                .map(|address| {
                    address
                        .as_str()
                        .map(|address| rpc_account(&state, address))
                        .unwrap_or(Value::Null)
                })
                .collect::<Vec<_>>();
            json!({"context":{"slot":100},"value":rows})
        }
        "getProgramAccounts" => {
            let program = args.first().and_then(Value::as_str).unwrap_or_default();
            let config = args.get(1).cloned().unwrap_or(Value::Null);
            let filters = config
                .get("filters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let rows = state
                .account_data
                .iter()
                .filter_map(|(address, (owner, data))| {
                    if owner != program || !matches_filters(data, &filters) {
                        return None;
                    }
                    let mut data = data.clone();
                    if data.len() == 165
                        && let Some(amount) = state.token_amounts.get(address)
                    {
                        data[64..72].copy_from_slice(&amount.to_le_bytes());
                    }
                    Some(json!({"pubkey":address,"account":{"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":RENT}}))
                })
                .collect::<Vec<_>>();
            json!(rows)
        }
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn matches_filters(data: &[u8], filters: &[Value]) -> bool {
    filters.iter().all(|filter| {
        let Some(memcmp) = filter.get("memcmp") else {
            return true;
        };
        let Some(offset) = memcmp
            .get("offset")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
        else {
            return false;
        };
        let Some(expected) = memcmp.get("bytes").and_then(Value::as_str) else {
            return false;
        };
        let Ok(expected) = bs58::decode(expected).into_vec() else {
            return false;
        };
        data.get(offset..offset.saturating_add(expected.len())) == Some(expected.as_slice())
    })
}
