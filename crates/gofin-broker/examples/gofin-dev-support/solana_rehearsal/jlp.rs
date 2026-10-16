//! Synthetic, proof-backed JLP mint and redemption transactions.
use super::*;
use base64::engine::general_purpose::STANDARD;
use bincode::Options;
use gofin_domain::{GoFinError, Result};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solana_instruction::{AccountMeta, Instruction};
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;

const PROGRAM: &str = gofin_jupiter::jlp::PERPS_PROGRAM_ID;
const JLP_MINT: &str = gofin_jupiter::jlp::JLP_MINT;
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOL: &str = gofin_jupiter::swap::WRAPPED_SOL_MINT;
const TOKEN: &str = gofin_solana::token::TOKEN_PROGRAM_ID;
const SYSTEM: &str = "11111111111111111111111111111111";
const FEE: u64 = 5_000;
const RENT: u64 = 2_039_280;

fn fixture_error(message: &str) -> GoFinError {
    GoFinError::validation(message)
}

fn key(value: &str) -> Result<Pubkey> {
    value
        .parse()
        .map_err(|_| fixture_error("Synthetic JLP address is invalid."))
}

fn derive(seed: &[&[u8]]) -> Result<String> {
    Ok(Pubkey::find_program_address(seed, &key(PROGRAM)?)
        .0
        .to_string())
}

fn discr(kind: &str, name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("{kind}:{name}").as_bytes());
    digest[..8].try_into().expect("fixed digest prefix")
}

fn pool_address() -> Result<String> {
    derive(&[b"pool", b"Pool"])
}

fn custody_address(mint: &str) -> Result<String> {
    let pool = key(&pool_address()?)?;
    let mint = key(mint)?;
    derive(&[b"custody", pool.as_ref(), mint.as_ref()])
}

fn custody_token_account(custody: &str, mint: &str) -> Result<String> {
    gofin_solana::associated_token_address(custody, mint, TOKEN)
}

fn mint_data(decimals: u8) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    data[44] = decimals;
    data[45] = 1;
    data
}

fn anchor(name: &str) -> [u8; 8] {
    discr("global", name)
}

fn instruction(
    wallet: &str,
    operation: &str,
    input_mint: &str,
    output_mint: &str,
    input_atoms: u64,
    output_atoms: u64,
    blockhash: solana_hash::Hash,
) -> Result<(String, String, String)> {
    let wallet_key = key(wallet)?;
    let input_ata = gofin_solana::associated_token_address(wallet, input_mint, TOKEN)?;
    let output_ata = gofin_solana::associated_token_address(wallet, output_mint, TOKEN)?;
    let pool = pool_address()?;
    let custody_mint = if operation == "jupiter.jlp.mint" {
        input_mint
    } else {
        output_mint
    };
    let custody = custody_address(custody_mint)?;
    let custody_token = custody_token_account(&custody, custody_mint)?;
    let pool_key = key(&pool)?;
    let lp_mint = key(JLP_MINT)?;
    let transfer_authority = derive(&[b"transfer_authority"])?;
    let perpetuals = derive(&[b"perpetuals"])?;
    let event_authority = derive(&[b"__event_authority"])?;
    let program = key(PROGRAM)?;
    let token_program = key(TOKEN)?;
    let ix_name = if operation == "jupiter.jlp.mint" {
        "add_liquidity2"
    } else {
        "remove_liquidity2"
    };
    let (account_one, account_two) = if operation == "jupiter.jlp.mint" {
        (key(&input_ata)?, key(&output_ata)?)
    } else {
        (key(&output_ata)?, key(&input_ata)?)
    };
    let ix = Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new(wallet_key, true),
            AccountMeta::new(account_one, false),
            AccountMeta::new(account_two, false),
            AccountMeta::new_readonly(key(&transfer_authority)?, false),
            AccountMeta::new_readonly(key(&perpetuals)?, false),
            AccountMeta::new(pool_key, false),
            AccountMeta::new(key(&custody)?, false),
            AccountMeta::new_readonly(key(USDC)?, false),
            AccountMeta::new_readonly(key(SOL)?, false),
            AccountMeta::new(key(&custody_token)?, false),
            AccountMeta::new(lp_mint, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(key(&event_authority)?, false),
            AccountMeta::new_readonly(program, false),
        ],
        data: {
            let mut data = anchor(ix_name).to_vec();
            data.extend_from_slice(&input_atoms.to_le_bytes());
            data.extend_from_slice(&output_atoms.to_le_bytes());
            if ix_name == "add_liquidity2" {
                data.push(0);
            }
            data
        },
    };
    let mut instructions = Vec::new();
    if operation == "jupiter.jlp.mint" && input_mint == SOL {
        instructions.push(Instruction {
            program_id: key(SYSTEM)?,
            accounts: vec![
                AccountMeta::new(wallet_key, true),
                AccountMeta::new(key(&input_ata)?, false),
            ],
            data: [
                2u32.to_le_bytes().as_slice(),
                input_atoms.to_le_bytes().as_slice(),
            ]
            .concat(),
        });
        instructions.push(Instruction {
            program_id: key(TOKEN)?,
            accounts: vec![AccountMeta::new(key(&input_ata)?, false)],
            data: vec![17],
        });
    }
    instructions.push(ix);
    if operation == "jupiter.jlp.burn" && output_mint == SOL {
        instructions.push(Instruction {
            program_id: key(TOKEN)?,
            accounts: vec![
                AccountMeta::new(key(&output_ata)?, false),
                AccountMeta::new(wallet_key, false),
                AccountMeta::new_readonly(wallet_key, true),
            ],
            data: vec![9],
        });
    }
    let message = Message::new_with_blockhash(&instructions, Some(&wallet_key), &blockhash);
    let tx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(message),
    };
    let wire = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .serialize(&tx)
        .map_err(|_| fixture_error("Could not serialize synthetic JLP transaction."))?;
    Ok((STANDARD.encode(wire), input_ata, output_ata))
}

