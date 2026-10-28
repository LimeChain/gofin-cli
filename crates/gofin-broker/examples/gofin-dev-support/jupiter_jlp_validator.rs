//! Synthetic JLP provider responses for an owned local validator.
//! Transactions use the actual validator blockhash and are submitted only by GoFin.
use async_trait::async_trait;
use base64::Engine;
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
use std::{sync::Arc, time::Duration};

pub struct LocalJlpApi {
    rpc: Arc<dyn gofin_solana::SolanaRpc>,
    wallet: String,
}
impl LocalJlpApi {
    pub fn new(rpc: Arc<dyn gofin_solana::SolanaRpc>, wallet: String) -> Self {
        Self { rpc, wallet }
    }
}

const PROGRAM: &str = gofin_jupiter::jlp::PERPS_PROGRAM_ID;
const JLP_MINT: &str = gofin_jupiter::jlp::JLP_MINT;
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOL: &str = gofin_jupiter::swap::WRAPPED_SOL_MINT;
const TOKEN: &str = gofin_solana::token::TOKEN_PROGRAM_ID;
const SYSTEM: &str = "11111111111111111111111111111111";

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

fn anchor(name: &str) -> [u8; 8] {
    discr("global", name)
}

// Fixture transaction builder mirrors the pinned protocol account graph.
#[allow(clippy::too_many_arguments)]
fn instruction(
    wallet: &str,
    operation: &str,
    input_mint: &str,
    output_mint: &str,
    input_atoms: u64,
    output_atoms: u64,
    blockhash: solana_hash::Hash,
    custody_token: &str,
    oracle: [&str; 2],
    remaining: &[String],
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
    let mut ix = Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new(wallet_key, true),
            AccountMeta::new(account_one, false),
            AccountMeta::new(account_two, false),
            AccountMeta::new_readonly(key(&transfer_authority)?, false),
            AccountMeta::new_readonly(key(&perpetuals)?, false),
            AccountMeta::new(pool_key, false),
            AccountMeta::new(key(&custody)?, false),
            AccountMeta::new_readonly(key(oracle[0])?, false),
            AccountMeta::new_readonly(key(oracle[1])?, false),
            AccountMeta::new(key(custody_token)?, false),
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
    for address in remaining {
        ix.accounts
            .push(AccountMeta::new_readonly(key(address)?, false));
    }
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

async fn api_request(
    api: &LocalJlpApi,
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
    let output_atoms = if minting {
        output_atoms / 10
    } else {
        output_atoms
    };
    let output_amount = gofin_domain::Quantity::from_u64(output_atoms).to_decimal(output_decimals);
    let latest = api
        .rpc
        .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
        .await?;
    let blockhash = latest
        .pointer("/value/blockhash")
        .and_then(Value::as_str)
        .ok_or_else(|| fixture_error("Local blockhash is missing."))?
        .parse()
        .map_err(|_| fixture_error("Local blockhash is invalid."))?;
    let last_valid = latest
        .pointer("/value/lastValidBlockHeight")
        .and_then(Value::as_u64)
        .ok_or_else(|| fixture_error("Local block height is missing."))?;
    let custody = custody_address(custody_mint)?;
    let account = api
        .rpc
        .request(
            "getAccountInfo",
            json!([custody, {"encoding":"base64", "commitment":"finalized"}]),
        )
        .await?;
    let encoded = account
        .pointer("/value/data/0")
        .and_then(Value::as_str)
        .ok_or_else(|| fixture_error("Local custody account is unavailable."))?;
    let data = STANDARD
        .decode(encoded)
        .map_err(|_| fixture_error("Local custody encoding is invalid."))?;
    let token_bytes: [u8; 32] = data
        .get(72..104)
        .ok_or_else(|| fixture_error("Local custody account is truncated."))?
        .try_into()
        .map_err(|_| fixture_error("Local custody token account is invalid."))?;
    let custody_token = Pubkey::new_from_array(token_bytes).to_string();
    let doves = Pubkey::new_from_array(
        data[384..416]
            .try_into()
            .map_err(|_| fixture_error("Invalid doves oracle."))?,
    )
    .to_string();
    let pyth = Pubkey::new_from_array(
        data[106..138]
            .try_into()
            .map_err(|_| fixture_error("Invalid pyth oracle."))?,
    )
    .to_string();
    let pool = api
        .rpc
        .request(
            "getAccountInfo",
            json!([pool_address()?, {"encoding":"base64", "commitment":"finalized"}]),
        )
        .await?;
    let pool_data = STANDARD
        .decode(
            pool.pointer("/value/data/0")
                .and_then(Value::as_str)
                .ok_or_else(|| fixture_error("Pool is missing."))?,
        )
        .map_err(|_| fixture_error("Pool encoding is invalid."))?;
    let name_len = u32::from_le_bytes(pool_data[8..12].try_into().unwrap()) as usize;
    let offset = 12 + name_len;
    let count = u32::from_le_bytes(pool_data[offset..offset + 4].try_into().unwrap()) as usize;
    if count == 0 || count > 32 || pool_data.len() < offset + 4 + count * 32 {
        return Err(fixture_error("Pool custody vector is invalid."));
    }
    let mut remaining = Vec::new();
    let mut oracles = Vec::new();
    for bytes in pool_data[offset + 4..offset + 4 + count * 32].chunks_exact(32) {
        let address = Pubkey::new_from_array(bytes.try_into().unwrap()).to_string();
        let row = api
            .rpc
            .request(
                "getAccountInfo",
                json!([address,{"encoding":"base64","commitment":"finalized"}]),
            )
            .await?;
        let raw = STANDARD
            .decode(
                row.pointer("/value/data/0")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fixture_error("Custody is missing."))?,
            )
            .map_err(|_| fixture_error("Custody encoding is invalid."))?;
        let oracle: [u8; 32] = raw
            .get(384..416)
            .ok_or_else(|| fixture_error("Custody is truncated."))?
            .try_into()
            .unwrap();
        remaining.push(address);
        oracles.push(Pubkey::new_from_array(oracle).to_string());
    }
    remaining.extend(oracles);
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
        &custody_token,
        [&doves, &pyth],
        &remaining,
    )?;
    let mut response = json!({"transactionType":"legacy","serializedTxBase64":encoded,"txMetadata":{"lastValidBlockHeight":last_valid.to_string()}});
    response[if minting {
        "mintTokenAmount"
    } else {
        "receiveTokenAmount"
    }] = json!(output_amount);
    Ok(Some(response))
}

#[async_trait]
impl gofin_jupiter::JupiterApi for LocalJlpApi {
    async fn request(
        &self,
        _host: gofin_jupiter::api::JupiterHost,
        method: Method,
        path: &str,
        body: Option<Value>,
        _bucket: gofin_jupiter::api::RateBucket,
        _priority: gofin_jupiter::api::RequestPriority,
        _max_wait: Duration,
    ) -> Result<Value> {
        api_request(self, method, path, body)
            .await?
            .ok_or_else(|| fixture_error("Unexpected local JLP provider request."))
    }
}