pub(super) async fn rpc_request(
    _rpc: &FixtureRpc,
    method: &str,
    params: &Value,
) -> Result<Option<Value>> {
    if method != "getAccountInfo" {
        return Ok(None);
    }
    let address = params
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .unwrap_or_default();
    if address != JLP_MINT {
        return Ok(None);
    }
    Ok(Some(
        json!({"context":{"slot":100},"value":{"owner":TOKEN,"data":[STANDARD.encode(mint_data(6)),"base64"],"lamports":RENT}}),
    ))
}

pub(super) async fn api_request(
    api: &FixtureJupiterApi,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Option<Value>> {
    if path == "/jlp-info" && method == Method::GET {
        return Ok(Some(
            json!({"cluster":"mainnet-beta","slot":100,"custodies":[{"symbol":"SOL","mint":SOL,"decimals":9},{"symbol":"USDC","mint":USDC,"decimals":6}] }),
        ));
    }
    if !matches!(path, "/jlp/mint" | "/jlp/burn") {
        return Ok(None);
    }
    if method != Method::POST {
        return Err(fixture_error(
            "Synthetic JLP fixture accepts only POST mint and burn.",
        ));
    }
    let body = body.ok_or_else(|| fixture_error("Synthetic JLP request is missing."))?;
    let minting = path == "/jlp/mint";
    let symbol = body
        .get(if minting {
            "depositToken"
        } else {
            "receiveToken"
        })
        .and_then(Value::as_str)
        .ok_or_else(|| fixture_error("Synthetic JLP token is missing."))?;
    let custody_mint = match symbol {
        "SOL" => SOL,
        "USDC" => USDC,
        _ => return Err(fixture_error("Synthetic JLP supports SOL and USDC only.")),
    };
    let (input_mint, output_mint) = if minting {
        (custody_mint, JLP_MINT)
    } else {
        (JLP_MINT, custody_mint)
    };
    let amount = if minting {
        body.get("depositTokenAmount")
            .and_then(Value::as_str)
            .ok_or_else(|| fixture_error("Synthetic JLP amount is missing."))?
    } else {
        body.get("burnTokenAmount")
            .and_then(Value::as_str)
            .ok_or_else(|| fixture_error("Synthetic JLP amount is missing."))?
    };
    let input_decimals = if input_mint == SOL { 9 } else { 6 };
    // Mint amounts arrive as decimal UI quantities. Burn amounts are already
    // exact JLP atoms, as required by the provider's burnTokenAmount contract.
    let input_atoms = if minting {
        gofin_domain::Quantity::from_decimal(amount, input_decimals)
            .map_err(|_| fixture_error("Synthetic JLP amount is malformed."))?
            .as_str()
            .parse::<u64>()
            .map_err(|_| fixture_error("Synthetic JLP amount is too large."))?
    } else {
        amount
            .parse::<u64>()
            .map_err(|_| fixture_error("Synthetic JLP amount is malformed."))?
    };
    let output_decimals = if output_mint == SOL { 9 } else { 6 };
    let input_price = if input_mint == SOL { 100u128 } else { 1 };
    let output_price = if output_mint == SOL { 100u128 } else { 1 };
    let output_atoms = u64::try_from(
        u128::from(input_atoms)
            .checked_mul(input_price)
            .and_then(|value| value.checked_mul(10u128.pow(u32::from(output_decimals))))
            .ok_or_else(|| fixture_error("Synthetic JLP output overflow."))?
            / (10u128.pow(u32::from(input_decimals)) * output_price),
    )
    .map_err(|_| fixture_error("Synthetic JLP output is too large."))?;
    let output_amount = gofin_domain::Quantity::from_u64(output_atoms).to_decimal(output_decimals);
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let (encoded, _, _) = instruction(
        &api.wallet,
        if minting {
            "jupiter.jlp.mint"
        } else {
            "jupiter.jlp.burn"
        },
        input_mint,
        output_mint,
        input_atoms,
        output_atoms,
        blockhash,
    )?;
    let mut response = json!({"transactionType":"legacy","serializedTxBase64":encoded,"txMetadata":{"lastValidBlockHeight":"1000"}});
    response[if minting {
        "mintTokenAmount"
    } else {
        "receiveTokenAmount"
    }] = json!(output_amount);
    Ok(Some(response))
}

pub(super) fn submit(rpc: &FixtureRpc, encoded: &str) -> Result<Option<String>> {
    if encoded.len() > 256 * 1024 {
        return Ok(None);
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| fixture_error("Synthetic transaction encoding is invalid."))?;
    let tx: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| fixture_error("Synthetic transaction is invalid."))?;
    tx.sanitize()
        .map_err(|_| fixture_error("Synthetic transaction is invalid."))?;
    let keys = tx
        .message
        .static_account_keys()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let Some(ix) = tx.message.instructions().iter().find(|ix| {
        keys.get(usize::from(ix.program_id_index))
            .is_some_and(|key| key == PROGRAM)
    }) else {
        return Ok(None);
    };
    // JLP and Perps share the program ID. Only claim the two reviewed JLP
    // instructions; other Perpetuals instructions belong to their own fixture.
    if !ix.data.starts_with(&anchor("add_liquidity2"))
        && !ix.data.starts_with(&anchor("remove_liquidity2"))
    {
        return Ok(None);
    }
    let account = |index: usize| -> Result<String> {
        let key_index = usize::from(
            *ix.accounts
                .get(index)
                .ok_or_else(|| fixture_error("Synthetic JLP accounts are truncated."))?,
        );
        keys.get(key_index)
            .cloned()
            .ok_or_else(|| fixture_error("Synthetic JLP account index is invalid."))
    };
    let minting = ix.data.starts_with(&anchor("add_liquidity2"));
    let expected_data = if minting {
        "add_liquidity2"
    } else {
        "remove_liquidity2"
    };
    if !ix.data.starts_with(&anchor(expected_data)) || ix.data.len() < 24 {
        return Err(fixture_error("Synthetic JLP instruction is invalid."));
    }
    let amount = u64::from_le_bytes(
        ix.data[8..16]
            .try_into()
            .map_err(|_| fixture_error("Synthetic JLP args invalid."))?,
    );
    let output = u64::from_le_bytes(
        ix.data[16..24]
            .try_into()
            .map_err(|_| fixture_error("Synthetic JLP args invalid."))?,
    );
    let (source, destination) = if minting {
        (account(1)?, account(2)?)
    } else {
        (account(2)?, account(1)?)
    };
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| fixture_error("Synthetic JLP signature is missing."))?;
    let mut state = rpc
        .0
        .lock()
        .map_err(|_| fixture_error("Synthetic JLP state is unavailable."))?;
    let source_before = *state
        .token_amounts
        .get(&source)
        .ok_or_else(|| fixture_error("Synthetic JLP source account is absent."))?;
    let destination_before = *state
        .token_amounts
        .get(&destination)
        .ok_or_else(|| fixture_error("Synthetic JLP destination account is absent."))?;
    let source_mint = {
        let data = state
            .account_data
            .get(&source)
            .ok_or_else(|| fixture_error("Synthetic JLP source account is absent."))?;
        bs58::encode(
            data.1
                .get(..32)
                .ok_or_else(|| fixture_error("Synthetic JLP source account is malformed."))?,
        )
        .into_string()
    };
    let destination_mint = {
        let data = state
            .account_data
            .get(&destination)
            .ok_or_else(|| fixture_error("Synthetic JLP destination account is absent."))?;
        bs58::encode(
            data.1
                .get(..32)
                .ok_or_else(|| fixture_error("Synthetic JLP destination account is malformed."))?,
        )
        .into_string()
    };
    let native_input = source_mint == SOL
        && tx.message.instructions().iter().any(|ix| {
            keys.get(usize::from(ix.program_id_index))
                .is_some_and(|program| program == SYSTEM)
                && ix.data.len() == 12
                && u32::from_le_bytes(ix.data[..4].try_into().unwrap_or_default()) == 2
        });
    let native_output =
        destination_mint == SOL && tx.message.instructions().iter().any(|ix| ix.data == [9]);
    let source_after = if native_input {
        source_before
    } else {
        source_before
            .checked_sub(amount)
            .ok_or_else(|| fixture_error("Synthetic JLP balance is insufficient."))?
    };
    let destination_after = destination_before
        .checked_add(output)
        .ok_or_else(|| fixture_error("Synthetic JLP output overflow."))?;
    let wallet_index = keys
        .iter()
        .position(|key| key == &state.wallet)
        .ok_or_else(|| fixture_error("Synthetic JLP transaction omitted wallet."))?;
    let source_index = keys
        .iter()
        .position(|key| key == &source)
        .ok_or_else(|| fixture_error("Synthetic JLP transaction omitted source."))?;
    let destination_index = keys
        .iter()
        .position(|key| key == &destination)
        .ok_or_else(|| fixture_error("Synthetic JLP transaction omitted destination."))?;
    let mint_for = |address: &str| -> Result<String> {
        let bytes = state
            .account_data
            .get(address)
            .ok_or_else(|| fixture_error("Synthetic JLP token data missing."))?;
        Ok(bs58::encode(
            bytes
                .1
                .get(..32)
                .ok_or_else(|| fixture_error("Synthetic JLP token data malformed."))?,
        )
        .into_string())
    };
    let source_mint = mint_for(&source)?;
    let destination_mint = mint_for(&destination)?;
    let source_program = state
        .account_data
        .get(&source)
        .map(|x| x.0.as_str())
        .unwrap_or(TOKEN)
        .to_owned();
    let destination_program = state
        .account_data
        .get(&destination)
        .map(|x| x.0.as_str())
        .unwrap_or(TOKEN)
        .to_owned();
    let wallet_before = state.lamports;
    let native_debit = if native_input { amount } else { 0 };
    let native_credit = if native_output { output } else { 0 };
    let rent_refund = if native_output { RENT } else { 0 };
    let wallet_after = wallet_before
        .checked_sub(FEE)
        .and_then(|value| value.checked_sub(native_debit))
        .and_then(|value| value.checked_add(native_credit))
        .and_then(|value| value.checked_add(rent_refund))
        .ok_or_else(|| fixture_error("Synthetic JLP lamport balance overflow."))?;
    let mut pre = vec![0u64; keys.len()];
    let mut post = vec![0u64; keys.len()];
    pre[wallet_index] = wallet_before;
    post[wallet_index] = wallet_after;
    if source_mint == SOL || destination_mint == SOL {
        let native_index = if source_mint == SOL {
            source_index
        } else {
            destination_index
        };
        pre[native_index] = RENT;
        post[native_index] = if native_output { 0 } else { RENT };
    }
    let pre_tokens = vec![
        token_balance(
            source_index,
            &source_mint,
            &state.wallet,
            &source_program,
            if source_mint == SOL { 9 } else { 6 },
            source_before,
        ),
        token_balance(
            destination_index,
            &destination_mint,
            &state.wallet,
            &destination_program,
            if destination_mint == SOL { 9 } else { 6 },
            destination_before,
        ),
    ];
    let post_tokens = vec![
        token_balance(
            source_index,
            &source_mint,
            &state.wallet,
            &source_program,
            if source_mint == SOL { 9 } else { 6 },
            source_after,
        ),
        token_balance(
            destination_index,
            &destination_mint,
            &state.wallet,
            &destination_program,
            if destination_mint == SOL { 9 } else { 6 },
            destination_after,
        ),
    ];
    state.lamports = wallet_after;
    state.token_amounts.insert(source.clone(), source_after);
    state
        .token_amounts
        .insert(destination.clone(), destination_after);
    state.transactions.push(StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.to_owned(),
        pre_balances: pre,
        post_balances: post,
        pre_token_balances: pre_tokens,
        post_token_balances: post_tokens,
        logs: vec![
            format!("Program {PROGRAM} invoke [1]"),
            format!("Program {PROGRAM} success"),
        ],
    });
    rpc.persist_state(&state)?;
    if native_output {
        state.account_data.remove(&destination);
        state.token_amounts.remove(&destination);
    }
    rpc.record_submission(&mut state, signature.clone())?;
    Ok(Some(signature))
}
